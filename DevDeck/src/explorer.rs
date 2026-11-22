//! Explorer view (the repository's working tree as a folder tree) with file operations
//! (new file / folder, rename, delete), and the text file view it opens files in (editable,
//! see `editor.rs`).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::app::*;
use crate::editor::{self, EditState};
use crate::git::{self, Group, Kind};
use crate::ui::Menu;
use crate::win::{self, MenuItem};

/// Files larger than this are not loaded into the viewer.
pub const MAX_VIEW_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ERow {
    /// Path relative to the repository root, '/' separated.
    pub path: String,
    pub name: String,
    pub depth: u8,
    pub dir: bool,
}

#[derive(Default)]
pub struct Explorer {
    pub rows: Vec<ERow>,
    /// Expanded folders.
    pub open: HashSet<String>,
    /// Selected row, by path.
    pub sel: Option<String>,
    /// Ignored paths from `git ls-files --ignored --directory` (folders without the trailing '/').
    pub ignored: HashSet<String>,
    /// Git status of changed files: (kind, letter).
    pub deco: HashMap<String, (Kind, char)>,
    /// Folders that contain changes, with the kind used for their color.
    pub dir_deco: HashMap<String, Kind>,
}

impl Explorer {
    pub fn is_ignored(&self, path: &str) -> bool {
        if self.ignored.is_empty() {
            return false;
        }
        if self.ignored.contains(path) {
            return true;
        }
        let mut p = path;
        while let Some((parent, _)) = p.rsplit_once('/') {
            if self.ignored.contains(parent) {
                return true;
            }
            p = parent;
        }
        false
    }

    pub fn sel_index(&self) -> Option<usize> {
        let s = self.sel.as_ref()?;
        self.rows.iter().position(|r| &r.path == s)
    }
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") }
}

/// Entries of a folder: folders first, then files, each sorted by name (case-insensitive).
/// `.git` is left out.
pub fn list_dir(root: &Path, rel: &str) -> Vec<(String, bool)> {
    let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut v: Vec<(String, bool)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                return None;
            }
            let is_dir = e.file_type().map(|t| t.is_dir() || (t.is_symlink() && e.path().is_dir())).unwrap_or(false);
            Some((name, is_dir))
        })
        .collect();
    // folders first, then by name ignoring case (keys computed once, not per comparison)
    v.sort_by_cached_key(|(n, d)| (!*d, n.to_lowercase(), n.clone()));
    v
}

/// Visible rows: the root's entries plus the contents of every expanded folder.
pub fn build_rows(root: &Path, open: &HashSet<String>) -> Vec<ERow> {
    fn walk(root: &Path, open: &HashSet<String>, rel: &str, depth: u8, out: &mut Vec<ERow>) {
        for (name, dir) in list_dir(root, rel) {
            let path = join(rel, &name);
            let expand = dir && open.contains(&path) && depth < 64;
            out.push(ERow { path: path.clone(), name, depth, dir });
            if expand {
                walk(root, open, &path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, open, "", 0, &mut out);
    out
}

/// Ignored files and folders (folders are reported once, not their contents).
pub fn ignored(root: &Path) -> HashSet<String> {
    let o = git::run(root, &["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"], None, true);
    o.text().split('\0').filter(|s| !s.is_empty()).map(|s| s.trim_end_matches('/').to_string()).collect()
}

/// Decodes file bytes as text: UTF-8 (with or without BOM) or UTF-16 with a BOM.
/// Returns None for binary content.
pub fn decode_text(bytes: &[u8]) -> Option<String> {
    let utf16 = |be: bool| {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
        String::from_utf16_lossy(&units)
    };
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(utf16(false));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(utf16(true));
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if git::is_binary(body) {
        return None;
    }
    Some(String::from_utf8_lossy(body).into_owned())
}

pub fn mtime(root: &Path, rel: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(root.join(rel)).and_then(|m| m.modified()).ok()
}

/// Splits file text into lines; true when most line breaks are CRLF (the ending used when
/// saving, so a stray CRLF doesn't turn an LF file into a CRLF one). Only the '\r' of a
/// "\r\n" is taken off, so "a\r\r\n" and a trailing lone '\r' survive a save.
pub fn split_lines(text: &str) -> (Vec<String>, bool) {
    let n = text.matches('\n').count();
    let crlf = text.matches("\r\n").count() * 2 > n;
    let lines = text.split('\n').enumerate().map(|(k, l)| if k < n { l.strip_suffix('\r').unwrap_or(l) } else { l }.to_string()).collect();
    (lines, crlf)
}

/// Builds the text view of a file (or a message when it can't be shown).
pub fn load_text(root: &Path, rel: &str) -> MainView {
    let name = short_name(rel).to_string();
    let p = root.join(rel);
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(e) => return MainView::Message(name, format!("The file could not be read: {e}")),
    };
    if meta.len() > MAX_VIEW_BYTES {
        return MainView::Message(name, format!("The file is not displayed because it is too large ({:.1} MB).", meta.len() as f64 / 1048576.0));
    }
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) => return MainView::Message(name, format!("The file could not be read: {e}")),
    };
    let Some((text, enc, lossless)) = editor::decode(&bytes) else {
        return MainView::Message(name, "The file is not displayed because it is binary.".into());
    };
    let (lines, crlf) = split_lines(&text);
    // a file with invalid UTF-8 / UTF-16 is shown read-only: saving would change those bytes
    let ed = EditState::new(&lines, enc, crlf, lossless);
    MainView::File(FileView { title: name, path: rel.to_string(), lines, conflicts: Vec::new(), change: None, mtime: meta.modified().ok(), ed, nav: None })
}

