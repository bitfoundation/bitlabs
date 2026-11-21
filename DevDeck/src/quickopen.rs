//! Quick Open: "Go to File..." (Ctrl+P) and "Go to Line/Column..." (Ctrl+G), like VS Code.
//!
//! - Files are matched fuzzily (see `fuzzy.rs`) against their name first, then their path,
//!   and the matched characters are highlighted. Several words must all match.
//! - Recently opened files come first ("recently opened"), the most recent at the top; the
//!   list is kept per folder in `%APPDATA%\DevDeck\history`.
//! - `name:line[:column]` opens the file at that line, and `:line[:column]` goes to a line in
//!   the active file. `>` switches to the Command Palette.
//! - The file list comes from `git ls-files` (tracked and untracked, not ignored) on a
//!   background thread. The last list is kept, so the picker opens at once with it and is
//!   updated when the new list arrives.
//! - Ctrl+P again while it is open selects the next file; releasing Ctrl then opens it, so
//!   Ctrl+P, P switches to the previous file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::app::*;
use crate::fuzzy;
use crate::git::{self, Group, Kind};
use crate::win;

/// Recently opened files kept per folder.
const HISTORY_MAX: usize = 50;
/// A query word found in the file name ranks the file above path-only matches.
const NAME_BONUS: i32 = 10_000;
/// The query is the whole name ("app.rs"), or the name without its extension ("app").
const EXACT_BONUS: i32 = 64;
const STEM_BONUS: i32 = 32;
/// Files of a folder without a repository are listed up to this many.
const MAX_LISTED: usize = 50_000;
const FILE_ICON: char = '\u{E8A5}';

/// What the open quick pick is.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum PickMode {
    #[default]
    Other,
    Palette,
    Files,
}

#[derive(Default)]
pub struct QuickOpen {
    /// The folder's files from the last listing ('/' separated, sorted) and the folder.
    pub files: Vec<String>,
    pub files_root: Option<PathBuf>,
    /// A listing is running; `gen` tells its result from older ones.
    pub loading: bool,
    gen: u64,
    /// Recently opened files of `history_root`, most recent first.
    history: Vec<String>,
    history_root: Option<PathBuf>,
    history_dirty: bool,
    // ---- the open picker
    /// Folded query words (also used to highlight matches in the other pickers).
    pub words: Vec<Vec<char>>,
    /// Line (and column) typed after the file name ("app.rs:120:5").
    pub line: Option<(usize, Option<usize>)>,
    /// Position of each item in `history` (u32::MAX: not recently opened).
    hist_rank: Vec<u32>,
    /// Rows that start a group, with the group's name.
    pub groups: Vec<(usize, &'static str)>,
    /// Items matching `last_query`: a longer query only searches these.
    matched: Vec<usize>,
    last_query: String,
    /// Ctrl+P was pressed again in the open picker: releasing Ctrl opens the selection.
    pub nav: bool,
    /// Git status of changed files, to color their names.
    pub deco: HashMap<String, Kind>,
    /// A line to go to once the file loading in the tab with this gen is shown.
    pending: Option<(u64, usize, Option<usize>)>,
}

/// Splits "name:line", "name:line:column" and "name:line,column" into the name and the
/// position; a trailing ':' alone ("name:") is dropped.
pub fn split_line(q: &str) -> (&str, Option<(usize, Option<usize>)>) {
    let num = |s: &str| if !s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit()) { s.parse::<usize>().ok() } else { None };
    if let Some((head, last)) = q.rsplit_once(':') {
        if last.is_empty() {
            return (head, None);
        }
        if let Some(n) = num(last) {
            if let Some((head2, l)) = head.rsplit_once(':') {
                if let Some(l) = num(l) {
                    return (head2, Some((l, Some(n))));
                }
            }
            return (head, Some((n, None)));
        }
        if let Some((l, c)) = last.split_once(',') {
            if let (Some(l), Some(c)) = (num(l), num(c)) {
                return (head, Some((l, Some(c))));
            }
        }
    }
    (q, None)
}

/// Score of a file for the query words (higher is better), None when a word doesn't match.
/// A word is looked for in the name first, then in the whole path; a word with '/' only in
/// the path.
pub fn rank(words: &[Vec<char>], name: &str, path: &str) -> Option<i32> {
    let mut total = 0;
    for w in words {
        let in_name = if w.contains(&'/') {
            None
        } else {
            fuzzy::score(w, name, None).map(|s| {
                let is = |t: &str| t.chars().map(fuzzy::fold).eq(w.iter().copied());
                let stem = name.rfind('.').filter(|&i| i > 0).map(|i| &name[..i]);
                s + NAME_BONUS + if is(name) { EXACT_BONUS } else if stem.is_some_and(is) { STEM_BONUS } else { 0 }
            })
        };
        total += match in_name {
            Some(s) => s,
            None => fuzzy::score(w, path, None)?,
        };
    }
    Some(total)
}

