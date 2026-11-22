//! Search view, like VS Code's: Find in Files (Ctrl+Shift+F) and Replace in Files
//! (Ctrl+Shift+H). The query runs on a background thread over the open folder's files (the ones
//! Go to File lists: tracked and untracked, not ignored), with the find widget's engine (match
//! case, whole word, regular expressions). Files with unsaved changes are searched as they are
//! in their tab. Results show as a tree of files and matching lines; Replace All (or the
//! replace buttons of a file or a match) changes the files on disk, or the text of the tabs
//! they are open in (saved when they had no unsaved changes, so one Ctrl+Z still undoes it).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::*;
use crate::editor::{self, char_len, Pos};
use crate::find::{self, Range};
use crate::theme::blend_colorref;
use crate::win::{self, rect, rh, xywh, Gfx, DT_LINE};

/// Matches kept over all files, like VS Code's search.maxResults.
pub const MAX_RESULTS: usize = 20000;
/// Delay between typing in the search box and the search starting.
const TYPE_DELAY_MS: u32 = 300;
/// Characters of a matching line shown in the results.
const PREVIEW_CHARS: usize = 250;

/// Search view buttons and boxes (`Hit::Search`).
pub const S_TOGGLE: u8 = 0;
pub const S_CASE: u8 = 1;
pub const S_WORD: u8 = 2;
pub const S_REGEX: u8 = 3;
pub const S_PRESERVE: u8 = 4;
pub const S_REPLACE_ALL: u8 = 5;
pub const S_FIND_INPUT: u8 = 6;
pub const S_REPLACE_INPUT: u8 = 7;
pub const S_REFRESH: u8 = 8;
pub const S_CLEAR: u8 = 9;
pub const S_COLLAPSE: u8 = 10;

/// Row actions (`Hit::SearchAct`).
pub const A_REPLACE: u8 = 0;
pub const A_DISMISS: u8 = 1;

/// A match in a file.
#[derive(Clone, Debug)]
pub struct SMatch {
    pub range: Range,
    /// The text shown for it: part of its first line (leading blanks off, cut around the match).
    pub preview: String,
    /// The match inside `preview` (char columns).
    pub hl: (usize, usize),
}

#[derive(Clone, Debug)]
pub struct SFile {
    /// Relative to the folder, '/' separated.
    pub path: String,
    pub matches: Vec<SMatch>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SRow {
    File(usize),
    Match(usize, usize),
}

/// The options a search ran with.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SQuery {
    pub text: String,
    pub case: bool,
    pub word: bool,
    pub regex: bool,
}

pub struct SearchView {
    pub query: String,
    pub replace_text: String,
    pub case: bool,
    pub word: bool,
    pub regex: bool,
    pub preserve: bool,
    /// The replace box is shown.
    pub show_replace: bool,
    pub files: Vec<SFile>,
    pub rows: Vec<SRow>,
    /// Files whose matches are hidden.
    pub collapsed: HashSet<String>,
    /// Selected row.
    pub sel: Option<usize>,
    pub running: bool,
    /// The query `files` were found for (None: no search yet).
    pub done: Option<SQuery>,
    /// There were more matches than MAX_RESULTS.
    pub capped: bool,
    /// Why the regular expression is invalid.
    pub error: Option<String>,
    /// Id of the newest search; older ones stop when they see it changed.
    gen: u64,
    current: Arc<AtomicU64>,
    pub find_edit: HWND,
    pub repl_edit: HWND,
    /// Where the edit controls are (x, y, width, height), None while hidden.
    placed: [Option<[i32; 4]>; 2],
    /// A match opened in a tab that is still loading: (view gen, range) selected when it loads.
    pub pending: Option<(u64, Range)>,
}

impl Default for SearchView {
    fn default() -> Self {
        SearchView {
            query: String::new(),
            replace_text: String::new(),
            case: false,
            word: false,
            regex: false,
            preserve: false,
            show_replace: false,
            files: Vec::new(),
            rows: Vec::new(),
            collapsed: HashSet::new(),
            sel: None,
            running: false,
            done: None,
            capped: false,
            error: None,
            gen: 0,
            current: Arc::new(AtomicU64::new(0)),
            find_edit: null_mut(),
            repl_edit: null_mut(),
            placed: [None; 2],
            pending: None,
        }
    }
}

impl SearchView {
    fn match_count(&self) -> usize {
        self.files.iter().map(|f| f.matches.len()).sum()
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (fi, f) in self.files.iter().enumerate() {
            self.rows.push(SRow::File(fi));
            if !self.collapsed.contains(&f.path) {
                self.rows.extend((0..f.matches.len()).map(|mi| SRow::Match(fi, mi)));
            }
        }
        if let Some(s) = self.sel {
            self.sel = if self.rows.is_empty() { None } else { Some(s.min(self.rows.len() - 1)) };
        }
    }

    fn query(&self) -> SQuery {
        SQuery { text: self.query.clone(), case: self.case, word: self.word, regex: self.regex }
    }
}

/// The preview of a match starting at `start` in `line` and ending at char column `end` of it
/// (the line's end for a match that goes on to the next lines).
fn preview(line: &str, start: usize, end: usize) -> (String, (usize, usize)) {
    let chars: Vec<char> = line.chars().collect();
    let lead = chars.iter().take_while(|c| c.is_whitespace()).count();
    // a match far into the line: some text before it stays, the rest is cut
    let from = if start > 40 { (start - 25).max(lead) } else { lead.min(start) };
    let to = chars.len().min(from + PREVIEW_CHARS).max(start.min(chars.len()));
    let mut s = String::new();
    let mut off = 0;
    if from > lead {
        s.push('\u{2026}');
        off = 1;
    }
    s.extend(chars[from..to].iter().map(|&c| if c == '\t' { ' ' } else { c }));
    let hs = start.saturating_sub(from) + off;
    let he = end.min(to).saturating_sub(from) + off;
    (s, (hs, he.max(hs)))
}

/// The matches of `q` in `lines`, at most `limit`; the flag tells whether there were more.
fn file_matches(search: &mut find::Search, lines: &[String], limit: usize) -> (Vec<SMatch>, bool) {
    let (ranges, capped) = search.all(lines, None, limit);
    let v = ranges
        .into_iter()
        .map(|(a, b)| {
            let line = &lines[a.0];
            let end = if b.0 == a.0 { b.1 } else { char_len(line) };
            let (preview, hl) = preview(line, a.1, end);
            SMatch { range: (a, b), preview, hl }
        })
        .collect();
    (v, capped)
}

/// Reads a file as lines for searching; None for binary, unreadable or too large files.
fn read_lines(root: &Path, rel: &str) -> Option<Vec<String>> {
    let p = root.join(rel);
    let meta = std::fs::metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > crate::explorer::MAX_VIEW_BYTES {
        return None;
    }
    let bytes = std::fs::read(&p).ok()?;
    let (text, _, _) = editor::decode(&bytes)?;
    Some(crate::explorer::split_lines(&text).0)
}