impl App {
    pub fn explorer_active(&self) -> bool {
        self.st.sidebar_visible && self.st.side_view == 1
    }

    /// Activity bar click: switches the side bar to `view` (0 Source Control, 1 Explorer);
    /// clicking the view that is already shown hides the side bar, like VS Code.
    pub fn set_side_view(&mut self, view: u8) {
        if self.st.sidebar_visible && self.st.side_view == view {
            self.set_sidebar_visible(false);
            return;
        }
        self.st.side_view = view;
        self.focus = if view == 1 { Focus::Explorer } else { Focus::Changes };
        self.set_sidebar_visible(true);
        if view == 1 {
            self.rebuild_explorer();
            // picks up the ignored files for the dimmed entries
            self.refresh();
        }
    }

    /// Re-reads the expanded folders and the git decorations.
    pub fn rebuild_explorer(&mut self) {
        let Some(root) = self.folder() else {
            self.ex.rows.clear();
            return;
        };
        let mut deco = HashMap::new();
        let mut dir_deco: HashMap<String, Kind> = HashMap::new();
        let files = self.repo.as_ref().map(|r| r.status.files.as_slice()).unwrap_or(&[]);
        // conflicts win over working tree changes, which win over staged ones
        for g in [Group::Staged, Group::Changes, Group::Merge] {
            for f in files.iter().filter(|f| f.group == g) {
                deco.insert(f.path.clone(), (f.kind, f.letter()));
                let mut p = f.path.as_str();
                while let Some((parent, _)) = p.rsplit_once('/') {
                    let e = dir_deco.entry(parent.to_string()).or_insert(f.kind);
                    if f.kind == Kind::Conflict {
                        *e = Kind::Conflict;
                    }
                    p = parent;
                }
            }
        }
        self.ex.deco = deco;
        self.ex.dir_deco = dir_deco;
        self.ex.rows = build_rows(&root, &self.ex.open);
        self.clamp_scroll(Area::Explorer);
        self.invalidate();
    }

    pub fn explorer_select(&mut self, i: usize) {
        if let Some(r) = self.ex.rows.get(i) {
            self.ex.sel = Some(r.path.clone());
            self.focus = Focus::Explorer;
        }
    }

    /// Collapses / expands the folder at row `i` (`expand`: None toggles).
    pub fn toggle_explorer_dir(&mut self, i: usize, expand: Option<bool>) {
        let Some(r) = self.ex.rows.get(i).cloned() else { return };
        if !r.dir {
            return;
        }
        self.explorer_select(i);
        let open = self.ex.open.contains(&r.path);
        let want = expand.unwrap_or(!open);
        if want == open {
            return;
        }
        if want {
            self.ex.open.insert(r.path);
        } else {
            let prefix = format!("{}/", r.path);
            self.ex.open.retain(|p| p != &r.path && !p.starts_with(&prefix));
        }
        self.rebuild_explorer();
    }