/// Matched char positions in a file's name (label) and folder (description), found the way
/// `rank` matched them.
pub fn highlights(words: &[Vec<char>], name: &str, dir: &str, path: &str) -> (Vec<usize>, Vec<usize>) {
    let (mut label, mut desc) = (Vec::new(), Vec::new());
    let name_at = if dir.is_empty() { 0 } else { dir.chars().count() + 1 };
    for w in words {
        if !w.contains(&'/') && fuzzy::score(w, name, Some(&mut label)).is_some() {
            continue;
        }
        let mut p = Vec::new();
        if fuzzy::score(w, path, Some(&mut p)).is_some() {
            for i in p {
                if i >= name_at {
                    label.push(i - name_at);
                } else if i + 1 < name_at {
                    desc.push(i);
                }
            }
        }
    }
    label.sort_unstable();
    label.dedup();
    desc.sort_unstable();
    desc.dedup();
    (label, desc)
}

/// Files for Quick Open: tracked (without the ones deleted from the working tree and
/// submodules) and untracked ones that aren't ignored; in a folder without a repository,
/// every file except in hidden folders and node_modules.
pub fn list_files(root: &Path, repo: bool) -> Vec<String> {
    let mut out: Vec<String> = if repo {
        let (staged, others, deleted) = std::thread::scope(|sc| {
            let o = sc.spawn(|| git::run(root, &["ls-files", "-z", "--others", "--exclude-standard"], None, true).text());
            let d = sc.spawn(|| git::run(root, &["ls-files", "-z", "--deleted"], None, true).text());
            let s = git::run(root, &["ls-files", "-z", "--stage"], None, true).text();
            (s, o.join().unwrap_or_default(), d.join().unwrap_or_default())
        });
        let deleted: std::collections::HashSet<&str> = deleted.split('\0').filter(|s| !s.is_empty()).collect();
        // "<mode> <object> <stage>\t<path>"; mode 160000 is a submodule
        let tracked = staged.split('\0').filter_map(|e| e.split_once('\t')).filter(|(meta, p)| !meta.starts_with("160000") && !deleted.contains(p)).map(|(_, p)| p);
        // a nested repository is listed as "dir/"
        let untracked = others.split('\0').filter(|p| !p.is_empty() && !p.ends_with('/'));
        tracked.chain(untracked).map(String::from).collect()
    } else {
        walk(root)
    };
    out.sort_by_cached_key(|p| p.to_lowercase());
    out.dedup();
    out
}

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(root.join(&rel)) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    if !name.starts_with('.') && name != "node_modules" {
                        stack.push(path);
                    }
                }
                Ok(t) if t.is_file() => {
                    out.push(path);
                    if out.len() >= MAX_LISTED {
                        return out;
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn file_item(path: &str) -> PickItem {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    pi(name, dir, FILE_ICON, path)
}

/// %APPDATA%\DevDeck\history\<hash of the folder>.txt
fn history_path(root: &Path) -> PathBuf {
    // FNV-1a: stable across builds, unlike std's hasher
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root.display().to_string().to_lowercase().bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    let base = PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into()));
    base.join("DevDeck").join("history").join(format!("{h:016x}.txt"))
}

/// The file starts with the folder it belongs to, then one path per line.
fn read_history(root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(history_path(root)) else { return Vec::new() };
    let mut lines = text.lines();
    if !lines.next().is_some_and(|l| l.eq_ignore_ascii_case(&root.display().to_string())) {
        return Vec::new();
    }
    lines.filter(|l| !l.is_empty()).take(HISTORY_MAX).map(String::from).collect()
}

/// The relative path of the file an editor tab key shows ("file:<path>", "change:<group>:<path>").
fn tab_file(key: &str) -> Option<&str> {
    key.strip_prefix("file:").or_else(|| key.strip_prefix("change:").and_then(|k| k.split_once(':')).map(|(_, p)| p))
}

impl App {
    /// "Go to File..." (Ctrl+P).
    pub fn goto_file(&mut self) {
        self.quick_open("");
    }

    /// "Go to Line/Column..." (Ctrl+G): Quick Open with ':' typed.
    pub fn goto_line_pick(&mut self) {
        self.quick_open(":");
    }