/// Runs a search on the folder's files with a few threads; stops early when `current` no longer
/// holds `gen` (a newer search started).
fn run(root: &Path, repo: bool, q: &SQuery, buffers: &HashMap<String, Vec<String>>, gen: u64, current: &AtomicU64) -> (Vec<SFile>, bool) {
    let files = crate::quickopen::list_files(root, repo);
    let next = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    let capped = std::sync::atomic::AtomicBool::new(false);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let mut found: Vec<(usize, SFile)> = std::thread::scope(|sc| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(|| {
                    let mut out = Vec::new();
                    let Ok(Some(mut search)) = find::Search::new(&q.text, q.case, q.word, q.regex) else { return out };
                    loop {
                        if current.load(Ordering::Relaxed) != gen || capped.load(Ordering::Relaxed) {
                            break;
                        }
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(rel) = files.get(i) else { break };
                        let owned;
                        let lines = match buffers.get(rel) {
                            Some(l) => l,
                            None => match read_lines(root, rel) {
                                Some(l) => {
                                    owned = l;
                                    &owned
                                }
                                None => continue,
                            },
                        };
                        let left = MAX_RESULTS.saturating_sub(total.load(Ordering::Relaxed));
                        if left == 0 {
                            capped.store(true, Ordering::Relaxed);
                            break;
                        }
                        let (matches, more) = file_matches(&mut search, lines, left);
                        if more {
                            capped.store(true, Ordering::Relaxed);
                        }
                        if !matches.is_empty() {
                            total.fetch_add(matches.len(), Ordering::Relaxed);
                            out.push((i, SFile { path: rel.clone(), matches }));
                        }
                    }
                    out
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
    });
    found.sort_by_key(|(i, _)| *i);
    // the threads may together have gone a little over the limit
    let mut left = MAX_RESULTS;
    let mut out = Vec::new();
    for (_, mut f) in found {
        if left == 0 {
            break;
        }
        f.matches.truncate(left);
        left -= f.matches.len();
        out.push(f);
    }
    (out, capped.load(Ordering::Relaxed))
}

/// Subclass of the search and replace boxes (single-line EDIT controls); `id` 6 is the replace box.
unsafe extern "system" fn search_edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, main: usize) -> LRESULT {
    let main = main as HWND;
    let repl = id == 6;
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wp as u16;
            if crate::altgr_char(vk) {
                return DefSubclassProc(hwnd, msg, wp, lp);
            }
            let (ctrl, shift, alt) = (crate::key_down(VK_CONTROL), crate::key_down(VK_SHIFT), crate::key_down(VK_MENU));
            if crate::with_app(main, |a| a.search_edit_key(vk, ctrl, shift, alt, repl)).unwrap_or(false) {
                let mut m: MSG = std::mem::zeroed();
                PeekMessageW(&mut m, hwnd, WM_CHAR, WM_CHAR, PM_REMOVE);
                PeekMessageW(&mut m, hwnd, WM_SYSCHAR, WM_SYSCHAR, PM_REMOVE);
                return 0;
            }
        }
        // Enter, Escape, Tab, Ctrl+A, Ctrl+Enter and Ctrl+Backspace are keys, not text
        WM_CHAR if matches!(wp, 0x0D | 0x1B | 0x09 | 0x01 | 0x0A | 0x7F) => return 0,
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

/// View geometry (client coordinates).
struct Geo {
    toggle: RECT,
    find_in: RECT,
    repl_in: RECT,
    repl_all: RECT,
    msg: RECT,
    list: RECT,
    opt_w: i32,
}

impl App {
    /// The Search view is shown in the side bar.
    pub fn search_visible(&self) -> bool {
        self.st.sidebar_visible && self.st.side_view == 2 && self.panel_mode() != PanelMode::FullScreen
    }

    fn search_geo(&self) -> Geo {
        let sb = self.l.sidebar;
        let top = self.l.title.bottom;
        let left = sb.left + self.s(24);
        let right = (sb.right - self.s(12)).max(left + self.s(60));
        let ih = self.s(26);
        let find_in = rect(left, top + self.s(2), right, top + self.s(2) + ih);
        let z = rect(0, 0, 0, 0);
        let (repl_in, repl_all) = if self.srch.show_replace {
            let y = find_in.bottom + self.s(4);
            let bw = self.s(24);
            (rect(left, y, (right - bw - self.s(4)).max(left + self.s(40)), y + ih), xywh(right - bw, y + (ih - bw) / 2, bw, bw))
        } else {
            (z, z)
        };
        let below = if self.srch.show_replace { repl_in.bottom } else { find_in.bottom };
        let msg = rect(sb.left + self.s(20), below + self.s(4), sb.right - self.s(12), below + self.s(26));
        let list = rect(sb.left, msg.bottom + self.s(2), sb.right, sb.bottom.max(msg.bottom + self.s(2)));
        Geo { toggle: rect(sb.left + self.s(4), find_in.top, sb.left + self.s(20), below), find_in, repl_in, repl_all, msg, list, opt_w: self.s(22) }
    }

    /// The results list's rectangle (for the layout).
    pub fn search_list_rect(&self) -> RECT {
        self.search_geo().list
    }

    fn search_edits(&mut self) {
        if !self.srch.find_edit.is_null() {
            return;
        }
        let class = win::wz("EDIT");
        let empty = win::wz("");
        unsafe {
            let hinst = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            for (k, cue) in [(0usize, "Search"), (1, "Replace")] {
                let h = CreateWindowExW(0, class.as_ptr(), empty.as_ptr(), WS_CHILD | ES_AUTOHSCROLL as u32, 0, 0, 10, 10, self.hwnd, (106 + k) as HMENU, hinst, std::ptr::null());
                SetWindowSubclass(h, Some(search_edit_proc), 5 + k, self.hwnd as usize);
                SendMessageW(h, WM_SETFONT, self.fonts.ui as WPARAM, 1);
                let c = win::wz(cue);
                SendMessageW(h, 0x1501 /* EM_SETCUEBANNER */, 1, c.as_ptr() as isize);
                win::set_control_theme(h, self.t.dark);
                if k == 0 {
                    self.srch.find_edit = h;
                } else {
                    self.srch.repl_edit = h;
                }
            }
        }
    }

    /// Where the edit controls go (x, y, width, height): the boxes' space left of their toggles.
    fn search_edit_rects(&self) -> [Option<[i32; 4]>; 2] {
        if !self.search_visible() {
            return [None; 2];
        }
        let geo = self.search_geo();
        let eh = self.s(18);
        let one = |r: RECT, n: i32| {
            let x = r.left + self.s(5);
            Some([x, r.top + (rh(&r) - eh) / 2, r.right - self.s(5) - geo.opt_w * n - x, eh])
        };
        [one(geo.find_in, 3), if self.srch.show_replace { one(geo.repl_in, 1) } else { None }]
    }