    pub fn explorer_collapse_all(&mut self) {
        self.ex.open.clear();
        self.rebuild_explorer();
    }

    /// Click / Enter: toggles a folder, opens a file (a preview tab unless `pin`).
    pub fn explorer_activate(&mut self, i: usize, pin: bool) {
        let Some(r) = self.ex.rows.get(i).cloned() else { return };
        if r.dir {
            self.toggle_explorer_dir(i, None);
        } else {
            self.explorer_select(i);
            self.open_file_viewer(r.path, pin);
            // Enter / double click: the keys go to the editor
            if pin {
                self.focus = Focus::Main;
            }
        }
    }

    /// Opens the read-only text view of `rel` in a tab.
    pub fn open_file_viewer(&mut self, rel: String, pin: bool) {
        let Some(root) = self.folder() else { return };
        if self.open_tab(format!("file:{rel}"), pin) && matches!(self.view, MainView::File(_) | MainView::Message(..)) {
            return;
        }
        self.view_gen += 1;
        let gen = self.view_gen;
        self.mark_tab_gen(gen);
        self.view = MainView::Loading(rel.clone());
        let tx = self.tx.clone();
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let view = load_text(&root, &rel);
            let _ = tx.send(JobResult::View { gen, view, keep_scroll: false });
            post(hwnd);
        });
        self.invalidate();
    }

    /// Reloads the active text view when its file changed on disk (keeps the scroll position).
    pub fn reload_text_view_if_changed(&mut self) {
        let Some(root) = self.folder() else { return };
        let MainView::File(f) = &self.view else { return };
        if f.change.is_some() || f.ed.dirty() {
            return;
        }
        let rel = f.path.clone();
        let now = mtime(&root, &rel);
        if now.is_some() && now == f.mtime {
            return;
        }
        self.view_gen += 1;
        let gen = self.view_gen;
        self.mark_tab_gen(gen);
        let tx = self.tx.clone();
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let view = load_text(&root, &rel);
            let _ = tx.send(JobResult::View { gen, view, keep_scroll: true });
            post(hwnd);
        });
    }

    /// Context menu of Explorer row `i` (usize::MAX: empty space).
    pub fn explorer_menu(&mut self, i: usize, m: &mut Menu) -> Vec<MenuItem> {
        let mut v = vec![m.it("New File...", move |a| a.explorer_new(i, false)), m.it("New Folder...", move |a| a.explorer_new(i, true)), MenuItem::Sep];
        let root = self.folder();
        if let (Some(r), Some(root)) = (self.ex.rows.get(i).cloned(), root.clone()) {
            // a Windows path for the clipboard and explorer.exe (the relative path is '/' separated)
            let abs = root.join(&r.path).display().to_string().replace('/', "\\");
            let (a1, a2, a3) = (abs.clone(), abs.clone(), abs);
            let rel = r.path.clone();
            // a folder opens the terminal in itself, a file in the folder that holds it
            let full = root.join(&r.path);
            let term_dir = if r.dir { full } else { full.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| root.clone()) };
            if r.dir {
                let open = self.ex.open.contains(&r.path);
                v.push(m.it(if open { "Collapse" } else { "Expand" }, move |a| a.toggle_explorer_dir(i, Some(!open))));
            } else {
                let (p1, p2) = (r.path.clone(), r.path.clone());
                v.push(m.it("Open\tEnter", move |a| a.open_file_viewer(p1, true)));
                let change = self.status().and_then(|s| {
                    [Group::Merge, Group::Changes, Group::Staged].iter().find_map(|g| s.files.iter().find(|f| f.group == *g && f.path == p2).cloned())
                });
                v.push(m.it_if("Open Changes", change.is_some(), move |a| {
                    if let Some(c) = change {
                        a.open_change(c, true);
                    }
                }));
                v.push(m.it("Open with Default App", move |_| win::shell_open(&a1)));
            }
            v.push(MenuItem::Sep);
            v.push(m.it("Reveal in File Explorer", move |_| win::reveal(&a2)));
            v.push(m.it("Open in Integrated Terminal", move |a| a.new_terminal_in(term_dir)));
            v.push(MenuItem::Sep);
            v.push(m.it("Copy Path", move |a| win::set_clipboard(a.hwnd, &a3)));
            v.push(m.it("Copy Relative Path", move |a| win::set_clipboard(a.hwnd, &rel)));
            v.push(MenuItem::Sep);
            v.push(m.it("Rename...\tF2", move |a| a.explorer_rename(i)));
            v.push(m.it("Delete\tDel", move |a| a.explorer_delete(i)));
            v.push(MenuItem::Sep);
        }
        v.push(m.it("Refresh Explorer\tF5", |a| a.refresh()));
        v.push(m.it("Collapse Folders in Explorer", |a| a.explorer_collapse_all()));
        v
    }
}