    fn quick_open(&mut self, text: &str) {
        let Some(root) = self.folder() else { return };
        if self.qo.files_root.as_ref() != Some(&root) {
            self.qo.files = Vec::new();
            self.qo.files_root = Some(root.clone());
        }
        self.load_history();
        let mut deco = HashMap::new();
        if let Some(s) = self.status() {
            // the working tree state wins over the staged one, a conflict over both
            for g in [Group::Staged, Group::Changes, Group::Merge] {
                for f in s.files.iter().filter(|f| f.group == g) {
                    deco.insert(f.path.clone(), f.kind);
                }
            }
        }
        let items = self.file_items();
        self.open_pick("Search files by name (append : to go to line)", items, PickMode::Files, |app, it| app.open_quick_item(it));
        self.reset_file_pick();
        self.qo.deco = deco;
        if !text.is_empty() {
            win::set_text(self.overlay_edit, text);
            let n = text.encode_utf16().count();
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(self.overlay_edit, 0x00B1 /* EM_SETSEL */, n, n as isize);
            }
        }
        self.filter_pick();
        // list the files again: they may have changed since the last time
        self.qo.gen += 1;
        self.qo.loading = true;
        let (gen, tx, hwnd, repo) = (self.qo.gen, self.tx.clone(), self.hwnd as isize, self.repo.is_some());
        std::thread::spawn(move || {
            let files = list_files(&root, repo);
            let _ = tx.send(JobResult::Files { gen, root, files });
            post(hwnd);
        });
    }

    /// The picker's items: the "go to line" row first (shown only for ":"), then the files.
    /// Before the first listing arrives, the recently opened files that still exist.
    fn file_items(&self) -> Vec<PickItem> {
        let mut items = vec![pi("", "", '\u{E8FD}', "")];
        if self.qo.files.is_empty() {
            if let Some(root) = &self.qo.files_root {
                items.extend(self.qo.history.iter().filter(|p| root.join(p).is_file()).map(|p| file_item(p)));
            }
        } else {
            items.extend(self.qo.files.iter().map(|p| file_item(p)));
        }
        items
    }

    fn reset_file_pick(&mut self) {
        let ranks: HashMap<&str, u32> = self.qo.history.iter().enumerate().map(|(i, p)| (p.as_str(), i as u32)).collect();
        self.qo.hist_rank = match &self.overlay {
            Some(Overlay { kind: OverlayKind::Pick { items, .. } }) => items.iter().map(|it| ranks.get(it.data.as_str()).copied().unwrap_or(u32::MAX)).collect(),
            _ => Vec::new(),
        };
        self.qo.matched = Vec::new();
        self.qo.last_query = String::new();
        self.qo.line = None;
        self.qo.nav = false;
        self.qo.groups = Vec::new();
    }

    /// A new file listing: the open picker shows it, keeping the query and the selected file.
    pub fn on_files_listed(&mut self, gen: u64, root: PathBuf, files: Vec<String>) {
        if gen != self.qo.gen {
            return;
        }
        self.qo.loading = false;
        if self.qo.files_root.as_ref() != Some(&root) {
            return;
        }
        let shown = !self.qo.files.is_empty();
        if shown && files == self.qo.files {
            if self.pick_mode == PickMode::Files {
                self.invalidate();
            }
            return;
        }
        self.qo.files = files;
        if self.pick_mode != PickMode::Files {
            return;
        }
        let selected = match &self.overlay {
            Some(Overlay { kind: OverlayKind::Pick { items, filtered, sel, .. } }) if *sel > 0 => filtered.get(*sel).map(|&i| items[i].data.clone()),
            _ => None,
        };
        let new_items = self.file_items();
        if let Some(Overlay { kind: OverlayKind::Pick { items, .. } }) = &mut self.overlay {
            *items = new_items;
        }
        let nav = self.qo.nav;
        self.reset_file_pick();
        self.filter_pick();
        self.qo.nav = nav;
        if let (Some(path), Some(Overlay { kind: OverlayKind::Pick { items, filtered, sel, .. } })) = (selected, &mut self.overlay) {
            if let Some(k) = filtered.iter().position(|&i| items[i].data == path) {
                *sel = k;
                self.reveal_row(Area::Pick, k);
            }
        }
        self.invalidate();
    }

    /// Filters and orders the file picker for query `raw`.
    pub fn filter_files(&mut self, raw: &str) {
        self.qo.nav = false;
        let q = raw.trim();
        // ">" switches to the Command Palette, keeping what follows
        if let Some(rest) = q.strip_prefix('>') {
            let rest = rest.trim_start().to_string();
            self.palette();
            if !rest.is_empty() {
                win::set_text(self.overlay_edit, &rest);
                let n = rest.encode_utf16().count();
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(self.overlay_edit, 0x00B1, n, n as isize);
                }
            }
            return;
        }
        let (file_q, line) = split_line(q);
        self.qo.line = line;
        // ":12" goes to a line of the active file
        if file_q.trim().is_empty() && q.starts_with(':') {
            let label = self.goto_line_label(line);
            self.qo.words = Vec::new();
            self.qo.groups = Vec::new();
            if let Some(Overlay { kind: OverlayKind::Pick { items, filtered, sel, .. } }) = &mut self.overlay {
                items[0].label = label;
                *filtered = vec![0];
                *sel = 0;
            }
            return;
        }
        let words: Vec<Vec<char>> = file_q.split_whitespace().map(fuzzy::pattern).collect();
        let key: String = words.iter().map(|w| w.iter().collect::<String>()).collect::<Vec<_>>().join(" ");
        let Some(Overlay { kind: OverlayKind::Pick { items, filtered, sel, .. } }) = &mut self.overlay else { return };
        let ranks = &self.qo.hist_rank;
        let hist = |i: usize| ranks.get(i).copied().unwrap_or(u32::MAX);
        let (mut recent, mut other): (Vec<(i32, usize)>, Vec<(i32, usize)>);
        if words.is_empty() {
            // nothing typed: the recently opened files, then all files in path order
            (recent, other) = (1..items.len()).map(|i| (0, i)).partition(|&(_, i)| hist(i) != u32::MAX);
            recent.sort_by_key(|&(_, i)| hist(i));
            self.qo.matched = Vec::new();
            self.qo.last_query = String::new();
        } else {
            // typing more only narrows the previous matches
            let narrow = !self.qo.last_query.is_empty() && key.starts_with(&self.qo.last_query);
            let candidates: Vec<usize> = if narrow { std::mem::take(&mut self.qo.matched) } else { (1..items.len()).collect() };
            let scored: Vec<(i32, usize)> = candidates.into_iter().filter_map(|i| rank(&words, &items[i].label, &items[i].data).map(|s| (s, i))).collect();
            self.qo.matched = scored.iter().map(|&(_, i)| i).collect();
            self.qo.last_query = key;
            (recent, other) = scored.into_iter().partition(|&(_, i)| hist(i) != u32::MAX);
            recent.sort_by_key(|&(s, i)| (std::cmp::Reverse(s), hist(i)));
            // best score, then the shorter name and path
            other.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| items[a.1].label.len().cmp(&items[b.1].label.len())).then_with(|| items[a.1].data.len().cmp(&items[b.1].data.len())).then(a.1.cmp(&b.1)));
        }
        self.qo.groups = Vec::new();
        if !recent.is_empty() {
            self.qo.groups.push((0, "recently opened"));
            if !other.is_empty() {
                self.qo.groups.push((recent.len(), "file results"));
            }
        }
        *filtered = recent.into_iter().chain(other).map(|(_, i)| i).collect();
        *sel = 0;
        self.qo.words = words;
    }

    fn goto_line_label(&self, line: Option<(usize, Option<usize>)>) -> String {
        let MainView::File(f) = &self.view else { return "Open a text editor first to go to a line.".into() };
        let n = f.lines.len();
        match line {
            Some((l, None)) => format!("Go to line {}.", l.clamp(1, n.max(1))),
            Some((l, Some(c))) => format!("Go to line {}, character {c}.", l.clamp(1, n.max(1))),
            None if f.ed.editable => format!("Current Line: {}, Character: {}. Type a line number between 1 and {n} to navigate to.", f.ed.cur.0 + 1, f.ed.cur.1 + 1),
            None => format!("Type a line number between 1 and {n} to navigate to."),
        }
    }

    /// The picked file opens in a kept-open tab with the keyboard in it, at the typed line.
    fn open_quick_item(&mut self, it: &PickItem) {
        let line = self.qo.line;
        if it.data.is_empty() {
            if let (Some((l, c)), MainView::File(_)) = (line, &self.view) {
                self.goto_line(l, c);
            }
            return;
        }
        let rel = it.data.clone();
        self.open_file_viewer(rel.clone(), true);
        self.focus = Focus::Main;
        self.sync_term_focus();
        // like VS Code's explorer.autoReveal
        if self.explorer_active() {
            self.explorer_reveal(&rel);
        }
        if let Some((l, c)) = line {
            match self.view {
                MainView::File(_) => self.goto_line(l, c),
                MainView::Loading(_) => self.qo.pending = self.tabs.get(self.tab_active).map(|t| (t.gen, l, c)),
                _ => {}
            }
        }
    }

    /// Moves the caret of the active file view to `line` (1-based) and `col` (1-based, default
    /// the line's start); the line is centered when it isn't in view.
    pub fn goto_line(&mut self, line: usize, col: Option<usize>) {
        let lh = self.line_h();
        let view_h = crate::win::rh(&self.l.main_body);
        let MainView::File(f) = &mut self.view else { return };
        if f.lines.is_empty() {
            return;
        }
        let l = line.clamp(1, f.lines.len()) - 1;
        let editable = f.ed.editable;
        if editable {
            let c = (col.unwrap_or(1).max(1) - 1).min(crate::editor::char_len(&f.lines[l]));
            f.move_to((l, c), false);
            f.ed.want_vcol = None;
        }
        let area = Area::Main as usize;
        let top = l as i32 * lh;
        let s = self.scroll[area];
        if top < s || top + lh > s + view_h {
            self.scroll[area] = (top - (view_h - lh) / 2).max(0);
            self.clamp_scroll(Area::Main);
        }
        if editable {
            self.reveal_caret();
        }
        self.focus = Focus::Main;
        self.invalidate();
    }

    /// A view finished loading into the tab with `gen`: the line typed in Quick Open for it.
    pub fn take_pending_goto(&mut self, gen: u64) -> Option<(usize, Option<usize>)> {
        match self.qo.pending {
            Some((g, l, c)) if g == gen => {
                self.qo.pending = None;
                Some((l, c))
            }
            _ => None,
        }
    }

    /// Ctrl+P / Ctrl+Shift+P pressed in an open picker: the same picker again selects the
    /// next item (and in Go to File, releasing Ctrl opens it), the other one switches.
    pub fn pick_shortcut(&mut self, palette: bool) {
        match (palette, self.pick_mode) {
            (false, PickMode::Files) => {
                self.overlay_key(0x28);
                self.qo.nav = true;
            }
            (true, PickMode::Palette) => {
                self.overlay_key(0x28);
            }
            (false, _) => self.goto_file(),
            (true, _) => self.palette(),
        }
    }

    /// Ctrl was released in an open picker: opens the item Ctrl+P moved to.
    pub fn pick_ctrl_up(&mut self) -> bool {
        if self.qo.nav && self.pick_mode == PickMode::Files {
            self.qo.nav = false;
            self.accept_overlay(None);
            return true;
        }
        false
    }

    /// The picker closed: drops its per-query state (the typed line stays for the pick's
    /// callback).
    pub fn end_pick(&mut self) {
        self.pick_mode = PickMode::Other;
        self.pick_hay = Vec::new();
        self.qo.words = Vec::new();
        self.qo.groups = Vec::new();
        self.qo.hist_rank = Vec::new();
        self.qo.matched = Vec::new();
        self.qo.last_query = String::new();
        self.qo.deco = HashMap::new();
        self.qo.nav = false;
    }

    // ---------------------------------------------------------------- history

    fn load_history(&mut self) {
        let Some(root) = self.folder() else { return };
        if self.qo.history_root.as_ref() == Some(&root) {
            return;
        }
        self.save_history();
        self.qo.history = read_history(&root);
        self.qo.history_root = Some(root);
    }

    /// Puts the file of the active tab at the top of the recently opened files.
    pub fn note_history(&mut self) {
        let Some(rel) = self.tabs.get(self.tab_active).and_then(|t| tab_file(&t.key)).map(String::from) else { return };
        self.load_history();
        let h = &mut self.qo.history;
        if h.first() == Some(&rel) {
            return;
        }
        h.retain(|p| *p != rel);
        h.insert(0, rel);
        h.truncate(HISTORY_MAX);
        self.qo.history_dirty = true;
    }

    /// Writes the recently opened files; ones another window of the folder added since are
    /// kept after this window's.
    pub fn save_history(&mut self) {
        if !self.qo.history_dirty {
            return;
        }
        self.qo.history_dirty = false;
        let Some(root) = self.qo.history_root.clone() else { return };
        let mut list = self.qo.history.clone();
        for p in read_history(&root) {
            if !list.contains(&p) {
                list.push(p);
            }
        }
        list.truncate(HISTORY_MAX);
        let path = history_path(&root);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut text = root.display().to_string();
        for p in &list {
            text.push('\n');
            text.push_str(p);
        }
        text.push('\n');
        let _ = std::fs::write(path, text);
    }
}