    /// Moves / shows / hides the edit controls to follow the view.
    pub fn search_place(&mut self) {
        if self.srch.find_edit.is_null() {
            return;
        }
        let want = self.search_edit_rects();
        for (k, want) in want.into_iter().enumerate() {
            let h = if k == 0 { self.srch.find_edit } else { self.srch.repl_edit };
            if want == self.srch.placed[k] {
                continue;
            }
            unsafe {
                match want {
                    Some([x, y, w, hh]) => {
                        MoveWindow(h, x, y, w.max(10), hh, 1);
                        if self.srch.placed[k].is_none() {
                            ShowWindow(h, SW_SHOWNOACTIVATE);
                        }
                    }
                    None => {
                        if GetFocus() == h {
                            SetFocus(self.hwnd);
                        }
                        ShowWindow(h, SW_HIDE);
                    }
                }
            }
            self.srch.placed[k] = want;
        }
    }

    fn search_focus_edit(&mut self, repl: bool) {
        self.search_edits();
        self.search_place();
        let h = if repl && self.srch.show_replace { self.srch.repl_edit } else { self.srch.find_edit };
        self.focus = Focus::Search;
        unsafe {
            if IsWindowVisible(h) != 0 {
                SetFocus(h);
                SendMessageW(h, 0x00B1 /* EM_SETSEL */, 0, -1);
            }
        }
        self.invalidate();
    }

    /// The side bar shows the Search view (activity bar, Ctrl+Shift+F); `focus`: the keyboard
    /// goes to the search box.
    pub fn show_search(&mut self, replace: bool) {
        if replace {
            self.srch.show_replace = true;
        }
        if !(self.st.sidebar_visible && self.st.side_view == 2) {
            self.st.side_view = 2;
            self.set_sidebar_visible(true);
        }
        if self.panel_mode() == PanelMode::FullScreen {
            self.set_panel_mode(PanelMode::Normal);
        }
        self.search_edits();
        self.layout();
    }

    /// Ctrl+Shift+F / Ctrl+Shift+H: shows the view, seeded with the editor's selection (one
    /// line) or the word at the caret, and puts the keyboard in the search box (the replace
    /// box for Ctrl+Shift+H when there is a query).
    pub fn find_in_files(&mut self, replace: bool) {
        self.open_search(replace, true);
    }

    /// Opens the Search view with the keyboard in its search box; `seed`: with the editor's
    /// selection or word (Ctrl+Shift+F), not for an activity bar click.
    pub fn open_search(&mut self, replace: bool, seed: bool) {
        let seed = match &self.view {
            MainView::File(f) if seed && self.focus == Focus::Main => match f.sel() {
                Some((a, b)) if a.0 == b.0 => f.selected_text(),
                Some(_) => None,
                None if f.ed.editable => crate::find::word_at(&f.lines[f.ed.cur.0], f.ed.cur.1),
                None => None,
            },
            _ => None,
        };
        self.show_search(replace);
        if let Some(s) = seed.filter(|s| !s.is_empty()) {
            let q = if self.srch.regex { find::escape_regex(&s) } else { s };
            win::set_text(self.srch.find_edit, &q);
            self.srch.query = q;
            self.search_start();
        } else {
            win::set_text(self.srch.find_edit, &self.srch.query.clone());
        }
        win::set_text(self.srch.repl_edit, &self.srch.replace_text.clone());
        let to_replace = replace && !self.srch.query.is_empty();
        self.search_focus_edit(to_replace);
    }

    /// Typing in the search box: searches once typing pauses.
    pub fn search_query_changed(&mut self) {
        let t = win::get_text(self.srch.find_edit);
        if t == self.srch.query {
            return;
        }
        self.srch.query = t;
        unsafe {
            SetTimer(self.hwnd, TIMER_SEARCH, TYPE_DELAY_MS, None);
        }
    }

    pub fn search_replace_changed(&mut self) {
        self.srch.replace_text = win::get_text(self.srch.repl_edit);
        self.invalidate();
    }

    /// The typing delay passed.
    pub fn search_timer(&mut self) {
        unsafe {
            KillTimer(self.hwnd, TIMER_SEARCH);
        }
        if self.srch.done.as_ref() != Some(&self.srch.query()) || self.srch.running {
            self.search_start();
        }
    }

    /// Starts a search with the query and options (an empty query clears the results).
    pub fn search_start(&mut self) {
        unsafe {
            KillTimer(self.hwnd, TIMER_SEARCH);
        }
        self.srch.gen += 1;
        let gen = self.srch.gen;
        self.srch.current.store(gen, Ordering::Relaxed);
        let q = self.srch.query();
        self.srch.error = None;
        let root = self.folder();
        if q.text.is_empty() || root.is_none() {
            self.srch.running = false;
            self.srch.done = None;
            self.srch.files.clear();
            self.srch.capped = false;
            self.srch.sel = None;
            self.srch.rebuild_rows();
            self.invalidate();
            return;
        }
        if let Err(e) = find::Search::new(&q.text, q.case, q.word, q.regex) {
            self.srch.running = false;
            self.srch.error = Some(e);
            self.srch.done = None;
            self.srch.files.clear();
            self.srch.rebuild_rows();
            self.invalidate();
            return;
        }
        let root = root.unwrap();
        // files with unsaved changes are searched as they are in their tab
        let buffers: HashMap<String, Vec<String>> = (0..self.tabs.len()).filter_map(|i| self.tab_file(i)).filter(|f| f.ed.dirty()).map(|f| (f.path.clone(), f.lines.clone())).collect();
        self.srch.running = true;
        let (tx, hwnd, repo, current) = (self.tx.clone(), self.hwnd as isize, self.repo.is_some(), self.srch.current.clone());
        std::thread::spawn(move || {
            let (files, capped) = run(&root, repo, &q, &buffers, gen, &current);
            if current.load(Ordering::Relaxed) == gen {
                let _ = tx.send(JobResult::Search { gen, query: q, files, capped });
                post(hwnd);
            }
        });
        self.invalidate();
    }

    pub fn on_search_done(&mut self, gen: u64, query: SQuery, files: Vec<SFile>, capped: bool) {
        if gen != self.srch.gen {
            return;
        }
        self.srch.running = false;
        self.srch.done = Some(query);
        self.srch.files = files;
        self.srch.capped = capped;
        self.srch.sel = None;
        self.scroll[Area::Search as usize] = 0;
        self.srch.rebuild_rows();
        self.clamp_scroll(Area::Search);
        self.invalidate();
    }

    pub fn search_clear(&mut self) {
        self.srch.query.clear();
        win::set_text(self.srch.find_edit, "");
        self.search_start();
    }

    fn search_toggle(&mut self, what: u8) {
        match what {
            S_CASE => self.srch.case = !self.srch.case,
            S_WORD => self.srch.word = !self.srch.word,
            S_REGEX => self.srch.regex = !self.srch.regex,
            S_PRESERVE => {
                self.srch.preserve = !self.srch.preserve;
                self.invalidate();
                return;
            }
            _ => return,
        }
        self.search_start();
    }

    pub fn search_collapse_all(&mut self) {
        let all_collapsed = self.srch.files.iter().all(|f| self.srch.collapsed.contains(&f.path));
        if all_collapsed {
            self.srch.collapsed.clear();
        } else {
            self.srch.collapsed = self.srch.files.iter().map(|f| f.path.clone()).collect();
        }
        self.srch.sel = None;
        self.srch.rebuild_rows();
        self.clamp_scroll(Area::Search);
        self.invalidate();
    }