/// Checks a name typed for a new file / folder ('/' separated segments allowed when
/// `nested`); returns it with '\' turned into '/', or the problem.
pub fn validate_name(input: &str, nested: bool) -> Result<String, String> {
    let name = input.trim().replace('\\', "/");
    let name = name.trim_matches('/');
    if name.is_empty() {
        return Err("A file or folder name must be provided.".into());
    }
    if !nested && name.contains('/') {
        return Err("The name can't contain '/' or '\\'.".into());
    }
    for seg in name.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!("The name '{name}' is not valid."));
        }
        if seg.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 0x20) {
            return Err(format!("The name '{seg}' is not valid: it can't contain < > : \" | ? * or control characters."));
        }
        if seg.ends_with('.') || seg.ends_with(' ') {
            return Err(format!("The name '{seg}' is not valid: it can't end with a dot or a space."));
        }
        let stem = seg.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            return Err(format!("The name '{seg}' is reserved by Windows."));
        }
        if seg.eq_ignore_ascii_case(".git") {
            return Err("The name '.git' is reserved by Git.".into());
        }
    }
    Ok(name.to_string())
}

/// Replaces the `old` prefix of path `p` (the path itself or something inside it) with `new`.
fn rebase_path(p: &str, old: &str, new: &str) -> Option<String> {
    if p == old {
        Some(new.to_string())
    } else {
        p.strip_prefix(old).and_then(|rest| rest.strip_prefix('/')).map(|rest| join(new, rest))
    }
}

impl App {
    /// Folder that new files go into for Explorer row `i` (usize::MAX: the root): the
    /// folder itself, or a file's parent.
    fn explorer_dir_of(&self, i: usize) -> String {
        match self.ex.rows.get(i) {
            Some(r) if r.dir => r.path.clone(),
            Some(r) => r.path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default(),
            None => String::new(),
        }
    }

    /// Row the Explorer commands act on: the selected one, else the root.
    pub fn explorer_target(&self) -> usize {
        self.ex.sel_index().unwrap_or(usize::MAX)
    }

    /// Expands the folders down to `rel`, selects it and scrolls it into view.
    pub fn explorer_reveal(&mut self, rel: &str) {
        let mut p = rel;
        while let Some((parent, _)) = p.rsplit_once('/') {
            self.ex.open.insert(parent.to_string());
            p = parent;
        }
        self.ex.sel = Some(rel.to_string());
        self.rebuild_explorer();
        if let Some(i) = self.ex.sel_index() {
            self.reveal_row(Area::Explorer, i);
        }
    }