    pub fn search_click(&mut self, what: u8) {
        match what {
            S_TOGGLE => {
                self.srch.show_replace = !self.srch.show_replace;
                self.layout();
                if self.srch.show_replace {
                    win::set_text(self.srch.repl_edit, &self.srch.replace_text.clone());
                }
                let to_replace = self.srch.show_replace;
                self.search_focus_edit(to_replace);
            }
            S_CASE | S_WORD | S_REGEX | S_PRESERVE => self.search_toggle(what),
            S_REPLACE_ALL => self.search_replace(None),
            S_FIND_INPUT => self.search_focus_edit(false),
            S_REPLACE_INPUT => self.search_focus_edit(true),
            S_REFRESH => self.search_start(),
            S_CLEAR => self.search_clear(),
            S_COLLAPSE => self.search_collapse_all(),
            _ => {}
        }
        self.invalidate();
    }

    pub fn search_tooltip(&self, what: u8) -> Option<String> {
        Some(
            match what {
                S_TOGGLE => "Toggle Replace",
                S_CASE => "Match Case (Alt+C)",
                S_WORD => "Match Whole Word (Alt+W)",
                S_REGEX => "Use Regular Expression (Alt+R)",
                S_PRESERVE => "Preserve Case (Alt+P)",
                S_REPLACE_ALL => "Replace All (Ctrl+Alt+Enter)",
                S_REFRESH => "Refresh",
                S_CLEAR => "Clear Search Results",
                S_COLLAPSE => "Collapse All / Expand All",
                _ => return None,
            }
            .into(),
        )
    }

    pub fn search_row_tooltip(&self, i: usize) -> Option<String> {
        match *self.srch.rows.get(i)? {
            SRow::File(fi) => {
                let f = self.srch.files.get(fi)?;
                Some(self.folder()?.join(&f.path).display().to_string().replace('/', "\\"))
            }
            SRow::Match(..) => None,
        }
    }

    pub fn search_act_tooltip(&self, i: usize, a: u8) -> Option<String> {
        let file = matches!(self.srch.rows.get(i)?, SRow::File(_));
        Some(
            match (a, file) {
                (A_REPLACE, true) => "Replace All in File",
                (A_REPLACE, false) => "Replace",
                (_, true) => "Dismiss File",
                (_, false) => "Dismiss",
            }
            .into(),
        )
    }

    // -------------------------------------------------------------- results

    /// Moving through the results with the keyboard: a match row is shown in the preview tab
    /// with the match selected, like a click (the keyboard stays in the list).
    fn search_select(&mut self, i: usize) {
        self.srch.sel = Some(i);
        self.focus = Focus::Search;
        if let Some(SRow::Match(fi, mi)) = self.srch.rows.get(i).copied() {
            if let Some(f) = self.srch.files.get(fi) {
                let (path, range) = (f.path.clone(), f.matches[mi].range);
                self.search_open(path, range, false);
            }
        }
        self.reveal_row(Area::Search, i);
        self.invalidate();
    }

    /// Clicking a row: a file row folds / unfolds its matches, a match opens in the preview tab
    /// (`pin`: a kept tab with the keyboard in it) with the match selected.
    pub fn search_activate(&mut self, i: usize, pin: bool) {
        let Some(row) = self.srch.rows.get(i).copied() else { return };
        self.srch.sel = Some(i);
        self.focus = Focus::Search;
        match row {
            SRow::File(fi) => {
                if !pin {
                    self.search_fold(fi, None);
                } else if let Some(f) = self.srch.files.get(fi) {
                    let path = f.path.clone();
                    self.open_file_viewer(path, true);
                    self.focus = Focus::Main;
                }
            }
            SRow::Match(fi, mi) => {
                let Some(f) = self.srch.files.get(fi) else { return };
                let (path, range) = (f.path.clone(), f.matches[mi].range);
                self.search_open(path, range, pin);
            }
        }
        self.invalidate();
    }

    /// Folds (`Some(false)`), unfolds (`Some(true)`) or toggles the matches of file `fi`.
    fn search_fold(&mut self, fi: usize, open: Option<bool>) {
        let Some(f) = self.srch.files.get(fi) else { return };
        let path = f.path.clone();
        let is_open = !self.srch.collapsed.contains(&path);
        let want = open.unwrap_or(!is_open);
        if want == is_open {
            return;
        }
        if want {
            self.srch.collapsed.remove(&path);
        } else {
            self.srch.collapsed.insert(path);
        }
        self.srch.rebuild_rows();
        self.srch.sel = self.srch.rows.iter().position(|r| *r == SRow::File(fi));
        self.clamp_scroll(Area::Search);
    }

    fn search_open(&mut self, path: String, range: Range, pin: bool) {
        self.open_file_viewer(path.clone(), pin);
        match &self.view {
            MainView::File(f) if f.path == path => self.search_show_match(range),
            MainView::Loading(_) => self.srch.pending = self.tabs.get(self.tab_active).map(|t| (t.gen, range)),
            _ => {}
        }
        if self.explorer_active() {
            self.explorer_reveal(&path);
        }
        self.focus = if pin { Focus::Main } else { Focus::Search };
        if pin {
            unsafe {
                SetFocus(self.hwnd);
            }
        }
        self.sync_term_focus();
    }

    /// Selects a match in the active file view (clamped to its text, which may have changed).
    fn search_show_match(&mut self, r: Range) {
        let MainView::File(f) = &self.view else { return };
        if f.lines.is_empty() {
            return;
        }
        let clamp = |p: Pos| {
            let l = p.0.min(f.lines.len() - 1);
            (l, p.1.min(char_len(&f.lines[l])))
        };
        let r = (clamp(r.0), clamp(r.1));
        self.find_select(r);
    }

    /// A view finished loading into the tab with `gen`: the match opened in it.
    pub fn take_search_pending(&mut self, gen: u64) {
        if let Some((g, r)) = self.srch.pending {
            if g == gen {
                self.srch.pending = None;
                self.search_show_match(r);
            }
        }
    }

    /// Removes a match, or a file's matches, from the results.
    fn search_dismiss(&mut self, i: usize) {
        let Some(row) = self.srch.rows.get(i).copied() else { return };
        match row {
            SRow::File(fi) => {
                self.srch.files.remove(fi);
            }
            SRow::Match(fi, mi) => {
                let f = &mut self.srch.files[fi];
                f.matches.remove(mi);
                if f.matches.is_empty() {
                    self.srch.files.remove(fi);
                }
            }
        }
        self.srch.rebuild_rows();
        self.clamp_scroll(Area::Search);
        self.invalidate();
    }

    pub fn search_act(&mut self, i: usize, a: u8) {
        match a {
            A_REPLACE => {
                let target = match self.srch.rows.get(i) {
                    Some(SRow::File(fi)) => Some((*fi, None)),
                    Some(SRow::Match(fi, mi)) => Some((*fi, Some(*mi))),
                    None => return,
                };
                self.search_replace(target);
            }
            _ => self.search_dismiss(i),
        }
    }

    // -------------------------------------------------------------- replace

    /// Replace All (`target` None, after asking), or the matches of one file / one match.
    pub fn search_replace(&mut self, target: Option<(usize, Option<usize>)>) {
        if self.srch.running || self.srch.done.as_ref() != Some(&self.srch.query()) {
            self.info("The search is not finished yet. Try again when it is done.");
            return;
        }
        let mut todo: Vec<(String, HashSet<Range>)> = Vec::new();
        for (fi, f) in self.srch.files.iter().enumerate() {
            match target {
                Some((t, _)) if t != fi => continue,
                Some((_, Some(mi))) => todo.push((f.path.clone(), f.matches.get(mi).map(|m| m.range).into_iter().collect())),
                _ => todo.push((f.path.clone(), f.matches.iter().map(|m| m.range).collect())),
            }
        }
        let n: usize = todo.iter().map(|(_, s)| s.len()).sum();
        if n == 0 {
            self.info("No results to replace.");
            return;
        }
        if target.is_some() {
            self.search_do_replace(todo);
            return;
        }
        let files = todo.len();
        let rep = &self.srch.replace_text;
        let msg = format!(
            "Replace {n} occurrence{} across {files} file{} with '{rep}'?",
            if n == 1 { "" } else { "s" },
            if files == 1 { "" } else { "s" }
        );
        self.confirm(&msg, "Files open in a tab are changed in the tab (and saved when they had no unsaved changes).", &["Replace"], move |a, _| a.search_do_replace(todo));
    }

    fn search_do_replace(&mut self, todo: Vec<(String, HashSet<Range>)>) {
        let Some(root) = self.folder() else { return };
        let q = self.srch.query();
        let Ok(Some(mut search)) = find::Search::new(&q.text, q.case, q.word, q.regex) else { return };
        let rep = self.srch.replace_text.clone();
        let preserve = self.srch.preserve;
        let (mut count, mut files) = (0usize, 0usize);
        let mut errors = Vec::new();
        for (path, keep) in todo {
            let mut keep_fn = |a: Pos, b: Pos| keep.contains(&(a, b));
            let key = format!("file:{path}");
            let tab = self.tabs.iter().position(|t| t.key == key).filter(|&i| self.tab_file(i).is_some_and(|f| f.ed.editable));
            if let Some(i) = tab {
                let was_dirty = self.tab_dirty(i);
                let Some(f) = self.tab_file_mut(i) else { continue };
                let Some((first, old_end, text, n)) = find::replace_lines(&mut search, &f.lines, &rep, preserve, &mut keep_fn) else { continue };
                let (cur, anchor) = (f.ed.cur, f.ed.anchor);
                f.edit_range((first, 0), old_end, &text);
                let clamp = |f: &FileView, p: Pos| {
                    let l = p.0.min(f.lines.len() - 1);
                    (l, p.1.min(char_len(&f.lines[l])))
                };
                let (c, a) = (clamp(f, cur), clamp(f, anchor));
                f.move_to(a, false);
                f.move_to(c, true);
                if let Some(t) = self.tabs.get_mut(i) {
                    t.preview = false;
                }
                count += n;
                files += 1;
                if !was_dirty {
                    self.save_tab(i, false);
                }
                continue;
            }
            match replace_on_disk(&root, &path, &mut search, &rep, preserve, &mut keep_fn) {
                Ok(0) => {}
                Ok(n) => {
                    count += n;
                    files += 1;
                }
                Err(e) => errors.push(e),
            }
        }
        if let Some(e) = errors.first() {
            let more = if errors.len() > 1 { format!(" (and {} more)", errors.len() - 1) } else { String::new() };
            self.error(format!("{e}{more}"));
        }
        if count > 0 {
            self.info(format!("Replaced {count} occurrence{} across {files} file{}.", if count == 1 { "" } else { "s" }, if files == 1 { "" } else { "s" }));
        }
        self.refresh();
        // the results follow the files
        self.search_start();
        self.invalidate();
    }

    // -------------------------------------------------------------- keys

    /// Keys in the search / replace boxes; true when handled.
    pub fn search_edit_key(&mut self, vk: u16, ctrl: bool, shift: bool, alt: bool, repl: bool) -> bool {
        match vk {
            // Enter: search now; Ctrl+Alt+Enter: replace all
            0x0D => {
                if ctrl && alt {
                    self.search_replace(None);
                } else if !ctrl && !alt {
                    self.search_start();
                }
                true
            }
            0x1B => {
                // the keyboard goes to the results
                self.focus = Focus::Search;
                unsafe {
                    SetFocus(self.hwnd);
                }
                self.invalidate();
                true
            }
            // Tab: search box, replace box, results
            0x09 if !ctrl && !alt => {
                match (repl, shift) {
                    (false, false) if self.srch.show_replace => self.search_focus_edit(true),
                    (true, true) => self.search_focus_edit(false),
                    (false, true) => {}
                    _ => self.search_focus_results(),
                }
                true
            }
            // Down: into the results
            0x28 if !ctrl && !alt && !shift && !self.srch.rows.is_empty() => {
                self.search_focus_results();
                true
            }
            0x52 if alt && shift && !ctrl => {
                self.exec(Cmd::RevealInExplorer);
                true
            }
            0x43 | 0x57 | 0x52 | 0x50 if alt && !ctrl => self.search_alt_key(vk),
            0x41 if ctrl && !shift && !alt => {
                let h = if repl { self.srch.repl_edit } else { self.srch.find_edit };
                unsafe {
                    SendMessageW(h, 0x00B1, 0, -1);
                }
                true
            }
            // Ctrl+Shift+F / Ctrl+Shift+H in the view: its search / replace box
            0x46 if ctrl && shift && !alt => {
                self.search_focus_edit(false);
                true
            }
            0x48 if ctrl && shift && !alt => {
                self.srch.show_replace = true;
                self.layout();
                win::set_text(self.srch.repl_edit, &self.srch.replace_text.clone());
                self.search_focus_edit(true);
                true
            }
            // the other shortcuts work as anywhere else; Ctrl+C / V / X / Z go to the box
            _ if (ctrl && !alt) || (0x70..=0x87).contains(&vk) => self.global_key(vk, ctrl, shift),
            _ => false,
        }
    }

    /// Alt+C / W / R / P while the Search view has the keyboard.
    pub fn search_alt_key(&mut self, vk: u16) -> bool {
        match vk {
            0x43 => self.search_toggle(S_CASE),
            0x57 => self.search_toggle(S_WORD),
            0x52 => self.search_toggle(S_REGEX),
            0x50 if self.srch.show_replace => self.search_toggle(S_PRESERVE),
            _ => return false,
        }
        self.invalidate();
        true
    }

    fn search_focus_results(&mut self) {
        self.focus = Focus::Search;
        if self.srch.sel.is_none() && !self.srch.rows.is_empty() {
            self.srch.sel = Some(0);
        }
        unsafe {
            SetFocus(self.hwnd);
        }
        if let Some(i) = self.srch.sel {
            self.reveal_row(Area::Search, i);
        }
        self.invalidate();
    }