    /// "New File..." / "New Folder..." in the folder of row `i`. Like VS Code, a name with
    /// '/' creates the folders in between.
    pub fn explorer_new(&mut self, i: usize, folder: bool) {
        if self.folder().is_none() {
            return;
        }
        if !self.explorer_active() {
            self.set_side_view(1);
        }
        let dir = self.explorer_dir_of(i);
        let prompt = match (folder, dir.is_empty()) {
            (false, true) => "New file name".to_string(),
            (true, true) => "New folder name".to_string(),
            (false, false) => format!("New file name in '{dir}'"),
            (true, false) => format!("New folder name in '{dir}'"),
        };
        self.input(&prompt, "", move |a, name| {
            let Some(root) = a.folder() else { return };
            let name = match validate_name(&name, true) {
                Ok(n) => n,
                Err(e) => return a.error(e),
            };
            let rel = join(&dir, &name);
            let abs = root.join(&rel);
            if std::fs::symlink_metadata(&abs).is_ok() {
                return a.error(format!("A file or folder '{name}' already exists at this location. Choose a different name."));
            }
            let made = if folder {
                std::fs::create_dir_all(&abs)
            } else {
                abs.parent().map(std::fs::create_dir_all).unwrap_or(Ok(())).and_then(|_| std::fs::OpenOptions::new().write(true).create_new(true).open(&abs).map(|_| ()))
            };
            if let Err(e) = made {
                return a.error(format!("Unable to create '{rel}': {e}"));
            }
            a.explorer_reveal(&rel);
            if !folder {
                a.open_file_viewer(rel, true);
                a.focus = Focus::Main;
            }
            a.refresh();
        });
    }

    /// "Rename..." (F2) of row `i`. Open tabs follow the renamed file or folder.
    pub fn explorer_rename(&mut self, i: usize) {
        let Some(r) = self.ex.rows.get(i).cloned() else { return };
        let parent = r.path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
        let old = r.path.clone();
        self.input(&format!("New name for '{}'", r.name), &r.name, move |a, name| {
            let Some(root) = a.folder() else { return };
            let name = match validate_name(&name, false) {
                Ok(n) => n,
                Err(e) => return a.error(e),
            };
            let new = join(&parent, &name);
            if new == old {
                return;
            }
            // a case-only rename is allowed (the same file on Windows)
            if new.to_lowercase() != old.to_lowercase() && std::fs::symlink_metadata(root.join(&new)).is_ok() {
                return a.error(format!("A file or folder '{name}' already exists at this location. Choose a different name."));
            }
            if let Err(e) = std::fs::rename(root.join(&old), root.join(&new)) {
                return a.error(format!("Unable to rename '{old}': {e}"));
            }
            a.ex.open = a.ex.open.iter().map(|p| rebase_path(p, &old, &new).unwrap_or_else(|| p.clone())).collect();
            a.renamed_in_tabs(&old, &new);
            a.explorer_reveal(&new);
            a.refresh();
        });
    }

    /// Points the file tabs under `old` to `new` (unsaved edits are kept).
    fn renamed_in_tabs(&mut self, old: &str, new: &str) {
        for i in 0..self.tabs.len() {
            let v = if i == self.tab_active { &mut self.view } else { &mut self.tabs[i].view };
            if let MainView::File(f) = v {
                if f.change.is_some() {
                    continue;
                }
                if let Some(p) = rebase_path(&f.path, old, new) {
                    f.title = short_name(&p).to_string();
                    f.path = p.clone();
                    self.tabs[i].key = format!("file:{p}");
                }
            }
        }
        self.invalidate();
    }

    /// "Delete" (Del) of row `i`: moves it to the Recycle Bin after a confirmation and
    /// closes its tabs.
    pub fn explorer_delete(&mut self, i: usize) {
        let Some(r) = self.ex.rows.get(i).cloned() else { return };
        let message = if r.dir { format!("Are you sure you want to delete '{}' and its contents?", r.name) } else { format!("Are you sure you want to delete '{}'?", r.name) };
        let detail = if r.dir { "You can restore this folder from the Recycle Bin." } else { "You can restore this file from the Recycle Bin." };
        self.confirm(&message, detail, &["Move to Recycle Bin"], move |a, _| {
            let Some(root) = a.folder() else { return };
            let abs = root.join(&r.path);
            if recycle(&abs) {
                return a.explorer_deleted(i, &r.path);
            }
            // the dialog promised the Recycle Bin: deleting for good needs its own yes
            a.confirm("Failed to delete using the Recycle Bin. Do you want to permanently delete instead?", &format!("'{}' can't be restored after this.", r.path), &["Delete Permanently"], move |a, _| {
                if !delete_permanently(&abs) {
                    return a.error(format!("Unable to delete '{}'.", r.path));
                }
                a.explorer_deleted(i, &r.path);
            });
        });
    }