    /// Keys of the results list.
    pub fn search_key(&mut self, vk: u16) {
        let n = self.srch.rows.len();
        if n == 0 || !self.search_visible() {
            if vk == 0x26 || vk == 0x0D {
                self.search_focus_edit(false);
            }
            return;
        }
        let cur = self.srch.sel;
        let c = cur.unwrap_or(0);
        let next = match vk {
            // Up on the first row goes back to the search box
            0x26 if c == 0 => {
                self.search_focus_edit(false);
                return;
            }
            0x26 => Some(c - 1),
            0x28 => Some(if cur.is_none() { 0 } else { (c + 1).min(n - 1) }),
            0x24 => Some(0),
            0x23 => Some(n - 1),
            0x21 => Some(c.saturating_sub(10)),
            0x22 => Some((c + 10).min(n - 1)),
            _ => None,
        };
        if let Some(i) = next {
            self.search_select(i);
            return;
        }
        match vk {
            // Enter opens a match in a kept tab, Space in the preview tab
            0x0D => self.search_activate(c, matches!(self.srch.rows[c], SRow::Match(..))),
            0x20 => self.search_activate(c, false),
            0x25 => match self.srch.rows[c] {
                SRow::File(fi) => self.search_fold(fi, Some(false)),
                SRow::Match(fi, _) => {
                    if let Some(p) = self.srch.rows.iter().position(|r| *r == SRow::File(fi)) {
                        self.srch.sel = Some(p);
                    }
                }
            },
            0x27 => {
                if let SRow::File(fi) = self.srch.rows[c] {
                    self.search_fold(fi, Some(true));
                }
            }
            0x2E | 0x08 if cur.is_some() => self.search_dismiss(c),
            _ => {}
        }
        if let Some(i) = self.srch.sel {
            self.reveal_row(Area::Search, i);
        }
        self.invalidate();
    }

    // -------------------------------------------------------------- painting

    pub fn paint_search(&mut self, g: &Gfx) {
        let l = self.l;
        let t = &self.t;
        g.fill(&l.sidebar, t.sidebar);
        let edge = if self.st.sidebar_right { l.sidebar.left - 1 } else { l.sidebar.right };
        g.vline(edge, l.sidebar.top, l.sidebar.bottom, t.border);
        let geo = self.search_geo();
        let hover = self.hover;
        let mut hits: Vec<(RECT, Hit)> = Vec::new();
        let s2 = self.s(2);
        // title and its buttons
        let bs = self.s(22);
        let by = l.title.top + (rh(&l.title) - bs) / 2;
        let mut x = l.title.right - self.s(8) - bs;
        let has = !self.srch.files.is_empty();
        for (what, glyph, enabled) in [(S_COLLAPSE, '\u{E70E}', has), (S_CLEAR, '\u{E894}', has || !self.srch.query.is_empty()), (S_REFRESH, '\u{E72C}', !self.srch.query.is_empty())] {
            let r = xywh(x, by, bs, bs);
            if enabled && hover == Hit::Search(what) {
                g.round_rect(&r, self.s(5), t.toolbar_hover, None);
            }
            g.icon(&r, glyph, self.fonts.icon, if enabled { t.fg } else { t.fg_dim });
            if enabled {
                hits.push((r, Hit::Search(what)));
            }
            x -= bs + self.s(2);
        }
        g.text_in(&rect(l.title.left + self.s(20), l.title.top, x + bs, l.title.bottom), "SEARCH", self.fonts.cap, t.fg, DT_LINE);
        // the replace toggle
        let r = geo.toggle;
        if hover == Hit::Search(S_TOGGLE) {
            g.round_rect(&r, s2 + 1, t.toolbar_hover, None);
        }
        g.icon(&r, if self.srch.show_replace { '\u{E70D}' } else { '\u{E76C}' }, self.fonts.icon_small, t.fg);
        hits.push((r, Hit::Search(S_TOGGLE)));
        // the input boxes, with their option toggles inside on the right
        let focus = unsafe { GetFocus() };
        let error = self.srch.error.is_some();
        let mut boxes = vec![(geo.find_in, false, S_FIND_INPUT)];
        if self.srch.show_replace {
            boxes.push((geo.repl_in, true, S_REPLACE_INPUT));
        }
        for (r, repl, what) in boxes {
            let edit = if repl { self.srch.repl_edit } else { self.srch.find_edit };
            g.fill(&r, t.input_bg);
            let border = if !repl && error {
                t.error
            } else if focus == edit && !edit.is_null() {
                t.focus
            } else {
                t.input_border
            };
            g.frame(&r, border);
            hits.push((r, Hit::Search(what)));
            let opts: &[(u8, &str, bool)] = if repl { &[(S_PRESERVE, "AB", self.srch.preserve)] } else { &[(S_CASE, "Aa", self.srch.case), (S_WORD, "ab", self.srch.word), (S_REGEX, ".*", self.srch.regex)] };
            let oy = r.top + (rh(&r) - self.s(20)) / 2;
            let mut ox = r.right - self.s(3) - geo.opt_w * opts.len() as i32;
            for &(w, label, on) in opts {
                let or = xywh(ox, oy, geo.opt_w - self.s(2), self.s(20));
                let hit = Hit::Search(w);
                if on {
                    g.round_rect(&or, s2 + 1, blend_colorref(t.input_bg, t.accent, 0.3), Some(t.accent));
                } else if hover == hit {
                    g.round_rect(&or, s2 + 1, t.toolbar_hover, None);
                }
                g.text_in(&or, label, self.fonts.small, if on { t.fg_strong } else { t.fg }, DT_LINE | windows_sys::Win32::Graphics::Gdi::DT_CENTER);
                if w == S_WORD {
                    let (x0, x1, y) = (or.left + self.s(5), or.right - self.s(5), or.bottom - self.s(5));
                    g.hline(x0, x1, y, t.fg);
                    g.vline(x0, y - self.s(2), y + 1, t.fg);
                    g.vline(x1 - 1, y - self.s(2), y + 1, t.fg);
                }
                hits.push((or, hit));
                ox += geo.opt_w;
            }
        }
        if self.srch.show_replace {
            let r = geo.repl_all;
            let enabled = has && !self.srch.running;
            if enabled && hover == Hit::Search(S_REPLACE_ALL) {
                g.round_rect(&r, s2 + 1, t.toolbar_hover, None);
            }
            g.icon(&r, '\u{E8EE}', self.fonts.icon_small, if enabled { t.fg } else { t.fg_dim });
            if enabled {
                hits.push((r, Hit::Search(S_REPLACE_ALL)));
            }
        }
        // what was found
        let n = self.srch.match_count();
        let (msg, color) = if let Some(e) = &self.srch.error {
            (e.clone(), t.error)
        } else if self.folder().is_none() {
            ("Open a folder to search in its files.".to_string(), t.fg_dim)
        } else if self.srch.running && self.srch.files.is_empty() {
            ("Searching...".to_string(), t.fg_dim)
        } else if self.srch.done.is_none() {
            (String::new(), t.fg_dim)
        } else if n == 0 {
            ("No results found.".to_string(), t.fg_dim)
        } else {
            let files = self.srch.files.len();
            let mut m = format!("{n} result{} in {files} file{}", if n == 1 { "" } else { "s" }, if files == 1 { "" } else { "s" });
            if self.srch.capped {
                m.push_str(" (only the first ones are shown)");
            }
            if self.srch.running {
                m.push_str(" - searching...");
            }
            (m, t.fg_dim)
        };
        g.text_in(&geo.msg, &msg, self.fonts.small, color, DT_LINE);
        self.hits.extend(hits);
        self.paint_search_results(g, geo.list);
    }

    fn paint_search_results(&mut self, g: &Gfx, r: RECT) {
        let rh22 = self.s(22);
        let saved = g.clip(&r);
        let scroll = self.scroll[Area::Search as usize];
        let first = (scroll / rh22).max(0) as usize;
        let n = (rh(&r) / rh22 + 2) as usize;
        let area_focused = self.main_has_focus() && self.focus == Focus::Search;
        let sel = self.srch.sel;
        let replace = self.srch.show_replace;
        let rep_label: String = self.srch.replace_text.chars().map(|c| if c == '\t' { ' ' } else { c }).collect();
        let m = self.hits.len();
        let rows: Vec<(usize, SRow)> = self.srch.rows.iter().copied().enumerate().skip(first).take(n).collect();
        for (i, row) in rows {
            let y = r.top + i as i32 * rh22 - scroll;
            let rr = rect(r.left, y, r.right, y + rh22);
            let row_hit = Hit::SearchRow(i);
            let hovered = self.hover == row_hit || matches!(self.hover, Hit::SearchAct(k, _) if k == i);
            let t = &self.t;
            if sel == Some(i) {
                g.fill(&rr, if area_focused { t.sel } else { t.sel_inactive });
                if area_focused {
                    g.frame(&rr, t.focus);
                }
            } else if hovered {
                g.fill(&rr, t.hover);
            }
            self.hits.push((rr, row_hit));
            // hover actions on the right
            let acts: Vec<(u8, char)> = if hovered || sel == Some(i) {
                let mut v = Vec::new();
                if replace {
                    v.push((A_REPLACE, if matches!(row, SRow::File(_)) { '\u{E8EE}' } else { '\u{E8AB}' }));
                }
                v.push((A_DISMISS, '\u{E711}'));
                v
            } else {
                Vec::new()
            };
            let bw = self.s(22);
            let mut ax = rr.right - self.s(8) - bw;
            let mut acts_left = rr.right - self.s(8);
            for (a, glyph) in acts.iter().rev() {
                let ar = xywh(ax, y, bw, rh22);
                let hit = Hit::SearchAct(i, *a);
                if self.hover == hit {
                    g.round_rect(&rect(ar.left, ar.top + self.s(1), ar.right, ar.bottom - self.s(1)), self.s(4), self.t.toolbar_hover, None);
                }
                g.icon(&ar, *glyph, self.fonts.icon_small, self.t.fg);
                self.hits.push((ar, hit));
                acts_left = ax;
                ax -= bw;
            }
            let t = &self.t;
            match row {
                SRow::File(fi) => {
                    let f = &self.srch.files[fi];
                    let open = !self.srch.collapsed.contains(&f.path);
                    let x0 = rr.left + self.s(8);
                    g.icon(&xywh(x0, y, self.s(16), rh22), if open { '\u{E70D}' } else { '\u{E76C}' }, self.fonts.icon_small, t.fg);
                    g.icon(&xywh(x0 + self.s(18), y, self.s(16), rh22), '\u{E8A5}', self.fonts.icon_small, t.fg_dim);
                    // the match count badge, then the name and the folder (dimmed)
                    let count = f.matches.len().to_string();
                    let cw = g.text_w(&count, self.fonts.cap).max(self.s(8)) + self.s(10);
                    let badge_right = if acts.is_empty() { rr.right - self.s(10) } else { acts_left - self.s(4) };
                    let b = rect(badge_right - cw, y + self.s(3), badge_right, y + rh22 - self.s(3));
                    g.round_rect(&b, self.s(16), t.badge_bg, None);
                    g.text_in(&b, &count, self.fonts.cap, t.badge_fg, DT_LINE | windows_sys::Win32::Graphics::Gdi::DT_CENTER);
                    let (dir, name) = match f.path.rsplit_once('/') {
                        Some((d, n)) => (d, n),
                        None => ("", f.path.as_str()),
                    };
                    let tx = x0 + self.s(38);
                    let right = b.left - self.s(6);
                    let nw = g.text_w(name, self.fonts.ui);
                    g.text_in(&rect(tx, y, right, y + rh22), name, self.fonts.ui, t.fg, DT_LINE);
                    if !dir.is_empty() && tx + nw + self.s(8) < right {
                        g.text_in(&rect(tx + nw + self.s(6), y, right, y + rh22), dir, self.fonts.small, t.fg_dim, DT_LINE);
                    }
                }
                SRow::Match(fi, mi) => {
                    let mt = &self.srch.files[fi].matches[mi];
                    let x0 = rr.left + self.s(46);
                    let clip = rect(x0, y, if acts.is_empty() { rr.right - self.s(8) } else { acts_left - self.s(2) }, y + rh22);
                    let chars: Vec<char> = mt.preview.chars().collect();
                    let (hs, he) = (mt.hl.0.min(chars.len()), mt.hl.1.min(chars.len()));
                    let before: String = chars[..hs].iter().collect();
                    let mid: String = chars[hs..he].iter().collect();
                    let after: String = chars[he..].iter().collect();
                    let font = self.fonts.ui;
                    let th = g.text_height(&rr, "Ag", font, DT_LINE);
                    let ty = y + (rh22 - th) / 2;
                    let w16 = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
                    let mut x = x0;
                    g.text_clip(x, ty, &w16(&before), font, t.fg, &clip);
                    x += g.text_w(&before, font);
                    let mw = g.text_w(&mid, font);
                    let hl = rect(x, y + self.s(3), (x + mw).min(clip.right), y + rh22 - self.s(3));
                    if replace {
                        // the match struck through, then its replacement, like VS Code
                        if hl.right > hl.left {
                            g.fill(&hl, t.del_text);
                        }
                        g.text_clip(x, ty, &w16(&mid), font, t.fg, &clip);
                        if x < clip.right {
                            g.hline(x, (x + mw).min(clip.right), y + rh22 / 2, t.fg);
                        }
                        x += mw;
                        let rw_ = g.text_w(&rep_label, font);
                        let ins = rect(x, y + self.s(3), (x + rw_).min(clip.right), y + rh22 - self.s(3));
                        if ins.right > ins.left {
                            g.fill(&ins, t.ins_text);
                        }
                        g.text_clip(x, ty, &w16(&rep_label), font, t.fg, &clip);
                        x += rw_;
                    } else {
                        if hl.right > hl.left {
                            g.fill(&hl, t.find_match);
                        }
                        g.text_clip(x, ty, &w16(&mid), font, t.fg, &clip);
                        x += mw;
                    }
                    g.text_clip(x, ty, &w16(&after), font, t.fg, &clip);
                }
            }
        }
        self.clip_hits(m, r);
        g.unclip(saved);
        self.draw_scrollbar(g, Area::Search);
    }