    /// After deleting `path` (row `i`): closes its tabs and selects the row that takes its place.
    fn explorer_deleted(&mut self, i: usize, path: &str) {
        // close the tabs of the deleted files; ones with unsaved changes stay open (saving
        // creates the file again), like VS Code
        for k in (0..self.tabs.len()).rev() {
            let gone = matches!(self.tab_view(k), MainView::File(f) if f.change.is_none() && rebase_path(&f.path, path, "").is_some());
            if gone && !self.tab_dirty(k) {
                self.close_tab_now(k);
            }
        }
        let prefix = format!("{path}/");
        self.ex.open.retain(|p| p != path && !p.starts_with(&prefix));
        // select the row that takes its place
        let next = self.ex.rows.iter().skip(i + 1).find(|x| !x.path.starts_with(&prefix)).or_else(|| if i > 0 { self.ex.rows.get(i - 1) } else { None }).map(|x| x.path.clone());
        self.ex.sel = next;
        self.rebuild_explorer();
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_text_encodings_and_rejects_binary() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi\r\nthere").as_deref(), Some("hi\r\nthere"));
        assert_eq!(decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]).as_deref(), Some("hi"));
        assert_eq!(decode_text(&[0xFE, 0xFF, 0, b'h', 0, b'i']).as_deref(), Some("hi"));
        assert_eq!(decode_text(&[b'a', 0, b'b']), None);
    }

    #[test]
    fn line_endings_follow_the_majority() {
        assert_eq!(split_lines("a\r\nb\r\nc"), (vec!["a".to_string(), "b".into(), "c".into()], true));
        // one stray CRLF doesn't make an LF file CRLF
        assert_eq!(split_lines("a\nb\r\nc\nd\n"), (vec!["a".to_string(), "b".into(), "c".into(), "d".into(), "".into()], false));
        // only the '\r' of a line break goes; a lone trailing '\r' stays
        assert_eq!(split_lines("a\r\r\nb\r"), (vec!["a\r".to_string(), "b\r".into()], true));
        assert_eq!(split_lines(""), (vec![String::new()], false));
    }

    #[test]
    fn lists_folders_first_and_expands_open_ones() {
        let dir = std::env::temp_dir().join(format!("devdeck-ex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/inner")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        for f in ["b.txt", "A.md", "src/main.rs", "src/inner/x.rs"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let rows = build_rows(&dir, &HashSet::new());
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["src", "A.md", "b.txt"]);
        let open: HashSet<String> = ["src".to_string(), "src/inner".to_string()].into();
        let rows = build_rows(&dir, &open);
        let paths: Vec<(&str, u8)> = rows.iter().map(|r| (r.path.as_str(), r.depth)).collect();
        assert_eq!(paths, [("src", 0), ("src/inner", 1), ("src/inner/x.rs", 2), ("src/main.rs", 1), ("A.md", 0), ("b.txt", 0)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validates_new_names() {
        assert_eq!(validate_name(" src\\a.rs ", true).as_deref(), Ok("src/a.rs"));
        assert!(validate_name("src/a.rs", false).is_err());
        assert!(validate_name("", true).is_err());
        assert!(validate_name("a/../b", true).is_err());
        assert!(validate_name("what?.txt", false).is_err());
        assert!(validate_name("name.", false).is_err());
        assert!(validate_name("nul.txt", false).is_err());
        assert!(validate_name("COM1", false).is_err());
        assert!(validate_name("COMMON.md", false).is_ok());
        assert!(validate_name(".GIT", false).is_err());
        assert_eq!(rebase_path("src/a/b.rs", "src/a", "lib"), Some("lib/b.rs".into()));
        assert_eq!(rebase_path("src/ab.rs", "src/a", "lib"), None);
        assert_eq!(rebase_path("src/a", "src/a", "lib"), Some("lib".into()));
        assert_eq!(rebase_path("src/a/b.rs", "src/a", ""), Some("b.rs".into()));
    }

    #[test]
    fn ignored_covers_children() {
        let mut ex = Explorer::default();
        ex.ignored.insert("target".into());
        assert!(ex.is_ignored("target"));
        assert!(ex.is_ignored("target/release/x.exe"));
        assert!(!ex.is_ignored("src/target.rs"));
    }
}