    /// Context menu of result row `i`.
    pub fn search_menu(&mut self, i: usize, m: &mut crate::ui::Menu) -> Vec<win::MenuItem> {
        let mut v = Vec::new();
        let Some(row) = self.srch.rows.get(i).copied() else { return v };
        self.srch.sel = Some(i);
        self.focus = Focus::Search;
        let replace = self.srch.show_replace;
        match row {
            SRow::File(fi) => {
                let Some(f) = self.srch.files.get(fi) else { return v };
                let path = f.path.clone();
                let (p1, p2) = (path.clone(), path.clone());
                v.push(m.it("Open File", move |a| {
                    a.open_file_viewer(p1, true);
                    a.focus = Focus::Main;
                }));
                v.push(win::MenuItem::Sep);
                v.push(m.it_if("Replace All in File", replace, move |a| a.search_act(i, A_REPLACE)));
                v.push(m.it("Dismiss", move |a| a.search_dismiss(i)));
                v.push(win::MenuItem::Sep);
                let abs = self.folder().map(|r| r.join(&path).display().to_string().replace('/', "\\")).unwrap_or_default();
                v.push(m.it("Copy Path", move |a| win::set_clipboard(a.hwnd, &abs)));
                v.push(m.it("Copy Relative Path", move |a| win::set_clipboard(a.hwnd, &p2)));
            }
            SRow::Match(fi, mi) => {
                let text = self.srch.files.get(fi).and_then(|f| f.matches.get(mi)).map(|m| m.preview.trim_start_matches('\u{2026}').to_string()).unwrap_or_default();
                v.push(m.it("Open", move |a| a.search_activate(i, true)));
                v.push(win::MenuItem::Sep);
                v.push(m.it_if("Replace", replace, move |a| a.search_act(i, A_REPLACE)));
                v.push(m.it("Dismiss", move |a| a.search_dismiss(i)));
                v.push(win::MenuItem::Sep);
                v.push(m.it("Copy", move |a| win::set_clipboard(a.hwnd, &text)));
            }
        }
        v.push(win::MenuItem::Sep);
        v.push(m.it("Collapse All / Expand All", |a| a.search_collapse_all()));
        v.push(m.it("Clear Search Results", |a| a.search_clear()));
        v
    }
}

/// Replaces the kept matches of file `rel` on disk; the number replaced.
fn replace_on_disk(root: &Path, rel: &str, search: &mut find::Search, rep: &str, preserve: bool, keep: &mut dyn FnMut(Pos, Pos) -> bool) -> Result<usize, String> {
    let p = root.join(rel);
    let bytes = std::fs::read(&p).map_err(|e| format!("Failed to read '{rel}': {e}"))?;
    let Some((text, enc, lossless)) = editor::decode(&bytes) else { return Ok(0) };
    if !lossless {
        return Err(format!("'{rel}' was not changed: it has invalid UTF-8 / UTF-16 that saving would change."));
    }
    let (mut lines, crlf) = crate::explorer::split_lines(&text);
    let Some((first, old_end, new, n)) = find::replace_lines(search, &lines, rep, preserve, keep) else { return Ok(0) };
    editor::replace(&mut lines, (first, 0), old_end, &new);
    std::fs::write(&p, editor::encode(&lines, enc, crlf)).map_err(|e| format!("Failed to save '{rel}': {e}"))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, case: bool, word: bool, regex: bool) -> SQuery {
        SQuery { text: text.into(), case, word, regex }
    }

    #[test]
    fn preview_cuts_around_the_match() {
        let (p, hl) = preview("    let x = foo;", 12, 15);
        assert_eq!(p, "let x = foo;");
        assert_eq!(&p.chars().collect::<Vec<_>>()[hl.0..hl.1].iter().collect::<String>(), "foo");
        let long = format!("{}needle tail", "a".repeat(100));
        let (p, hl) = preview(&long, 100, 106);
        assert!(p.starts_with('\u{2026}'));
        assert_eq!(&p.chars().collect::<Vec<_>>()[hl.0..hl.1].iter().collect::<String>(), "needle");
    }

    #[test]
    fn searches_files_and_replaces_on_disk() {
        let root = std::env::temp_dir().join(format!("devdeck-test-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("a.txt"), "Foo foo\r\nbar FOO\r\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "fn foo() {}\nfoobar\n").unwrap();
        std::fs::write(root.join("bin.dat"), b"foo\0\x01\x02").unwrap();
        let current = AtomicU64::new(1);
        let none = HashMap::new();
        // case-insensitive: every "foo", binary files skipped
        let (files, capped) = run(&root, false, &q("foo", false, false, false), &none, 1, &current);
        assert!(!capped);
        let mut got: Vec<(String, usize)> = files.iter().map(|f| (f.path.clone(), f.matches.len())).collect();
        got.sort();
        assert_eq!(got, vec![("a.txt".to_string(), 3), ("src/b.rs".to_string(), 2)]);
        // whole word and match case
        let (files, _) = run(&root, false, &q("foo", true, true, false), &none, 1, &current);
        let n: usize = files.iter().map(|f| f.matches.len()).sum();
        assert_eq!(n, 2);
        // unsaved text of a tab is searched instead of the file
        let buffers: HashMap<String, Vec<String>> = [("a.txt".to_string(), vec!["nothing here".to_string()])].into();
        let (files, _) = run(&root, false, &q("foo", false, false, false), &buffers, 1, &current);
        assert!(files.iter().all(|f| f.path != "a.txt"));
        // replace only the kept matches, keeping CRLF line endings
        let (files, _) = run(&root, false, &q("foo", false, false, false), &none, 1, &current);
        let a = files.iter().find(|f| f.path == "a.txt").unwrap();
        let keep: HashSet<Range> = [a.matches[0].range, a.matches[2].range].into();
        let mut search = find::Search::new("foo", false, false, false).unwrap().unwrap();
        let n = replace_on_disk(&root, "a.txt", &mut search, "baz", true, &mut |s, e| keep.contains(&(s, e))).unwrap();
        assert_eq!(n, 2);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "Baz foo\r\nbar BAZ\r\n");
        // regular expression with a group
        let mut search = find::Search::new(r"fn (\w+)", true, false, true).unwrap().unwrap();
        let n = replace_on_disk(&root, "src/b.rs", &mut search, "pub fn $1", false, &mut |_, _| true).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read_to_string(root.join("src/b.rs")).unwrap(), "pub fn foo() {}\nfoobar\n");
        // an older search stops when a newer one starts
        let (files, _) = run(&root, false, &q("foo", false, false, false), &none, 1, &AtomicU64::new(2));
        assert!(files.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
