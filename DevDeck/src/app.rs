//! Application state, background jobs and git commands.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{HBRUSH, HDC, HBITMAP};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, SetTimer, KillTimer, WM_APP};

use crate::diff::{self, FileDiff, LineId, Row, Block};
use crate::git::{self, Commit, FileChange, GitError, GitResult, Group, Kind, RefInfo, RefKind, Stash, Status};
use crate::graph::{self, GraphRow};
use crate::settings::Settings;
use crate::theme::Theme;
use crate::watch::Watcher;
use crate::win::{self, Fonts};

/// Height of the title bar row (unscaled pixels).
pub const TITLE_H: i32 = 30;
pub const WM_JOB: u32 = WM_APP + 1;
pub const WM_FS: u32 = WM_APP + 2;
pub const WM_TERM: u32 = WM_APP + 4;
pub const TIMER_FS: usize = 1;
pub const TIMER_TOAST: usize = 2;
pub const TIMER_SPIN: usize = 3;
/// Hover delay before a tooltip shows.
pub const TIMER_TIP: usize = 4;
/// Typing pause before the Search view searches.
pub const TIMER_SEARCH: usize = 5;

pub type Then = Box<dyn FnOnce(&mut App) + Send>;
pub type OnErr = Box<dyn FnOnce(&mut App, &GitError) -> bool + Send>;

pub enum JobResult {
    Refresh { gen: u64, status: GitResult<Status>, refs: Vec<RefInfo>, stashes: Vec<Stash>, ignored: Option<HashSet<String>> },
    Log { gen: u64, commits: Vec<Commit>, more: bool, append: bool },
    View { gen: u64, view: MainView, keep_scroll: bool },
    /// Files of `root` for Quick Open (see `quickopen.rs`).
    Files { gen: u64, root: PathBuf, files: Vec<String> },
    /// `root`: the repository the op ran in; its callbacks are dropped when another one is open
    /// by the time it finishes (None = always run them).
    Op { title: String, root: Option<PathBuf>, result: GitResult<String>, then: Option<Then>, on_err: Option<OnErr> },
    /// Matches of the Search view (see `search.rs`).
    Search { gen: u64, query: crate::search::SQuery, files: Vec<crate::search::SFile>, capped: bool },
}

pub struct Repo {
    pub root: PathBuf,
    pub name: String,
    pub status: Status,
    pub refs: Vec<RefInfo>,
    pub stashes: Vec<Stash>,
    pub loaded: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CRow {
    Group(Group),
    /// A folder in tree view (index into `App::cdirs`).
    Dir(usize),
    File(usize),
}

/// A folder row of the Changes tree. `label` may span several path segments when
/// single-child folders are compacted ("src/app"), like VS Code's compact folders.
#[derive(Clone, Debug)]
pub struct DirRow {
    pub group: Group,
    pub path: String,
    pub label: String,
    pub depth: u8,
}

/// Folder tree of one group's files, used to emit the tree view rows.
#[derive(Default)]
struct DirNode {
    dirs: Vec<(String, DirNode)>,
    files: Vec<usize>,
}

impl DirNode {
    fn insert(&mut self, path: &str, file: usize) {
        match path.split_once('/') {
            Some((head, rest)) => {
                let pos = match self.dirs.iter().position(|(n, _)| n == head) {
                    Some(p) => p,
                    None => {
                        self.dirs.push((head.to_string(), DirNode::default()));
                        self.dirs.len() - 1
                    }
                };
                self.dirs[pos].1.insert(rest, file);
            }
            None => self.files.push(file),
        }
    }

    /// Appends folders (sorted, compacted) and then files (sorted by name) as rows.
    #[allow(clippy::too_many_arguments)]
    fn emit(&self, group: Group, parent: &str, depth: u8, files: &[FileChange], collapsed: &HashSet<(u8, String)>, rows: &mut Vec<CRow>, depths: &mut Vec<u8>, dirs: &mut Vec<DirRow>) {
        let mut subs: Vec<&(String, DirNode)> = self.dirs.iter().collect();
        subs.sort_by_key(|(n, _)| n.to_lowercase());
        for (name, first) in subs {
            let mut label = name.clone();
            let mut node = first;
            while node.files.is_empty() && node.dirs.len() == 1 {
                let (n, child) = &node.dirs[0];
                label.push('/');
                label.push_str(n);
                node = child;
            }
            let path = if parent.is_empty() { label.clone() } else { format!("{parent}/{label}") };
            let open = !collapsed.contains(&(group_id(group), path.clone()));
            dirs.push(DirRow { group, path: path.clone(), label, depth });
            rows.push(CRow::Dir(dirs.len() - 1));
            depths.push(depth);
            if open {
                node.emit(group, &path, depth.saturating_add(1), files, collapsed, rows, depths, dirs);
            }
        }
        let mut fs = self.files.clone();
        fs.sort_by_key(|&i| short_name(&files[i].path).to_lowercase());
        for i in fs {
            rows.push(CRow::File(i));
            depths.push(depth);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffKind {
    WorkingTree,
    Index,
    Untracked,
    Commit,
}

pub struct DiffView {
    pub title: String,
    pub desc: String,
    pub path: String,
    pub kind: DiffKind,
    pub change: Option<FileChange>,
    pub fd: FileDiff,
    pub rows: Vec<Row>,
    pub blocks: Vec<Block>,
    pub sel: Option<(usize, usize)>,
    pub back: Option<Box<MainView>>,
    pub max_cols: usize,
    /// Rows were built for the inline (true) or side-by-side layout.
    pub inline: bool,
    /// First row of the change F7 / Shift+F7 went to last.
    pub nav: Option<usize>,
    /// Text selected on one side (copied with Ctrl+C), as in an editor.
    pub tsel: Option<TextSel>,
    /// Identifies the diff for word wrap's visual rows.
    pub uid: crate::editor::Uid,
}

/// A text selection in a diff: `side` 0 is the original (or the inline column), 1 the
/// modified side; positions are (row, char column).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TextSel {
    pub side: u8,
    pub anchor: (usize, usize),
    pub cur: (usize, usize),
}

impl DiffView {
    /// The line shown on `side` of `row` (none for filler rows and hunk separators).
    pub fn side_line(&self, row: usize, side: u8) -> Option<&diff::DLine> {
        let r = self.rows.get(row)?;
        let (hi, li) = if side == 0 || self.inline { r.left? } else { r.right? };
        self.fd.hunks.get(hi)?.lines.get(li)
    }

    /// The selected text of one side, lines joined with CRLF (None when nothing is selected).
    pub fn selected_text(&self) -> Option<String> {
        let t = self.tsel.filter(|t| t.anchor != t.cur)?;
        let (a, b) = (t.anchor.min(t.cur), t.anchor.max(t.cur));
        let mut parts = Vec::new();
        for row in a.0..=b.0.min(self.rows.len().saturating_sub(1)) {
            let Some(l) = self.side_line(row, t.side) else { continue };
            let chars: Vec<char> = l.text.trim_end_matches('\r').chars().collect();
            let from = if row == a.0 { a.1.min(chars.len()) } else { 0 };
            let to = if row == b.0 { b.1.min(chars.len()) } else { chars.len() };
            parts.push(chars[from..to.max(from)].iter().collect::<String>());
        }
        Some(parts.join("\r\n"))
    }
}

pub struct FileView {
    pub title: String,
    pub path: String,
    pub lines: Vec<String>,
    pub conflicts: Vec<diff::Conflict>,
    pub change: Option<FileChange>,
    /// Modification time when loaded (text viewer: reload when the file changes).
    pub mtime: Option<std::time::SystemTime>,
    /// Caret, selection, undo history and save format of an editable file.
    pub ed: crate::editor::EditState,
    /// Row of the conflict F7 / Shift+F7 went to last.
    pub nav: Option<usize>,
}

pub struct CommitView {
    pub commit: Commit,
    pub files: Vec<FileChange>,
    pub parent: Option<String>,
    pub sel: Option<usize>,
    pub stash: Option<usize>,
}

pub enum MainView {
    Empty,
    Loading(String),
    Message(String, String),
    Diff(DiffView),
    File(FileView),
    Commit(CommitView),
    /// A terminal shown in an editor tab (by `TermSession::id`).
    Terminal(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Area {
    Changes = 0,
    Graph = 1,
    Main = 2,
    Output = 3,
    Pick = 4,
    Branches = 5,
    Explorer = 6,
    Search = 7,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Explorer,
    Changes,
    Branches,
    Graph,
    Main,
    Terminal,
    /// The Search view's results (its boxes are edit controls with their own focus).
    Search,
}

pub struct PickItem {
    pub label: String,
    pub desc: String,
    pub icon: char,
    pub data: String,
    pub sep: String,
}

pub fn pi(label: impl Into<String>, desc: impl Into<String>, icon: char, data: impl Into<String>) -> PickItem {
    PickItem { label: label.into(), desc: desc.into(), icon, data: data.into(), sep: String::new() }
}

pub enum OverlayKind {
    Pick { items: Vec<PickItem>, filtered: Vec<usize>, sel: usize, on_pick: Option<Box<dyn FnOnce(&mut App, &PickItem)>> },
    Input { prompt: String, on_ok: Option<Box<dyn FnOnce(&mut App, String)>> },
    Confirm { message: String, detail: String, buttons: Vec<String>, sel: usize, on_choice: Option<Box<dyn FnOnce(&mut App, usize)>> },
}

pub struct Overlay {
    pub kind: OverlayKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Warning,
    Error,
}

pub struct Toast {
    pub kind: ToastKind,
    pub message: String,
    pub expires: std::time::Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sash {
    Sidebar,
    Graph,
    Branches,
    Output,
}

/// Size of the bottom panel (Output / Terminal).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelMode {
    /// Below the main area, resizable.
    Normal = 0,
    /// Spans the whole window width, under the activity bar and side bar too.
    FullWidth = 1,
    /// Takes the whole main area height (VS Code's "Maximize Panel Size").
    FullHeight = 2,
    /// Covers everything except the status bar.
    FullScreen = 3,
}

impl PanelMode {
    pub fn from_u8(v: u8) -> PanelMode {
        match v {
            1 => PanelMode::FullWidth,
            2 => PanelMode::FullHeight,
            3 => PanelMode::FullScreen,
            _ => PanelMode::Normal,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    None,
    Backdrop,
    Sash(Sash),
    Cmd(Cmd),
    GroupRow(usize),
    GroupAct(usize, u8),
    DirRow(usize),
    DirAct(usize, u8),
    FileRow(usize),
    FileAct(usize, u8),
    CommitBtn,
    CommitDrop,
    ActionBtn,
    GraphRow(usize),
    GraphMore,
    BranchRow(usize),
    BranchesHeader,
    GraphHeader,
    PanelMenu,
    Tab(usize),
    TabClose(usize),
    ExplorerRow(usize),
    ExplorerHeader,
    /// A button or input box of the Search view (`search::S_*`).
    Search(u8),
    SearchRow(usize),
    /// An action button of a Search result row (`search::A_*`).
    SearchAct(usize, u8),
    ActivityView(u8),
    DiffRow(usize),
    DiffBlock(usize, u8),
    DiffBack,
    /// The text of an editable file.
    EditorText,
    /// The horizontal scroll bar of the editor / diff: its thumb, and the track around it.
    HThumb,
    HTrack,
    /// A button or input box of the find widget (`find::F_*`).
    Find(u8),
    /// The find widget's background.
    FindBox,
    CommitFile(usize),
    Conflict(usize, u8),
    Thumb(Area),
    PickRow(usize),
    DialogBtn(usize),
    ToastClose(usize),
    ToastOutput,
    StatusBranch,
    StatusSync,
    Recent(usize),
    Welcome(u8),
    PanelTab(u8),
    TermBody,
    /// The terminal in the editor area.
    EditorTerm,
    TermShellMenu,
    /// Trash button of the panel's toolbar: kills the panel's active terminal only.
    TermKillPanel,
    TermTab(usize),
    TermTabKill(usize),
    /// Minimize / maximize (restore) / close buttons drawn in the title bar.
    CaptionMin,
    CaptionMax,
    CaptionClose,
}

pub enum Drag {
    Sash(Sash, i32, i32),
    Thumb(Area, i32, i32),
    /// Dragging the horizontal scroll bar's thumb: the x where it started, hscroll then.
    HThumb(i32, i32),
    DiffSelect,
    TermSelect,
    EditSelect,
    /// Dragging editor tab `.0` to reorder it; `.1` is the x where the drag started, `.2`
    /// whether the mouse moved far enough to count as a drag.
    Tab(usize, i32, bool),
}

#[derive(Clone, Copy)]
pub struct Layout {
    /// Title bar row (the window has no native caption): app icon and title, the editor
    /// tabs over the main area, and the caption buttons on the right.
    pub titlebar: RECT,
    pub captions: RECT,
    pub activity: RECT,
    pub sidebar: RECT,
    pub title: RECT,
    pub changes_header: RECT,
    pub explorer_header: RECT,
    pub explorer: RECT,
    /// The Search view's results list.
    pub search: RECT,
    pub edit: RECT,
    pub button: RECT,
    pub changes: RECT,
    pub branches_header: RECT,
    pub branches: RECT,
    pub graph_header: RECT,
    pub graph: RECT,
    pub main: RECT,
    pub main_header: RECT,
    pub main_body: RECT,
    pub output: RECT,
    pub output_body: RECT,
    pub status: RECT,
}

impl Default for Layout {
    fn default() -> Self {
        let z = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        Layout { titlebar: z, captions: z, activity: z, sidebar: z, title: z, changes_header: z, explorer_header: z, explorer: z, search: z, edit: z, button: z, changes: z, branches_header: z, branches: z, graph_header: z, graph: z, main: z, main_header: z, main_body: z, output: z, output_body: z, status: z }
    }
}

macro_rules! cmds {
    ($($name:ident => $title:expr, $key:expr;)*) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Cmd { $($name),* }
        pub const COMMANDS: &[(Cmd, &str, &str)] = &[$((Cmd::$name, $title, $key)),*];

        /// Keyboard shortcut of a command ("" when it has none).
        pub fn cmd_key(c: Cmd) -> &'static str {
            match c { $(Cmd::$name => $key),* }
        }
    };
}

cmds! {
    Palette => "Show All Commands", "Ctrl+Shift+P";
    Refresh => "Git: Refresh", "F5";
    Commit => "Git: Commit", "Ctrl+Enter";
    CommitAmend => "Git: Commit (Amend)", "";
    CommitAll => "Git: Commit All", "Ctrl+Shift+Enter";
    CommitPush => "Git: Commit & Push", "";
    CommitSync => "Git: Commit & Sync", "";
    UndoCommit => "Git: Undo Last Commit", "";
    StageAll => "Git: Stage All Changes", "";
    UnstageAll => "Git: Unstage All Changes", "";
    DiscardAll => "Git: Discard All Changes", "";
    Checkout => "Git: Checkout to...", "";
    Branch => "Git: Create Branch...", "";
    BranchFrom => "Git: Create Branch From...", "";
    RenameBranch => "Git: Rename Branch...", "";
    DeleteBranch => "Git: Delete Branch...", "";
    Merge => "Git: Merge...", "";
    Rebase => "Git: Rebase Branch...", "";
    MergeAbort => "Git: Abort Merge", "";
    RebaseAbort => "Git: Abort Rebase", "";
    RebaseContinue => "Git: Continue Rebase", "";
    Fetch => "Git: Fetch", "";
    FetchPrune => "Git: Fetch (Prune)", "";
    FetchAll => "Git: Fetch From All Remotes", "";
    Pull => "Git: Pull", "";
    PullRebase => "Git: Pull (Rebase)", "";
    Push => "Git: Push", "";
    PushForce => "Git: Push (Force With Lease)", "";
    PushTags => "Git: Push Tags", "";
    Sync => "Git: Sync", "";
    Publish => "Git: Publish Branch...", "";
    AddRemote => "Git: Add Remote...", "";
    RemoveRemote => "Git: Remove Remote", "";
    Stash => "Git: Stash", "";
    StashUntracked => "Git: Stash (Include Untracked)", "";
    StashStaged => "Git: Stash Staged", "";
    StashPop => "Git: Pop Latest Stash", "";
    StashPopPick => "Git: Pop Stash...", "";
    StashApply => "Git: Apply Stash...", "";
    StashDrop => "Git: Drop Stash...", "";
    StashView => "Git: View Stash...", "";
    TagCreate => "Git: Create Tag", "";
    TagDelete => "Git: Delete Tag", "";
    CherryPick => "Git: Cherry Pick...", "";
    ConfigureUser => "Git: Configure User Name and Email", "";
    OpenFolder => "File: Open Folder...", "Ctrl+O";
    OpenRecent => "File: Open Recent...", "Ctrl+R";
    NewWindow => "File: New Window", "Ctrl+Shift+N";
    Clone => "Git: Clone", "";
    Init => "Git: Initialize Repository", "";
    OpenExplorer => "File: Reveal Repository in File Explorer", "";
    RevealInExplorer => "File: Reveal Active File in Explorer View", "Alt+Shift+R";
    OpenTerminal => "Terminal: Open External Terminal", "Ctrl+Shift+C";
    ToggleTerminal => "View: Toggle Terminal", "Ctrl+`";
    NewTerminal => "Terminal: Create New Terminal", "Ctrl+Alt+`";
    ToggleEditorTerminal => "View: Toggle Terminal in Editor Area", "Ctrl+Shift+`";
    NewTerminalEditor => "Terminal: Create New Terminal in Editor Area", "";
    MoveTerminalToEditor => "Terminal: Move Terminal into Editor Area", "";
    MoveTerminalToPanel => "Terminal: Move Terminal into Panel", "";
    KillTerminal => "Terminal: Kill the Active Terminal Instance", "";
    FocusNextTerminal => "Terminal: Focus Next Terminal", "Ctrl+PageDown";
    FocusPrevTerminal => "Terminal: Focus Previous Terminal", "Ctrl+PageUp";
    RenameTerminal => "Terminal: Rename...", "";
    SelectShell => "Terminal: Select Default Profile", "";
    ClearTerminal => "Terminal: Clear", "";
    ClaudeOpen => "Claude Code: Open", "";
    ClaudeNew => "Claude Code: New Session", "Ctrl+Shift+A";
    CodexOpen => "Codex: Open", "";
    CodexNew => "Codex: New Session", "Ctrl+Shift+O";
    ToggleOutput => "View: Toggle Output", "Ctrl+J";
    CloseEditor => "View: Close Editor", "Ctrl+W";
    CloseOtherEditors => "View: Close Other Editors", "";
    CloseAllEditors => "View: Close All Editors", "";
    KeepEditor => "View: Keep Editor", "";
    PinEditor => "View: Pin Editor", "";
    UnpinEditor => "View: Unpin Editor", "";
    NextEditor => "View: Open Next Editor", "Ctrl+Tab";
    PrevEditor => "View: Open Previous Editor", "Ctrl+Shift+Tab";
    TogglePanelMax => "View: Toggle Maximized Panel", "Ctrl+Shift+M";
    PanelRestore => "View: Restore Panel Size", "";
    PanelFullWidth => "View: Panel Full Width", "";
    PanelFullHeight => "View: Panel Full Height", "";
    PanelFullScreen => "View: Panel Full Screen", "";
    ToggleGraphSection => "Source Control: Toggle Graph Section", "";
    ToggleBranchesSection => "Source Control: Toggle Branches Section", "";
    ToggleTheme => "Preferences: Toggle Light/Dark Theme", "";
    ToggleSidebar => "View: Toggle Primary Side Bar Visibility", "Ctrl+B";
    ShowExplorer => "View: Show Explorer", "Ctrl+Shift+E";
    ShowScm => "View: Show Source Control", "";
    FindInFiles => "Search: Find in Files", "Ctrl+Shift+F";
    ReplaceInFiles => "Search: Replace in Files", "Ctrl+Shift+H";
    ClearSearch => "Search: Clear Search Results", "";
    GotoFile => "Go to File...", "Ctrl+P";
    GotoLine => "Go to Line/Column...", "Ctrl+G";
    Find => "Find", "Ctrl+F";
    Replace => "Replace", "Ctrl+H";
    FindNext => "Find Next", "F3";
    FindPrev => "Find Previous", "Shift+F3";
    FindNextSel => "Find Next Selection", "Ctrl+F3";
    FindPrevSel => "Find Previous Selection", "Ctrl+Shift+F3";
    ReplaceAll => "Replace All", "Ctrl+Alt+Enter";
    Save => "File: Save", "Ctrl+S";
    SaveAll => "File: Save All", "";
    RevertFile => "File: Revert File", "";
    ExplorerNewFile => "Explorer: New File...", "";
    ExplorerNewFolder => "Explorer: New Folder...", "";
    ExplorerRename => "Explorer: Rename...", "F2";
    ExplorerDelete => "Explorer: Delete", "Del";
    ExplorerCollapse => "Explorer: Collapse Folders in Explorer", "";
    ToggleSidebarPos => "View: Toggle Primary Side Bar Position", "";
    ToggleTreeView => "Source Control: Toggle View as Tree / List", "";
    ToggleInline => "Diff: Toggle Inline View", "";
    ToggleCompact => "Diff: Toggle Collapse Unchanged Regions", "";
    ToggleWordWrap => "View: Toggle Word Wrap", "Alt+Z";
    ToggleGraphAll => "Graph: Toggle Show All Branches", "";
    NextChange => "Diff: Go to Next Change", "F7";
    PrevChange => "Diff: Go to Previous Change", "Shift+F7";
    FocusCommit => "Source Control: Focus Commit Message", "Ctrl+Shift+G";
    ClearOutput => "Output: Clear", "";
    About => "Help: About", "";
}

pub struct App {
    pub hwnd: HWND,
    pub dpi: u32,
    pub scale: f32,
    pub fonts: Fonts,
    pub t: Theme,
    pub st: Settings,
    pub tx: Sender<JobResult>,
    pub rx: Receiver<JobResult>,
    pub op_tx: Sender<Box<dyn FnOnce() + Send>>,
    pub repo: Option<Repo>,
    /// The work tree, plus the git dir when it is kept elsewhere (linked worktree, submodule).
    pub watcher: Vec<Watcher>,
    pub refresh_gen: u64,
    pub refreshing: bool,
    pub refresh_pending: bool,
    pub log_gen: u64,
    pub commits: Vec<Commit>,
    pub graph_rows: Vec<GraphRow>,
    pub has_more: bool,
    pub log_loading: bool,
    pub log_key: String,
    pub crows: Vec<CRow>,
    /// Indent level of each row in `crows` (tree view).
    pub crow_depth: Vec<u8>,
    pub cdirs: Vec<DirRow>,
    pub collapsed: [bool; 3],
    /// Collapsed tree folders: (group id, folder path).
    pub collapsed_dirs: HashSet<(u8, String)>,
    pub sel: HashSet<(u8, String)>,
    /// Rows of the Branches view.
    pub brows: Vec<crate::branches::BRow>,
    /// Explorer view state.
    pub ex: crate::explorer::Explorer,
    /// Selected Branches row, by key (see `brow_key`).
    pub branch_sel: Option<String>,
    /// Collapsed Branches groups ("local", "remote:<name>", "tags").
    pub collapsed_bgroups: HashSet<String>,
    pub focus_row: Option<usize>,
    pub anchor_row: Option<usize>,
    pub scroll: [i32; 8],
    pub hscroll: i32,
    pub view: MainView,
    /// Editor tabs; `view` holds the active tab's view.
    pub tabs: Vec<crate::tabs::EditorTab>,
    pub tab_active: usize,
    pub view_gen: u64,
    pub ops: Vec<String>,
    pub spin: u32,
    pub overlay: Option<Overlay>,
    pub toasts: Vec<Toast>,
    pub hits: Vec<(RECT, Hit)>,
    pub hover: Hit,
    /// Link under the pointer while Ctrl is held (underlined; Ctrl+click opens it).
    pub link: Option<crate::link::Link>,
    pub drag: Option<Drag>,
    pub focus: Focus,
    pub commit_edit: HWND,
    /// MERGE_MSG put into the commit box, to take it back out when the merge is aborted.
    pub merge_prefill: Option<String>,
    pub overlay_edit: HWND,
    /// The commit box when it had the keyboard as an overlay opened: it gets it back after.
    pub overlay_prev_focus: HWND,
    /// The commit box or quick pick that had the keyboard when the window was deactivated.
    pub saved_focus: HWND,
    /// What the open quick pick is (Go to File filters and paints its own way).
    pub pick_mode: crate::quickopen::PickMode,
    /// Quick Open (Go to File) state: the file list, the recently opened files.
    pub qo: crate::quickopen::QuickOpen,
    /// Lowercased search text of each quick pick item and the length of its name part, made
    /// once when the pick opens instead of on every keystroke.
    pub pick_hay: Vec<(String, usize)>,
    pub edit_brush: HBRUSH,
    pub edit_lines: i32,
    /// Area the open overlay covers (client coordinates), recorded while painting it.
    pub overlay_bounds: Option<RECT>,
    /// Edit control and overlay rectangles the window region of the commit box, the find box
    /// and the replace box was last cut for (None = unclipped).
    pub edit_clip: [Option<[i32; 8]>; 5],
    /// Find / Replace widget of file views.
    pub find: crate::find::Find,
    /// Search view (find / replace in files).
    pub srch: crate::search::SearchView,
    /// Visual rows of the active file or diff with word wrap.
    pub wrap: Option<crate::wrap::WrapMap>,
    /// Columns of the widest line of a file view, for the horizontal scroll bar: (view id,
    /// text version, columns).
    pub file_cols: std::cell::Cell<(u64, u64, usize)>,
    pub l: Layout,
    pub graph_sel: Option<usize>,
    pub git_version: Option<String>,
    pub buf: (HDC, HBITMAP, i32, i32),
    pub output_seen: usize,
    pub mouse: (i32, i32),
    /// A wheel scroll moved the rows under the pointer: hover is recomputed after the next paint.
    pub hover_stale: bool,
    /// Tooltip shown: the hovered item and the mouse position it was shown at.
    pub tip: Option<(Hit, i32, i32)>,
    /// Where the tooltip was painted (the commit box is cut out under it).
    pub tip_bounds: Option<RECT>,
    pub pending_init: Option<PathBuf>,
    pub pending_file: Option<PathBuf>,
    /// Terminal instances, in the panel or in editor tabs; `term_active` is the one shown
    /// in the panel.
    pub terms: Vec<crate::term_ui::TermSession>,
    pub term_active: usize,
    pub term_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// With `Focus::Terminal`: input goes to the terminal in the active editor tab, not the panel's.
    pub term_editor_focus: bool,
    pub term_next_id: u64,
    /// Terminal tab used last, which Ctrl+Shift+` brings back.
    pub term_editor_last: Option<u64>,
    /// Key of the tab that was active before a terminal tab was shown, which Ctrl+Shift+`
    /// returns to.
    pub term_editor_prev: Option<String>,
    /// Caption button (HTMINBUTTON / HTMAXBUTTON / HTCLOSE) pressed in the title bar, 0 = none.
    pub caption_down: u32,
    /// App icon for the title bar and its size in pixels.
    pub title_icon: (windows_sys::Win32::UI::WindowsAndMessaging::HICON, i32),
    pub term_surrogate: u16,
    pub shells: Vec<crate::term::Shell>,
}

/// `<remote> <branch>:<upstream branch>` for pushing the current branch, like VS Code: a bare
/// `git push` fails (push.default=simple) when the upstream branch has another name.
fn push_target(s: &Status) -> Vec<String> {
    match (&s.branch, &s.upstream) {
        (Some(b), Some(up)) => {
            let (remote, ub) = crate::branches::split_remote(&s.remotes, up);
            if remote.is_empty() {
                vec![]
            } else {
                vec![remote, format!("{b}:{ub}")]
            }
        }
        _ => vec![],
    }
}

pub fn group_id(g: Group) -> u8 {
    match g {
        Group::Merge => 0,
        Group::Staged => 1,
        Group::Changes => 2,
    }
}

pub fn group_label(g: Group) -> &'static str {
    match g {
        Group::Merge => "Merge Changes",
        Group::Staged => "Staged Changes",
        Group::Changes => "Changes",
    }
}

pub fn post(hwnd: isize) {
    unsafe {
        PostMessageW(hwnd as HWND, WM_JOB, 0, 0);
    }
}

impl App {
    pub fn new(hwnd: HWND, dpi: u32, st: Settings) -> App {
        let (tx, rx) = channel();
        let (op_tx, op_rx) = channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name("git-ops".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                for job in op_rx {
                    job();
                }
            })
            .unwrap();
        let scale = dpi as f32 / 96.0;
        let t = if st.dark { Theme::dark() } else { Theme::light() };
        App {
            hwnd,
            dpi,
            scale,
            fonts: Fonts::new(scale),
            t,
            st,
            tx,
            rx,
            op_tx,
            repo: None,
            watcher: Vec::new(),
            refresh_gen: 0,
            refreshing: false,
            refresh_pending: false,
            log_gen: 0,
            commits: Vec::new(),
            graph_rows: Vec::new(),
            has_more: false,
            log_loading: false,
            log_key: String::new(),
            crows: Vec::new(),
            crow_depth: Vec::new(),
            cdirs: Vec::new(),
            collapsed: [false; 3],
            collapsed_dirs: HashSet::new(),
            sel: HashSet::new(),
            focus_row: None,
            anchor_row: None,
            scroll: [0; 8],
            hscroll: 0,
            view: MainView::Empty,
            tabs: Vec::new(),
            tab_active: 0,
            view_gen: 0,
            ops: Vec::new(),
            spin: 0,
            overlay: None,
            toasts: Vec::new(),
            hits: Vec::new(),
            hover: Hit::None,
            link: None,
            tip: None,
            tip_bounds: None,
            drag: None,
            focus: Focus::Changes,
            commit_edit: std::ptr::null_mut(),
            merge_prefill: None,
            overlay_edit: std::ptr::null_mut(),
            overlay_prev_focus: std::ptr::null_mut(),
            saved_focus: std::ptr::null_mut(),
            pick_mode: Default::default(),
            qo: Default::default(),
            pick_hay: Vec::new(),
            edit_brush: std::ptr::null_mut(),
            edit_lines: 1,
            overlay_bounds: None,
            edit_clip: [None; 5],
            find: crate::find::Find::new(),
            srch: Default::default(),
            wrap: None,
            file_cols: std::cell::Cell::new((0, 0, 0)),
            l: Layout::default(),
            graph_sel: None,
            brows: Vec::new(),
            ex: crate::explorer::Explorer::default(),
            branch_sel: None,
            collapsed_bgroups: ["tags".to_string()].into_iter().collect(),
            git_version: None,
            buf: (std::ptr::null_mut(), std::ptr::null_mut(), 0, 0),
            output_seen: 0,
            mouse: (0, 0),
            hover_stale: false,
            pending_init: None,
            pending_file: None,
            terms: Vec::new(),
            term_active: 0,
            term_pending: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            term_editor_focus: false,
            term_next_id: 1,
            term_editor_last: None,
            term_editor_prev: None,
            caption_down: 0,
            title_icon: (std::ptr::null_mut(), 0),
            term_surrogate: 0,
            shells: Vec::new(),
        }
    }

    pub fn s(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }

    pub fn invalidate(&self) {
        unsafe {
            windows_sys::Win32::Graphics::Gdi::InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    // ------------------------------------------------------------- toasts

    pub fn toast(&mut self, kind: ToastKind, msg: impl Into<String>) {
        let secs = if kind == ToastKind::Error { 12 } else { 6 };
        self.toasts.push(Toast { kind, message: msg.into(), expires: std::time::Instant::now() + std::time::Duration::from_secs(secs) });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        unsafe {
            SetTimer(self.hwnd, TIMER_TOAST, 500, None);
        }
        self.invalidate();
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.toast(ToastKind::Info, msg);
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        self.toast(ToastKind::Warning, msg);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.toast(ToastKind::Error, msg);
    }

    pub fn tick_toasts(&mut self) {
        let now = std::time::Instant::now();
        let n = self.toasts.len();
        self.toasts.retain(|t| t.expires > now);
        if self.toasts.is_empty() {
            unsafe {
                KillTimer(self.hwnd, TIMER_TOAST);
            }
        }
        if n != self.toasts.len() {
            self.invalidate();
        }
    }

    // --------------------------------------------------------------- repo

    pub fn open_repo(&mut self, folder: &Path) {
        // a recent folder that was deleted or renamed: drop it from the list, like VS Code
        if !folder.is_dir() {
            self.st.remove_recent(&folder.display().to_string());
            self.st.save();
            self.error(format!("The folder '{}' does not exist anymore.", folder.display()));
            return;
        }
        // the open tabs go away: ask about unsaved files first
        let dirty = self.dirty_tab_keys();
        if !dirty.is_empty() {
            let f = folder.to_path_buf();
            self.confirm_unsaved(dirty, move |a| {
                a.open_repo(&f);
                a.layout();
            });
            return;
        }
        let Some(root) = git::toplevel(folder) else {
            let f = folder.display().to_string();
            self.repo = None;
            self.watcher.clear();
            self.tabs.clear();
            self.tab_active = 0;
            self.prune_editor_terms();
            self.sync_term_focus();
            self.view = MainView::Message("Not a Git repository".into(), format!("The folder '{f}' doesn't have a git repository. Use 'Git: Initialize Repository' to create one."));
            self.st.add_recent(&f);
            self.st.save();
            self.pending_init = Some(folder.to_path_buf());
            // the folder still opens, like VS Code: the Explorer, editor and terminal work on
            // it and Source Control offers to initialize a repository
            self.ex = crate::explorer::Explorer::default();
            self.watcher = Watcher::start(folder, self.hwnd, WM_FS, false).into_iter().collect();
            self.st.side_view = 1;
            self.st.sidebar_visible = true;
            self.rebuild_explorer();
            self.update_title();
            self.invalidate();
            return;
        };
        self.pending_init = None;
        let name = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string());
        self.st.add_recent(&root.display().to_string());
        self.st.save();
        self.watcher.clear();
        self.watcher = Watcher::start(&root, self.hwnd, WM_FS, false).into_iter().collect();
        // `.git` is a file pointing elsewhere: index, HEAD and refs change there
        if let Some(d) = std::fs::read_to_string(root.join(".git")).ok().and_then(|t| t.trim().strip_prefix("gitdir:").map(|d| root.join(d.trim()))) {
            if !d.starts_with(&root) {
                self.watcher.extend(Watcher::start(&d, self.hwnd, WM_FS, true));
            }
        }
        self.repo = Some(Repo { root, name, status: Status::default(), refs: vec![], stashes: vec![], loaded: false });
        self.commits.clear();
        self.graph_rows.clear();
        self.log_key.clear();
        self.sel.clear();
        self.crows.clear();
        self.scroll = [0; 8];
        self.ex = crate::explorer::Explorer::default();
        self.brows.clear();
        self.branch_sel = None;
        self.graph_sel = None;
        self.view = MainView::Empty;
        self.tabs.clear();
        self.tab_active = 0;
        self.prune_editor_terms();
        self.sync_term_focus();
        win::set_text(self.commit_edit, "");
        self.merge_prefill = None;
        // results of a status or log load still running for the previous repository are stale
        self.refresh_gen += 1;
        self.refreshing = false;
        self.refresh_pending = false;
        self.log_gen += 1;
        self.log_loading = false;
        self.refresh();
        self.update_title();
    }

    /// Opens a command line target (folder or file), like `code <path>`.
    pub fn open_target(&mut self, t: crate::cli::Target) {
        self.pending_file = t.file;
        self.open_repo(&t.folder);
    }

    /// Shows the changes of a file given on the command line.
    pub fn open_file_target(&mut self, file: &Path) {
        let Some(root) = self.root() else { return };
        let canon_root = std::fs::canonicalize(&root).ok();
        let canon_file = std::fs::canonicalize(file).ok();
        let rel = match (canon_root, canon_file) {
            (Some(r), Some(f)) => f.strip_prefix(&r).ok().map(|p| p.to_string_lossy().replace('\\', "/")),
            _ => None,
        };
        let Some(rel) = rel else { return };
        let found = self.repo.as_ref().and_then(|r| {
            let order = [Group::Merge, Group::Changes, Group::Staged];
            order.iter().find_map(|g| r.status.files.iter().find(|f| f.group == *g && f.path.eq_ignore_ascii_case(&rel)).cloned())
        });
        match found {
            Some(f) => {
                if let Some(i) = self.crows.iter().position(|r| matches!(r, CRow::File(x) if self.repo.as_ref().map(|rp| rp.status.files[*x].path == f.path && rp.status.files[*x].group == f.group).unwrap_or(false))) {
                    self.focus_row = Some(i);
                    self.sel.clear();
                    self.sel.insert((group_id(f.group), f.path.clone()));
                }
                self.open_change(f, true);
            }
            None => {
                self.open_tab(format!("msg:{rel}"), true);
                self.view = MainView::Message(short_name(&rel).to_string(), format!("'{rel}' has no changes."));
                self.invalidate();
            }
        }
    }

    pub fn root(&self) -> Option<PathBuf> {
        self.repo.as_ref().map(|r| r.root.clone())
    }

    /// The open folder: the repository's root, or a folder without a repository.
    pub fn folder(&self) -> Option<PathBuf> {
        self.root().or_else(|| self.pending_init.clone())
    }

    pub fn refresh(&mut self) {
        let Some(root) = self.root() else {
            // a folder without a repository: a repository created in it (e.g. `git init` in
            // the terminal) opens it, otherwise only the Explorer follows the changes
            if let Some(f) = self.pending_init.clone() {
                if f.join(".git").exists() && git::toplevel(&f).is_some() {
                    self.open_repo(&f);
                    self.layout();
                } else {
                    if self.explorer_active() {
                        self.rebuild_explorer();
                    }
                    // no status refresh follows here, so open files are reloaded directly
                    self.reload_text_view_if_changed();
                }
            }
            return;
        };
        if self.refreshing {
            self.refresh_pending = true;
            return;
        }
        self.refreshing = true;
        self.refresh_gen += 1;
        let gen = self.refresh_gen;
        let tx = self.tx.clone();
        let hwnd = self.hwnd as isize;
        // the Explorer dims ignored files; only look them up while it is shown
        let want_ignored = self.explorer_active();
        std::thread::spawn(move || {
            let (status, (refs, stashes), ignored) = std::thread::scope(|sc| {
                let a = sc.spawn(|| git::refs(&root));
                let b = sc.spawn(|| git::stashes(&root));
                let c = sc.spawn(|| if want_ignored { Some(crate::explorer::ignored(&root)) } else { None });
                let s = git::status(&root);
                (s, (a.join().unwrap_or_default(), b.join().unwrap_or_default()), c.join().unwrap_or_default())
            });
            let _ = tx.send(JobResult::Refresh { gen, status, refs, stashes, ignored });
            post(hwnd);
        });
    }

    pub fn on_jobs(&mut self) {
        while let Ok(r) = self.rx.try_recv() {
            match r {
                JobResult::Refresh { gen, status, refs, stashes, ignored } => {
                    if gen != self.refresh_gen {
                        continue;
                    }
                    self.refreshing = false;
                    if let Some(ig) = ignored {
                        self.ex.ignored = ig;
                    }
                    match status {
                        Ok(s) => self.apply_status(s, refs, stashes),
                        Err(e) => self.error(e.message),
                    }
                    if self.refresh_pending {
                        self.refresh_pending = false;
                        self.refresh();
                    }
                }
                JobResult::Log { gen, commits, more, append } => {
                    if gen != self.log_gen {
                        continue;
                    }
                    self.log_loading = false;
                    if append {
                        self.commits.extend(commits);
                    } else {
                        // the selected commit stays selected where it moved to (a new commit
                        // above it, Auto / All), or the selection goes
                        let sel = self.graph_sel.and_then(|i| self.commits.get(i)).map(|c| c.hash.clone());
                        self.graph_sel = sel.and_then(|h| commits.iter().position(|c| c.hash == h));
                        self.commits = commits;
                    }
                    self.has_more = more;
                    self.rebuild_graph();
                }
                JobResult::Files { gen, root, files } => self.on_files_listed(gen, root, files),
                JobResult::Search { gen, query, files, capped } => self.on_search_done(gen, query, files, capped),
                JobResult::View { gen, view, keep_scroll } => {
                    // route the result to the tab that asked for it (it may be in the background now)
                    let Some(ti) = self.tabs.iter().position(|t| t.gen == gen) else { continue };
                    // a line typed in Quick Open ("app.rs:120") for the file loading in this tab
                    let goto = self.take_pending_goto(gen);
                    if ti != self.tab_active {
                        let t = &mut self.tabs[ti];
                        // the tab was edited since the reload started: keep the edits
                        if matches!(&t.view, MainView::File(f) if f.ed.dirty()) {
                            continue;
                        }
                        if !keep_scroll {
                            t.scroll = 0;
                            t.hscroll = 0;
                        }
                        t.view = view;
                        continue;
                    }
                    let mut view = view;
                    // a reload never replaces unsaved edits, whatever it found (the file may be
                    // gone or too large now); Save asks before overwriting
                    if keep_scroll && matches!(&self.view, MainView::File(o) if o.ed.dirty()) {
                        continue;
                    }
                    if keep_scroll {
                        if let (MainView::File(old), MainView::File(new)) = (&self.view, &mut view) {
                            // the caret stays where it was
                            if old.path == new.path && old.ed.editable && new.ed.editable {
                                let clamp = |p: crate::editor::Pos| {
                                    let l = p.0.min(new.lines.len() - 1);
                                    (l, p.1.min(crate::editor::char_len(&new.lines[l])))
                                };
                                new.ed.cur = clamp(old.ed.cur);
                                new.ed.anchor = clamp(old.ed.anchor);
                            }
                        }
                    } else {
                        self.scroll[Area::Main as usize] = 0;
                        self.hscroll = 0;
                    }
                    let (prev_sel, prev_tsel) = if let MainView::Diff(d) = &self.view { (d.sel, d.tsel) } else { (None, None) };
                    self.view = view;
                    if keep_scroll {
                        if let MainView::Diff(d) = &mut self.view {
                            if let Some((a, b)) = prev_sel {
                                if a < d.rows.len() && b < d.rows.len() {
                                    d.sel = Some((a, b));
                                }
                            }
                            // the same diff again (a refresh): a text selection stays
                            if let Some(t) = prev_tsel {
                                if t.anchor.0.max(t.cur.0) < d.rows.len() {
                                    d.tsel = Some(t);
                                }
                            }
                        }
                    }
                    if let Some((l, c)) = goto {
                        self.goto_line(l, c);
                    }
                    // a match opened from the Search view
                    self.take_search_pending(gen);
                }
                JobResult::Op { title, root, result, then, on_err } => {
                    if let Some(i) = self.ops.iter().position(|t| *t == title) {
                        self.ops.remove(i);
                    }
                    // the follow-up of an op (push after commit, force checkout, ...) belongs to
                    // the repository it ran in, not to one opened since
                    let (then, on_err) = if root.is_some() && root != self.root() { (None, None) } else { (then, on_err) };
                    match result {
                        Ok(_) => {
                            if let Some(t) = then {
                                t(self);
                            }
                        }
                        Err(e) => {
                            let handled = match on_err {
                                Some(f) => f(self, &e),
                                None => false,
                            };
                            if !handled {
                                self.error(e.message.clone());
                            }
                        }
                    }
                    if self.ops.is_empty() {
                        unsafe {
                            KillTimer(self.hwnd, TIMER_SPIN);
                        }
                    }
                    self.refresh();
                }
            }
        }
        self.invalidate();
    }

    fn apply_status(&mut self, s: Status, refs: Vec<RefInfo>, stashes: Vec<Stash>) {
        let first = self.repo.as_ref().map(|r| !r.loaded).unwrap_or(false);
        let Some(repo) = self.repo.as_mut() else { return };
        // keep merge message in the commit box
        if let Some(m) = &s.merge_msg {
            // once per merge: a message the user cleared isn't put back on the next refresh
            if self.merge_prefill.is_none() && win::get_text(self.commit_edit).trim().is_empty() && !m.is_empty() {
                win::set_text(self.commit_edit, m);
                self.merge_prefill = Some(m.clone());
            }
        } else if let Some(m) = self.merge_prefill.take() {
            // merge/cherry-pick aborted: drop the prefilled message unless the user edited it
            if win::get_text(self.commit_edit).replace("\r\n", "\n").trim() == m {
                win::set_text(self.commit_edit, "");
            }
        }
        repo.status = s;
        unsafe {
            windows_sys::Win32::Graphics::Gdi::InvalidateRect(self.commit_edit, std::ptr::null(), 1);
        }
        repo.refs = refs;
        repo.stashes = stashes;
        repo.loaded = true;
        self.rebuild_changes();
        self.rebuild_branches();
        self.clamp_scroll(Area::Branches);
        if self.explorer_active() {
            self.rebuild_explorer();
        }
        self.reload_text_view_if_changed();
        let r = self.repo.as_ref().unwrap();
        let mut key = format!("{:?}|{:?}|{:?}|{}", r.status.head, r.status.branch, r.status.upstream, self.st.graph_all);
        for rf in &r.refs {
            key.push_str(&rf.name);
            key.push_str(&rf.sha[..rf.sha.len().min(8)]);
        }
        if key != self.log_key {
            self.log_key = key;
            self.load_log(false);
        }
        self.update_title();
        if first {
            if let Some(i) = self.crows.iter().position(|r| matches!(r, CRow::File(_))) {
                self.focus_row = Some(i);
            }
            if let Some(file) = self.pending_file.take() {
                self.open_file_target(&file);
                return;
            }
        }
        // live-update the open change diff
        let current = match &self.view {
            MainView::Diff(d) if d.kind != DiffKind::Commit && d.back.is_none() => d.change.clone(),
            MainView::File(f) => f.change.clone(),
            _ => None,
        };
        // unsaved edits in a conflict view are never replaced
        if let (Some(c), false) = (current, matches!(&self.view, MainView::File(f) if f.ed.dirty())) {
            let files = &self.repo.as_ref().unwrap().status.files;
            // a staged, unstaged or resolved file is followed to its new group, so the view
            // never shows changes that are no longer there
            let still = files.iter().find(|f| f.path == c.path && f.group == c.group).or_else(|| files.iter().find(|f| f.path == c.path)).cloned();
            match still {
                Some(f) => {
                    let key = format!("change:{}:{}", group_id(f.group), f.path);
                    if f.group != c.group && !self.tabs.iter().any(|t| t.key == key) {
                        if let Some(t) = self.tabs.get_mut(self.tab_active) {
                            t.key = key;
                        }
                    }
                    self.load_change_view(f, true);
                }
                // committed or discarded
                None => {
                    let (title, _) = crate::tabs::view_title(&self.view);
                    self.view = MainView::Message(title, "There are no changes in this file.".into());
                }
            }
        }
    }

    pub fn rebuild_changes(&mut self) {
        let Some(repo) = &self.repo else {
            self.crows.clear();
            self.crow_depth.clear();
            self.cdirs.clear();
            return;
        };
        let tree = self.st.tree_view;
        let mut rows = Vec::new();
        let mut depths = Vec::new();
        let mut dirs = Vec::new();
        for g in [Group::Merge, Group::Staged, Group::Changes] {
            let idx: Vec<usize> = repo.status.files.iter().enumerate().filter(|(_, f)| f.group == g).map(|(i, _)| i).collect();
            if idx.is_empty() && g != Group::Changes {
                continue;
            }
            rows.push(CRow::Group(g));
            depths.push(0);
            if self.collapsed[group_id(g) as usize] {
                continue;
            }
            if tree {
                let mut node = DirNode::default();
                for i in idx {
                    node.insert(&repo.status.files[i].path, i);
                }
                node.emit(g, "", 0, &repo.status.files, &self.collapsed_dirs, &mut rows, &mut depths, &mut dirs);
            } else {
                depths.extend(std::iter::repeat(0).take(idx.len()));
                rows.extend(idx.into_iter().map(CRow::File));
            }
        }
        let keys: HashSet<(u8, String)> = repo.status.files.iter().map(|f| (group_id(f.group), f.path.clone())).collect();
        self.sel.retain(|k| keys.contains(k));
        self.crows = rows;
        self.crow_depth = depths;
        self.cdirs = dirs;
        if let Some(f) = self.focus_row {
            if f >= self.crows.len() {
                self.focus_row = if self.crows.is_empty() { None } else { Some(self.crows.len() - 1) };
            }
        }
    }

    pub fn file(&self, idx: usize) -> Option<&FileChange> {
        self.repo.as_ref().and_then(|r| r.status.files.get(idx))
    }

    pub fn update_title(&self) {
        let t = win::wz(&self.window_title());
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(self.hwnd, t.as_ptr());
        }
        self.invalidate();
    }

    /// Window title: the repository (branch) or folder, then the app name.
    pub fn window_title(&self) -> String {
        match &self.repo {
            Some(r) => {
                let b = r.status.branch.clone().unwrap_or_else(|| r.status.head.as_deref().map(|h| h[..h.len().min(8)].to_string()).unwrap_or_default());
                format!("{} ({}) - DevDeck", r.name, b)
            }
            None => match &self.pending_init {
                Some(f) => format!("{} - DevDeck", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| f.display().to_string())),
                None => "DevDeck".to_string(),
            },
        }
    }

    // --------------------------------------------------------------- graph

    pub fn load_log(&mut self, append: bool) {
        // "load more" while a full reload runs would drop the reload and append to the old list
        if append && self.log_loading {
            return;
        }
        let Some(repo) = &self.repo else { return };
        if repo.status.head.is_none() {
            self.commits.clear();
            self.graph_rows.clear();
            self.has_more = false;
            // a load still running (history from before an undone first commit) is stale
            self.log_gen += 1;
            self.log_loading = false;
            return;
        }
        let root = repo.root.clone();
        let refs: Vec<String> = if self.st.graph_all {
            vec!["--branches".into(), "--remotes".into(), "--tags".into(), "HEAD".into()]
        } else {
            let mut v = vec!["HEAD".to_string()];
            if let Some(u) = &repo.status.upstream {
                v.push(format!("refs/remotes/{u}"));
            }
            v
        };
        let skip = if append { self.commits.len() } else { 0 };
        let limit = if append { 100 } else { self.commits.len().clamp(100, 1000) };
        self.log_gen += 1;
        let gen = self.log_gen;
        self.log_loading = true;
        let tx = self.tx.clone();
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let (commits, more) = git::log(&root, &refs, skip, limit);
            let _ = tx.send(JobResult::Log { gen, commits, more, append });
            post(hwnd);
        });
    }

    pub fn rebuild_graph(&mut self) {
        let (b, u, h) = match &self.repo {
            Some(r) => (r.status.branch.clone(), r.status.upstream.clone(), r.status.head.clone()),
            None => (None, None, None),
        };
        self.graph_rows = graph::compute(&self.commits, b.as_deref(), u.as_deref(), h.as_deref());
    }

    // ---------------------------------------------------------------- ops

    pub fn op<F>(&mut self, title: &str, f: F, then: Option<Then>, on_err: Option<OnErr>)
    where
        F: FnOnce(&Path) -> GitResult<String> + Send + 'static,
    {
        let Some(root) = self.root() else {
            self.info("There are no available repositories.");
            return;
        };
        let tx = self.tx.clone();
        let hwnd = self.hwnd as isize;
        let t = title.to_string();
        self.ops.push(t.clone());
        unsafe {
            SetTimer(self.hwnd, TIMER_SPIN, 120, None);
        }
        let _ = self.op_tx.send(Box::new(move || {
            let result = f(&root);
            let _ = tx.send(JobResult::Op { title: t, root: Some(root), result, then, on_err });
            post(hwnd);
        }));
        self.invalidate();
    }

    /// Simple op: runs one git command.
    pub fn git_op(&mut self, title: &str, args: Vec<String>) {
        self.op(title, move |root| {
            let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            git::exec(root, &a)
        }, None, None);
    }

    pub fn status(&self) -> Option<&Status> {
        self.repo.as_ref().map(|r| &r.status)
    }

    // ---------------------------------------------------------- overlays

    /// Like VS Code, cancelling an overlay gives the keyboard back to where it was: the commit
    /// box when it had it (a chain of overlays keeps the first one's).
    fn remember_focus(&mut self) {
        if self.overlay.is_none() {
            let f = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus() };
            let back = f == self.commit_edit || f == self.find.find_edit || f == self.find.repl_edit || f == self.srch.find_edit || f == self.srch.repl_edit;
            self.overlay_prev_focus = if back && !f.is_null() { f } else { std::ptr::null_mut() };
        }
    }

    fn restore_focus(&mut self) {
        let prev = std::mem::replace(&mut self.overlay_prev_focus, std::ptr::null_mut());
        let to = if !prev.is_null() && unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(prev) } != 0 { prev } else { self.hwnd };
        unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(to);
        }
    }

    pub fn pick(&mut self, placeholder: &str, items: Vec<PickItem>, on_pick: impl FnOnce(&mut App, &PickItem) + 'static) {
        self.open_pick(placeholder, items, crate::quickopen::PickMode::Other, on_pick);
    }

    pub fn open_pick(&mut self, placeholder: &str, items: Vec<PickItem>, mode: crate::quickopen::PickMode, on_pick: impl FnOnce(&mut App, &PickItem) + 'static) {
        self.remember_focus();
        self.end_pick();
        self.pick_mode = mode;
        // the name, the description, and both as a path ("src/ui.rs"); Go to File matches
        // the items' own fields instead
        if mode != crate::quickopen::PickMode::Files {
            self.pick_hay = items
                .iter()
                .map(|it| {
                    let (name, desc) = (it.label.to_lowercase(), it.desc.to_lowercase());
                    (format!("{name} {desc} {desc}/{name}"), name.len())
                })
                .collect();
        }
        let filtered = (0..items.len()).collect();
        self.overlay = Some(Overlay { kind: OverlayKind::Pick { items, filtered, sel: 0, on_pick: Some(Box::new(on_pick)) } });
        self.scroll[Area::Pick as usize] = 0;
        self.show_overlay_edit(placeholder, "");
    }

    pub fn input(&mut self, prompt: &str, value: &str, on_ok: impl FnOnce(&mut App, String) + 'static) {
        self.remember_focus();
        self.end_pick();
        self.overlay = Some(Overlay { kind: OverlayKind::Input { prompt: format!("{prompt} (Press 'Enter' to confirm or 'Escape' to cancel)"), on_ok: Some(Box::new(on_ok)) } });
        self.show_overlay_edit(prompt, value);
    }

    pub fn confirm(&mut self, message: &str, detail: &str, buttons: &[&str], on_choice: impl FnOnce(&mut App, usize) + 'static) {
        self.remember_focus();
        self.end_pick();
        let mut b: Vec<String> = buttons.iter().map(|s| s.to_string()).collect();
        b.push("Cancel".into());
        self.overlay = Some(Overlay { kind: OverlayKind::Confirm { message: message.into(), detail: detail.into(), buttons: b, sel: 0, on_choice: Some(Box::new(on_choice)) } });
        self.hide_overlay_edit();
        unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(self.hwnd);
        }
        self.invalidate();
    }

    pub fn close_overlay(&mut self) {
        self.overlay = None;
        self.end_pick();
        self.hide_overlay_edit();
        self.restore_focus();
        self.invalidate();
    }

    pub fn filter_pick(&mut self) {
        let raw = win::get_text(self.overlay_edit);
        if !matches!(self.overlay, Some(Overlay { kind: OverlayKind::Pick { .. } })) {
            return;
        }
        if self.pick_mode == crate::quickopen::PickMode::Files {
            self.filter_files(&raw);
            self.scroll[Area::Pick as usize] = 0;
            self.invalidate();
            return;
        }
        // a path typed with backslashes matches the "dir/name" of files too
        let q = raw.to_lowercase().replace('\\', "/");
        let hay = &self.pick_hay;
        if let Some(Overlay { kind: OverlayKind::Pick { items, filtered, sel, .. } }) = &mut self.overlay {
            let words: Vec<&str> = q.split_whitespace().collect();
            *filtered = (0..items.len()).filter(|&i| hay.get(i).is_some_and(|(h, _)| words.iter().all(|w| h.contains(w)))).collect();
            *sel = 0;
        }
        self.qo.words = q.split_whitespace().map(crate::fuzzy::pattern).collect();
        self.scroll[Area::Pick as usize] = 0;
        self.invalidate();
    }

    pub fn accept_overlay(&mut self, index: Option<usize>) {
        let Some(ov) = self.overlay.take() else { return };
        // before the callback, which may open another picker (the typed line stays for it)
        self.end_pick();
        let text = win::get_text(self.overlay_edit);
        self.hide_overlay_edit();
        // the chosen item usually opens something (a file, a command's view): the keyboard goes
        // to the window, not back to the commit box (only cancelling returns there)
        self.overlay_prev_focus = std::ptr::null_mut();
        unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(self.hwnd);
        }
        match ov.kind {
            OverlayKind::Pick { items, filtered, sel, mut on_pick } => {
                let i = index.unwrap_or(sel);
                if let Some(&ix) = filtered.get(i) {
                    if let Some(f) = on_pick.take() {
                        f(self, &items[ix]);
                    }
                }
            }
            OverlayKind::Input { mut on_ok, .. } => {
                if let Some(f) = on_ok.take() {
                    f(self, text);
                }
            }
            OverlayKind::Confirm { buttons, sel, mut on_choice, .. } => {
                let i = index.unwrap_or(sel);
                if i + 1 < buttons.len() {
                    if let Some(f) = on_choice.take() {
                        f(self, i);
                    }
                }
            }
        }
        self.invalidate();
    }

    // ------------------------------------------------------------ views

    pub fn diff_context(&self) -> String {
        format!("-U{}", if self.st.compact_diff { 3 } else { 100000 })
    }

    /// Opens the diff of a change in a tab (a preview tab unless `pin`).
    pub fn open_change(&mut self, f: FileChange, pin: bool) {
        // an already open tab is just activated (and refreshed), keeping its scroll position
        if self.open_tab(format!("change:{}:{}", group_id(f.group), f.path), pin) && matches!(self.view, MainView::Diff(_) | MainView::File(_)) {
            return;
        }
        self.load_change_view(f, false);
    }

    pub fn load_change_view(&mut self, f: FileChange, keep_scroll: bool) {
        let Some(root) = self.root() else { return };
        if self.tabs.is_empty() {
            self.open_tab(format!("change:{}:{}", group_id(f.group), f.path), false);
        }
        self.view_gen += 1;
        let gen = self.view_gen;
        self.mark_tab_gen(gen);
        if !keep_scroll {
            self.view = MainView::Loading(f.path.clone());
        }
        let ctx = self.diff_context();
        let inline = self.st.inline_diff;
        let tx = self.tx.clone();
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let view = build_change_view(&root, &f, &ctx, inline);
            let _ = tx.send(JobResult::View { gen, view, keep_scroll });
            post(hwnd);
        });
        self.invalidate();
    }

    pub fn open_commit(&mut self, commit: Commit, stash: Option<usize>, pin: bool) {
        let Some(root) = self.root() else { return };
        let key = match stash {
            Some(i) => format!("stash:{i}:{}", commit.hash),
            None => format!("commit:{}", commit.hash),
        };
        if self.open_tab(key, pin) && !matches!(self.view, MainView::Loading(_) | MainView::Empty) {
            return;
        }
        self.view_gen += 1;
        let gen = self.view_gen;
        self.mark_tab_gen(gen);
        self.view = MainView::Loading(commit.subject().to_string());
        let tx = self.tx.clone();
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let (files, parent) = git::commit_files(&root, &commit.hash);
            let view = MainView::Commit(CommitView { commit, files, parent, sel: None, stash });
            let _ = tx.send(JobResult::View { gen, view, keep_scroll: false });
            post(hwnd);
        });
        self.invalidate();
    }

    pub fn open_commit_file(&mut self, index: usize) {
        let Some(root) = self.root() else { return };
        let MainView::Commit(cv) = std::mem::replace(&mut self.view, MainView::Empty) else { return };
        let Some(file) = cv.files.get(index).cloned() else {
            self.view = MainView::Commit(cv);
            return;
        };
        let sha = cv.commit.hash.clone();
        let parent = cv.parent.clone();
        let mut cv = cv;
        cv.sel = Some(index);
        self.view_gen += 1;
        let gen = self.view_gen;
        self.mark_tab_gen(gen);
        let ctx = self.diff_context();
        let inline = self.st.inline_diff;
        let tx = self.tx.clone();
        let hwnd = self.hwnd as isize;
        let back = Box::new(MainView::Commit(cv));
        self.view = MainView::Loading(file.path.clone());
        // keep `back` alive across the thread by sending it along
        std::thread::spawn(move || {
            let mut args: Vec<String> = match &parent {
                Some(p) => vec!["--literal-pathspecs".into(), "diff".into(), "--no-color".into(), "--no-ext-diff".into(), "-M".into(), ctx.clone(), p.clone(), sha.clone()],
                None => vec!["--literal-pathspecs".into(), "show".into(), "--format=".into(), "--no-color".into(), "--no-ext-diff".into(), ctx.clone(), sha.clone()],
            };
            args.push("--".into());
            if let Some(o) = &file.orig {
                args.push(o.clone());
            }
            args.push(file.path.clone());
            let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let out = git::run(&root, &a, None, false);
            let text = out.text();
            let mut view = diff_view_from(&text, &file, DiffKind::Commit, inline, &format!("{} ({})", short_name(&file.path), &sha[..8]));
            if let MainView::Diff(d) = &mut view {
                d.back = Some(back);
            } else {
                view = *back;
            }
            let _ = tx.send(JobResult::View { gen, view, keep_scroll: false });
            post(hwnd);
        });
        self.invalidate();
    }

    pub fn go_back(&mut self) {
        if let MainView::Diff(d) = &mut self.view {
            if let Some(b) = d.back.take() {
                self.view = *b;
                self.scroll[Area::Main as usize] = 0;
                self.invalidate();
            }
        }
    }

    pub fn rebuild_rows(&mut self) {
        let inline = self.st.inline_diff;
        if let MainView::Diff(d) = &mut self.view {
            let (rows, blocks) = diff::build_rows(&d.fd, inline);
            d.rows = rows;
            d.blocks = blocks;
            d.inline = inline;
            d.sel = None;
            d.uid = Default::default();
        }
    }

    // ------------------------------------------------------------ actions on files

    /// Files of a tree folder row (same group, anywhere below the folder).
    pub fn dir_files(&self, d: usize) -> Vec<FileChange> {
        let (Some(repo), Some(dr)) = (&self.repo, self.cdirs.get(d)) else { return vec![] };
        let prefix = format!("{}/", dr.path);
        repo.status.files.iter().filter(|f| f.group == dr.group && f.path.starts_with(&prefix)).cloned().collect()
    }

    pub fn selected_files(&self, row: Option<usize>) -> Vec<FileChange> {
        let Some(repo) = &self.repo else { return vec![] };
        let in_sel = |f: &FileChange| self.sel.contains(&(group_id(f.group), f.path.clone()));
        if let Some(r) = row {
            if let Some(CRow::Dir(d)) = self.crows.get(r) {
                return self.dir_files(*d);
            }
            if let Some(CRow::File(i)) = self.crows.get(r) {
                let f = &repo.status.files[*i];
                if !in_sel(f) {
                    return vec![f.clone()];
                }
                return repo.status.files.iter().filter(|x| x.group == f.group && in_sel(x)).cloned().collect();
            }
            return vec![];
        }
        repo.status.files.iter().filter(|f| in_sel(f)).cloned().collect()
    }

    pub fn stage(&mut self, files: Vec<FileChange>) {
        if files.is_empty() {
            return;
        }
        let conflicted: Vec<String> = files.iter().filter(|f| f.group == Group::Merge).map(|f| f.path.clone()).collect();
        let root = self.root().unwrap();
        let still_marked: Vec<String> = conflicted.iter().filter(|p| std::fs::read_to_string(root.join(p)).map(|t| t.lines().any(|l| l.starts_with("<<<<<<<"))).unwrap_or(false)).cloned().collect();
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let run = move |app: &mut App| {
            app.op("Stage", move |root| {
                for chunk in path_chunks(&paths) {
                    // literal: a file named app/[id]/x must not also match app/i/x
                    let mut a = vec!["--literal-pathspecs", "add", "-A", "--"];
                    a.extend(chunk.iter().map(|s| s.as_str()));
                    git::exec(root, &a)?;
                }
                Ok(String::new())
            }, None, None);
        };
        if !still_marked.is_empty() {
            let name = short_name(&still_marked[0]).to_string();
            self.confirm(&format!("Are you sure you want to stage {name} with merge conflicts?"), "", &["Yes"], move |app, _| run(app));
        } else {
            run(self);
        }
    }

    pub fn unstage(&mut self, files: Vec<FileChange>) {
        let mut paths: Vec<String> = Vec::new();
        for f in files.iter().filter(|f| f.group == Group::Staged) {
            paths.push(f.path.clone());
            if let Some(o) = &f.orig {
                paths.push(o.clone());
            }
        }
        if paths.is_empty() {
            return;
        }
        let has_head = self.status().map(|s| s.head.is_some()).unwrap_or(false);
        self.op("Unstage", move |root| {
            for chunk in path_chunks(&paths) {
                // literal: bracketed names aren't globs; -f: rm refuses "staged content different" otherwise, and --cached never touches the work tree
                let mut a = if has_head { vec!["--literal-pathspecs", "reset", "-q", "HEAD", "--"] } else { vec!["--literal-pathspecs", "rm", "--cached", "-r", "-f", "-q", "--"] };
                a.extend(chunk.iter().map(|s| s.as_str()));
                git::exec(root, &a)?;
            }
            Ok(String::new())
        }, None, None);
    }

    pub fn discard(&mut self, files: Vec<FileChange>) {
        let files: Vec<FileChange> = files.into_iter().filter(|f| f.group == Group::Changes).collect();
        if files.is_empty() {
            return;
        }
        let untracked: Vec<String> = files.iter().filter(|f| f.kind == Kind::Untracked).map(|f| f.path.clone()).collect();
        let tracked: Vec<String> = files.iter().filter(|f| f.kind != Kind::Untracked).map(|f| f.path.clone()).collect();
        let (msg, btn) = if untracked.is_empty() {
            if tracked.len() == 1 {
                (format!("Are you sure you want to discard changes in '{}'?", short_name(&tracked[0])), "Discard File".to_string())
            } else {
                (format!("Are you sure you want to discard changes in {} files?", tracked.len()), format!("Discard {} Files", tracked.len()))
            }
        } else if tracked.is_empty() {
            if untracked.len() == 1 {
                (format!("Are you sure you want to DELETE the following untracked file: '{}'?", short_name(&untracked[0])), "Delete File".to_string())
            } else {
                (format!("Are you sure you want to DELETE {} untracked files?", untracked.len()), format!("Delete {} Files", untracked.len()))
            }
        } else {
            (format!("Are you sure you want to discard changes in {} files?\n\nThis will DELETE {} untracked files!", files.len(), untracked.len()), format!("Discard {} Files", files.len()))
        };
        let detail = if untracked.is_empty() { "This is IRREVERSIBLE!" } else { "Untracked files are moved to the Recycle Bin. Tracked changes are IRREVERSIBLE!" };
        self.confirm(&msg, detail, &[&btn], move |app, _| {
            app.op("Discard", move |root| discard_paths(root, &tracked, &untracked), None, None);
        });
    }

    pub fn stage_all(&mut self) {
        let files: Vec<FileChange> = self.status().map(|s| s.files.iter().filter(|f| f.group == Group::Changes).cloned().collect()).unwrap_or_default();
        self.stage(files);
    }

    pub fn unstage_all(&mut self) {
        let has_head = self.status().map(|s| s.head.is_some()).unwrap_or(false);
        self.op("Unstage", move |root| if has_head { git::exec(root, &["reset", "-q", "HEAD", "--", "."]) } else { git::exec(root, &["rm", "--cached", "-r", "-f", "-q", "--", "."]) }, None, None);
    }

    pub fn discard_all(&mut self) {
        let files: Vec<FileChange> = self.status().map(|s| s.files.iter().filter(|f| f.group == Group::Changes).cloned().collect()).unwrap_or_default();
        self.discard(files);
    }

    pub fn add_to_gitignore(&mut self, files: Vec<FileChange>) {
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        self.op("Ignore", move |root| {
            use std::io::Write;
            let p = root.join(".gitignore");
            // appended as bytes: a file that can't be read (not UTF-8, locked) is never rewritten
            let cur = match std::fs::read(&p) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(GitError::new(e.to_string())),
            };
            let mut add = String::new();
            if !cur.is_empty() && !cur.ends_with(b"\n") {
                add.push('\n');
            }
            for x in paths {
                // the path itself, not a pattern: app/[id]/x must not also ignore app/i/x
                if x.starts_with(['#', '!']) {
                    add.push('\\');
                }
                for ch in x.chars() {
                    if matches!(ch, '[' | '*' | '?' | '\\') {
                        add.push('\\');
                    }
                    add.push(ch);
                }
                add.push('\n');
            }
            std::fs::OpenOptions::new().append(true).create(true).open(&p).and_then(|mut f| f.write_all(add.as_bytes())).map_err(|e| GitError::new(e.to_string()))?;
            Ok(String::new())
        }, None, None);
    }

    // ------------------------------------------------------------ partial staging

    pub fn diff_selection_lines(&self, block: Option<usize>) -> HashSet<LineId> {
        let mut set = HashSet::new();
        let MainView::Diff(d) = &self.view else { return set };
        if let Some(b) = block {
            if let Some(bl) = d.blocks.get(b) {
                set.extend(bl.lines.iter().copied());
            }
            return set;
        }
        let Some((a, b)) = d.sel else { return set };
        let (lo, hi) = (a.min(b), a.max(b));
        for r in d.rows.iter().take(hi + 1).skip(lo) {
            for id in [r.left, r.right].into_iter().flatten() {
                let l = &d.fd.hunks[id.0].lines[id.1];
                if matches!(l.kind, diff::LK::Add | diff::LK::Del) {
                    set.insert(id);
                }
            }
        }
        set
    }

    /// action: 0 = stage, 1 = unstage, 2 = revert
    pub fn apply_lines(&mut self, block: Option<usize>, action: u8) {
        let lines = self.diff_selection_lines(block);
        let MainView::Diff(d) = &self.view else { return };
        if lines.is_empty() {
            self.info("Select changed lines first.");
            return;
        }
        let kind = d.kind;
        let change = d.change.clone();
        if kind == DiffKind::Untracked && action == 0 && block.is_some() {
            if let Some(c) = change {
                self.stage(vec![c]);
            }
            return;
        }
        let reverse = action != 0;
        let Some(patch) = diff::build_patch(&d.fd, &lines, reverse) else { return };
        let args: Vec<&'static str> = match (action, kind) {
            (0, DiffKind::WorkingTree) | (0, DiffKind::Untracked) => vec!["apply", "--cached", "--whitespace=nowarn", "--recount", "-"],
            (1, DiffKind::Index) => vec!["apply", "--cached", "-R", "--whitespace=nowarn", "--recount", "-"],
            (2, DiffKind::WorkingTree) => vec!["apply", "-R", "--whitespace=nowarn", "--recount", "-"],
            _ => return,
        };
        let title = ["Stage", "Unstage", "Revert"][action as usize];
        let run = move |app: &mut App| {
            app.op(title, move |root| git::exec_input(root, &args, Some(patch.as_bytes())), None, None);
        };
        if action == 2 {
            self.confirm("Are you sure you want to revert the selected changes?", "This is IRREVERSIBLE!", &["Revert"], move |app, _| run(app));
        } else {
            run(self);
        }
    }

    pub fn resolve_conflict(&mut self, line: usize, which: u8) {
        let MainView::File(f) = &self.view else { return };
        let (fpath, change) = (f.path.clone(), f.change.clone());
        let Some(root) = self.root() else { return };
        let path = root.join(&fpath);
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let acc = match which {
            0 => diff::Accept::Current,
            1 => diff::Accept::Incoming,
            _ => diff::Accept::Both,
        };
        let out = diff::resolve_conflict(&text, line, acc);
        if let Err(e) = std::fs::write(&path, out) {
            self.error(format!("Unable to write file: {e}"));
        }
        if let Some(c) = change {
            self.load_change_view(c, true);
        }
    }

    // --------------------------------------------------------------- commit

    pub fn commit(&mut self, amend: bool, all: bool, post_cmd: u8) {
        // Ctrl+Enter isn't disabled like the button: don't commit the same message twice
        if self.ops.iter().any(|o| o == "Committing" || o == "Continue Rebase") {
            return;
        }
        let Some(s) = self.status().cloned() else {
            self.info("There are no available repositories.");
            return;
        };
        let msg = win::get_text(self.commit_edit).replace("\r\n", "\n").trim_end().to_string();
        if s.count(Group::Merge) > 0 {
            self.warn("There are merge conflicts. Resolve them before committing.");
            return;
        }
        if s.rebasing && !amend {
            self.op("Continue Rebase", |root| git::exec(root, &["-c", "core.editor=true", "rebase", "--continue"]), Some(Box::new(|app: &mut App| win::set_text(app.commit_edit, ""))), None);
            return;
        }
        let staged = s.count(Group::Staged);
        let changes = s.count(Group::Changes);
        let mut all = all;
        // staging everything because nothing looked staged: checked again when the commit runs,
        // since a Stage still queued before it makes the status above stale
        let mut implied = false;
        if staged == 0 && !amend && !s.merging && !all {
            if changes == 0 {
                self.info("There are no changes to commit.");
                return;
            }
            match self.st.smart_commit {
                Some(true) => {
                    all = true;
                    implied = true;
                }
                Some(false) => {
                    self.info("There are no staged changes to commit.");
                    return;
                }
                None => {
                    self.confirm("There are no staged changes to commit.\n\nWould you like to stage all your changes and commit them directly?", "", &["Yes", "Always", "Never"], move |app, i| {
                        match i {
                            0 => app.commit(amend, true, post_cmd),
                            1 => {
                                app.st.smart_commit = Some(true);
                                app.st.save();
                                app.commit(amend, true, post_cmd)
                            }
                            _ => {
                                app.st.smart_commit = Some(false);
                                app.st.save();
                            }
                        }
                    });
                    return;
                }
            }
        }
        if msg.is_empty() && !amend {
            self.input("Commit message", "", move |app, m| {
                if m.trim().is_empty() {
                    app.info("Commit operation was cancelled due to empty commit message.");
                    return;
                }
                win::set_text(app.commit_edit, &m);
                app.commit(amend, all, post_cmd);
            });
            return;
        }
        let sent = msg.clone();
        let then: Then = Box::new(move |app: &mut App| {
            // a message typed while the commit ran (hooks can take a while) is kept
            if win::get_text(app.commit_edit).replace("\r\n", "\n").trim_end() == sent {
                win::set_text(app.commit_edit, "");
                app.edit_changed();
            }
            match post_cmd {
                1 => app.push(false),
                2 => app.sync(),
                _ => {}
            }
        });
        self.op("Committing", move |root| {
            // `diff --cached --quiet` exits with 1 when something is staged
            if all && !(implied && git::run(root, &["diff", "--cached", "--quiet"], None, true).code == 1) {
                git::exec(root, &["add", "-A"])?;
            }
            if amend && msg.is_empty() {
                git::exec(root, &["commit", "--quiet", "--amend", "--no-edit"])
            } else {
                let mut a = vec!["commit", "--quiet", "--allow-empty-message", "-F", "-"];
                if amend {
                    a.push("--amend");
                }
                git::exec_input(root, &a, Some(msg.as_bytes()))
            }
        }, Some(then), None);
    }

    pub fn undo_commit(&mut self) {
        let Some(s) = self.status() else { return };
        if s.head.is_none() {
            self.warn("Can't undo because HEAD doesn't point to any commit.");
            return;
        }
        // HEAD is read when the op runs: a commit still queued before it is the one undone
        let msg = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let m = msg.clone();
        self.op("Undo Commit", move |root| {
            *m.lock().unwrap() = git::run(root, &["log", "-1", "--format=%B"], None, true).text().trim().to_string();
            if git::run(root, &["rev-parse", "--verify", "-q", "HEAD~"], None, true).ok() {
                git::exec(root, &["reset", "--soft", "HEAD~"])
            } else {
                git::exec(root, &["update-ref", "-d", "HEAD"])
            }
        }, Some(Box::new(move |app: &mut App| {
            let msg = std::mem::take(&mut *msg.lock().unwrap());
            win::set_text(app.commit_edit, &msg);
            app.edit_changed();
        })), None);
    }

    // ------------------------------------------------------------ branches

    pub fn ref_items(&self, local: bool, remote: bool, tags: bool, exclude_current: bool) -> Vec<PickItem> {
        let mut v = Vec::new();
        let Some(r) = &self.repo else { return v };
        let cur = r.status.branch.clone();
        let mut first = true;
        for rf in r.refs.iter().filter(|x| x.kind == RefKind::Head && local) {
            if exclude_current && Some(&rf.name) == cur.as_ref() {
                continue;
            }
            let mut it = pi(&rf.name, format!("{} \u{2022} {}", &rf.sha[..8], rf.subject), 'B', format!("H:{}", rf.name));
            if first {
                it.sep = "branches".into();
                first = false;
            }
            v.push(it);
        }
        first = true;
        for rf in r.refs.iter().filter(|x| x.kind == RefKind::Remote && remote) {
            let mut it = pi(&rf.name, format!("{} \u{2022} {}", &rf.sha[..8], rf.subject), '\u{E753}', format!("R:{}", rf.name));
            if first {
                it.sep = "remote branches".into();
                first = false;
            }
            v.push(it);
        }
        first = true;
        for rf in r.refs.iter().filter(|x| x.kind == RefKind::Tag && tags) {
            let mut it = pi(&rf.name, format!("{} \u{2022} {}", &rf.sha[..8], rf.subject), '\u{E8EC}', format!("T:{}", rf.name));
            if first {
                it.sep = "tags".into();
                first = false;
            }
            v.push(it);
        }
        v
    }

    pub fn checkout_picker(&mut self) {
        if self.repo.is_none() {
            return;
        }
        let mut items = vec![pi("Create new branch...", "", '\u{E710}', "new"), pi("Create new branch from...", "", '\u{E710}', "newfrom"), pi("Checkout detached...", "", '\u{E71B}', "detached")];
        items.extend(self.ref_items(true, true, true, true));
        self.pick("Select a branch or tag to checkout", items, |app, it| match it.data.as_str() {
            "new" => app.create_branch(None),
            "newfrom" => app.create_branch_from(),
            "detached" => {
                let items = app.ref_items(true, true, true, false);
                app.pick("Select a branch to checkout in detached mode", items, |app, it| {
                    let name = it.data[2..].to_string();
                    app.checkout(format!("D:{name}"));
                });
            }
            d => app.checkout(d.to_string()),
        });
    }

    /// `spec` is "H:name" (local), "R:remote/name", "T:tag", "D:anything detached".
    pub fn checkout(&mut self, spec: String) {
        let (k, name) = spec.split_at(2);
        let name = name.to_string();
        let args: Vec<String> = match k {
            "H:" => vec!["checkout".into(), "-q".into(), name.clone(), "--".into()],
            "R:" => {
                let (_, short) = crate::branches::split_remote(&self.remotes(), &name);
                // a local branch of the same name counts only when it doesn't track another
                // remote branch; otherwise --track reports the clash instead of switching silently
                let local = self.repo.as_ref().and_then(|r| r.refs.iter().find(|x| x.kind == RefKind::Head && (x.upstream.as_deref() == Some(name.as_str()) || x.name == short && x.upstream.is_none())).map(|x| x.name.clone()));
                match local {
                    Some(l) => vec!["checkout".into(), "-q".into(), l, "--".into()],
                    None => vec!["checkout".into(), "-q".into(), "--track".into(), name.clone()],
                }
            }
            _ => vec!["checkout".into(), "-q".into(), "--detach".into(), name.clone(), "--".into()],
        };
        let args2 = args.clone();
        let on_err: OnErr = Box::new(move |app: &mut App, e: &GitError| {
            if !(e.stderr.contains("would be overwritten") || e.stderr.contains("commit your changes or stash them")) {
                return false;
            }
            let args3 = args2.clone();
            app.confirm("Your local changes would be overwritten by checkout.", "", &["Stash & Checkout", "Migrate Changes", "Force Checkout"], move |app, i| {
                let a = args3.clone();
                app.op("Checkout", move |root| {
                    let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
                    if i == 2 {
                        let mut f = refs.clone();
                        f.insert(2, "--force");
                        return git::exec(root, &f);
                    }
                    git::exec(root, &["stash", "push", "--include-untracked"])?;
                    git::exec(root, &refs)?;
                    if i == 1 {
                        stash_restore(root, "pop", None)?;
                    }
                    Ok(String::new())
                }, None, None);
            });
            true
        });
        self.op("Checkout", move |root| {
            let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            git::exec(root, &a)
        }, None, Some(on_err));
    }

    pub fn create_branch(&mut self, from: Option<String>) {
        self.input("Please provide a new branch name", "", move |app, name| {
            let name = sanitize_branch(&name);
            if name.is_empty() {
                app.warn("Please provide a valid branch name");
                return;
            }
            let from = from.clone();
            app.op("Branch", move |root| {
                let mut a = vec!["checkout", "-q", "-b", name.as_str()];
                if let Some(f) = &from {
                    // like VS Code: a branch made from origin/x must not track origin/x (autoSetupMerge), or Push/Sync would go there
                    a.insert(2, "--no-track");
                    a.push(f.as_str());
                }
                git::exec(root, &a)
            }, None, None);
        });
    }

    pub fn create_branch_from(&mut self) {
        let mut items = vec![pi("HEAD", "Current", 'B', "HEAD")];
        items.extend(self.ref_items(true, true, true, false));
        self.pick("Select a ref to create the branch from", items, |app, it| {
            let r = if it.data == "HEAD" { "HEAD".to_string() } else { it.data[2..].to_string() };
            app.create_branch(Some(r));
        });
    }

    pub fn rename_branch(&mut self) {
        let cur = self.status().and_then(|s| s.branch.clone()).unwrap_or_default();
        self.input("Please provide a new branch name", &cur, |app, name| {
            let name = sanitize_branch(&name);
            if !name.is_empty() {
                app.git_op("Rename Branch", vec!["branch".into(), "-m".into(), name]);
            }
        });
    }

    pub fn delete_branch(&mut self, name: Option<String>) {
        let run = |app: &mut App, name: String| {
            let n2 = name.clone();
            let on_err: OnErr = Box::new(move |app: &mut App, e: &GitError| {
                if !e.stderr.contains("not fully merged") {
                    return false;
                }
                let n3 = n2.clone();
                app.confirm(&format!("The branch '{n3}' is not fully merged. Delete anyway?"), "", &["Delete Branch"], move |app, _| {
                    app.git_op("Delete Branch", vec!["branch".into(), "-D".into(), n3]);
                });
                true
            });
            app.op("Delete Branch", move |root| git::exec(root, &["branch", "-d", &name]), None, Some(on_err));
        };
        match name {
            Some(n) => run(self, n),
            None => {
                let items = self.ref_items(true, false, false, true);
                self.pick("Select a branch to delete", items, move |app, it| run(app, it.data[2..].to_string()));
            }
        }
    }

    pub fn merge_picker(&mut self) {
        let items = self.ref_items(true, true, true, true);
        self.pick("Select a branch or tag to merge from", items, |app, it| {
            let n = it.data[2..].to_string();
            app.git_op("Merge", vec!["merge".into(), n]);
        });
    }

    pub fn rebase_picker(&mut self) {
        let items = self.ref_items(true, true, false, true);
        let cur = self.status().and_then(|s| s.branch.clone()).unwrap_or_default();
        self.pick(&format!("Select a branch to rebase '{cur}' onto"), items, |app, it| {
            let n = it.data[2..].to_string();
            app.git_op("Rebase", vec!["rebase".into(), n]);
        });
    }

    /// Aborts whichever merge/rebase/cherry-pick/revert is in progress, after a confirm:
    /// aborting throws away any conflict resolution done so far.
    pub fn abort_op(&mut self) {
        let Some((what, verb)) = self.status().and_then(op_in_progress) else {
            self.info("There is no merge, rebase, cherry-pick or revert in progress.");
            return;
        };
        let btn = format!("Abort {what}");
        self.confirm(&format!("Abort the {}? Resolved conflicts will be lost.", what.to_lowercase()), "", &[&btn.clone()], move |app, _| {
            app.git_op(&btn, vec![verb.into(), "--abort".into()]);
        });
    }

    // -------------------------------------------------------------- remote

    pub fn fetch(&mut self, prune: bool, all: bool) {
        if self.status().map(|s| s.remotes.is_empty()).unwrap_or(true) {
            self.warn("This repository has no remotes configured to fetch from.");
            return;
        }
        let mut a = vec!["fetch".to_string()];
        if all {
            a.push("--all".into());
        }
        if prune {
            a.push("--prune".into());
        }
        self.git_op("Fetching", a);
    }

    pub fn pull(&mut self, rebase: bool) {
        let Some(s) = self.status() else { return };
        if s.remotes.is_empty() {
            self.warn("Your repository has no remotes configured to pull from.");
            return;
        }
        if s.upstream.is_none() {
            self.warn("The current branch has no upstream branch. Publish it first.");
            return;
        }
        let mut a = vec!["pull".to_string(), "--tags".to_string()];
        if rebase {
            a.push("--rebase".into());
        }
        self.git_op("Pulling", a);
    }

    pub fn push(&mut self, force: bool) {
        let Some(s) = self.status().cloned() else { return };
        if s.remotes.is_empty() {
            self.confirm("Your repository has no remotes configured to push to.", "", &["Add Remote"], |app, _| app.add_remote());
            return;
        }
        let Some(branch) = s.branch.clone() else {
            self.warn("Please check out a branch to push to a remote.");
            return;
        };
        if s.upstream.is_none() {
            self.confirm(&format!("The branch \"{branch}\" has no remote branch. Would you like to publish this branch?"), "", &["OK"], |app, _| app.publish());
            return;
        }
        let target = push_target(&s);
        if force {
            self.confirm("You are about to force push your changes, this can be destructive and could inadvertently overwrite changes made by others.\n\nAre you sure to continue?", "", &["OK"], move |app, _| {
                app.git_op("Pushing", [vec!["push".into(), "--force-with-lease".into()], target].concat());
            });
            return;
        }
        self.git_op("Pushing", [vec!["push".to_string()], target].concat());
    }

    pub fn publish(&mut self) {
        let Some(s) = self.status().cloned() else { return };
        let Some(branch) = s.branch.clone() else {
            self.warn("Please check out a branch to publish it.");
            return;
        };
        if s.remotes.is_empty() {
            self.confirm("Your repository has no remotes configured to publish to.", "", &["Add Remote"], |app, _| app.add_remote());
            return;
        }
        let go = move |app: &mut App, remote: String| {
            app.git_op("Publishing", vec!["push".into(), "-u".into(), remote, branch.clone()]);
        };
        if s.remotes.len() == 1 {
            go(self, s.remotes[0].clone());
        } else {
            let items = s.remotes.iter().map(|r| pi(r.clone(), "", '\u{E753}', r.clone())).collect();
            self.pick("Pick a remote to publish the branch to:", items, move |app, it| go(app, it.data.clone()));
        }
    }

    pub fn sync(&mut self) {
        let Some(s) = self.status().cloned() else { return };
        if s.upstream.is_none() {
            self.publish();
            return;
        }
        let target = push_target(&s);
        let run = move |app: &mut App| {
            let target = target.clone();
            app.op("Syncing", move |root| {
                git::exec(root, &["pull", "--tags"])?;
                let mut a = vec!["push"];
                a.extend(target.iter().map(|x| x.as_str()));
                git::exec(root, &a)
            }, None, None);
        };
        if self.st.confirm_sync {
            let up = s.upstream.clone().unwrap();
            self.confirm(&format!("This action will pull and push commits from and to \"{up}\"."), "", &["OK", "OK, Don't Show Again"], move |app, i| {
                if i == 1 {
                    app.st.confirm_sync = false;
                    app.st.save();
                }
                run(app);
            });
        } else {
            run(self);
        }
    }

    pub fn add_remote(&mut self) {
        self.input("Provide repository URL", "", |app, url| {
            if url.trim().is_empty() {
                return;
            }
            let has_origin = app.status().map(|s| s.remotes.iter().any(|r| r == "origin")).unwrap_or(false);
            app.input("Please provide a remote name", if has_origin { "" } else { "origin" }, move |app, name| {
                let name = name.trim().to_string();
                if name.is_empty() || name.contains(' ') {
                    app.warn("Remote name format invalid");
                    return;
                }
                app.git_op("Add Remote", vec!["remote".into(), "add".into(), name, url.trim().to_string()]);
            });
        });
    }

    pub fn remove_remote(&mut self) {
        let remotes = self.status().map(|s| s.remotes.clone()).unwrap_or_default();
        let items = remotes.iter().map(|r| pi(r.clone(), "", '\u{E753}', r.clone())).collect();
        self.pick("Select a remote to remove", items, |app, it| app.git_op("Remove Remote", vec!["remote".into(), "remove".into(), it.data.clone()]));
    }

    // --------------------------------------------------------------- stash

    pub fn stash(&mut self, untracked: bool, staged: bool) {
        let Some(s) = self.status() else { return };
        // without --include-untracked, git stashes nothing (and says so with exit code 0) when
        // only untracked files changed
        if s.files.iter().all(|f| !untracked && f.kind == Kind::Untracked) {
            self.info("There are no changes to stash.");
            return;
        }
        if staged && s.count(Group::Staged) == 0 {
            self.info("There are no staged changes to stash.");
            return;
        }
        self.input("Optionally provide a stash message", "", move |app, m| {
            let mut a = vec!["stash".to_string(), "push".to_string()];
            if untracked {
                a.push("--include-untracked".into());
            }
            if staged {
                a.push("--staged".into());
            }
            if !m.trim().is_empty() {
                a.push("-m".into());
                a.push(m.trim().to_string());
            }
            app.git_op("Stash", a);
        });
    }

    pub fn stash_pick(&mut self, placeholder: &str, f: impl FnOnce(&mut App, usize) + 'static) {
        let stashes = self.repo.as_ref().map(|r| r.stashes.clone()).unwrap_or_default();
        if stashes.is_empty() {
            self.info("There are no stashes in the repository.");
            return;
        }
        let items = stashes.iter().map(|s| pi(format!("#{}:  {}", s.index, s.message), "", '\u{E7B8}', s.index.to_string())).collect();
        self.pick(placeholder, items, move |app, it| f(app, it.data.parse().unwrap_or(0)));
    }

    pub fn stash_apply(&mut self, index: usize, pop: bool) {
        let r = format!("stash@{{{index}}}");
        let verb = if pop { "pop" } else { "apply" };
        self.op(if pop { "Stash Pop" } else { "Stash Apply" }, move |root| stash_restore(root, verb, Some(&r)), None, None);
    }

    pub fn stash_view(&mut self, index: usize) {
        let Some(root) = self.root() else { return };
        let r = format!("stash@{{{index}}}");
        let out = git::run(&root, &["log", "-1", "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%D%x1f%B%x1e", &r], None, true);
        if let Some(c) = git::parse_log(&out.text()).into_iter().next() {
            self.open_commit(c, Some(index), true);
        }
    }

    // ---------------------------------------------------------------- tags

    pub fn create_tag(&mut self, at: Option<String>) {
        self.input("Please provide a tag name", "", move |app, name| {
            let name = name.trim().replace(' ', "-");
            if name.is_empty() {
                return;
            }
            let at = at.clone();
            app.input("Please provide a message to annotate the tag", "", move |app, msg| {
                let mut a = vec!["tag".to_string()];
                if !msg.trim().is_empty() {
                    a.extend(["-a".into(), name.clone(), "-m".into(), msg.trim().to_string()]);
                } else {
                    a.push(name.clone());
                }
                if let Some(r) = at {
                    a.push(r);
                }
                app.git_op("Tag", a);
            });
        });
    }

    pub fn delete_tag(&mut self) {
        let items = self.ref_items(false, false, true, false);
        if items.is_empty() {
            self.warn("This repository has no tags.");
            return;
        }
        self.pick("Select a tag to delete", items, |app, it| app.git_op("Delete Tag", vec!["tag".into(), "-d".into(), it.data[2..].to_string()]));
    }

    // ---------------------------------------------------------------- misc

    pub fn configure_user(&mut self) {
        let root = self.root().unwrap_or_else(|| PathBuf::from("."));
        let cur_name = git::run(&root, &["config", "--get", "user.name"], None, true).text().trim().to_string();
        let cur_mail = git::run(&root, &["config", "--get", "user.email"], None, true).text().trim().to_string();
        self.input("Please provide your name (user.name)", &cur_name, move |app, name| {
            app.input("Please provide your email (user.email)", &cur_mail, move |app, email| {
                if name.trim().is_empty() || email.trim().is_empty() {
                    return;
                }
                app.op("Config", move |root| {
                    git::exec(root, &["config", "--global", "user.name", name.trim()])?;
                    git::exec(root, &["config", "--global", "user.email", email.trim()])
                }, None, None);
            });
        });
    }

    pub fn open_folder(&mut self) {
        if let Some(p) = win::pick_folder(self.hwnd, "Open Folder") {
            self.open_repo(Path::new(&p));
        }
    }

    pub fn open_recent(&mut self) {
        self.st.refresh_recent();
        let items: Vec<PickItem> = self.st.recent.iter().map(|r| pi(Path::new(r).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), r.clone(), '\u{E8B7}', r.clone())).collect();
        if items.is_empty() {
            self.info("No recently opened folders.");
            return;
        }
        self.pick("Select a folder to open (Ctrl+Enter opens a new window)", items, |app, it| {
            // runs inside the Enter key's WM_KEYDOWN, so the Ctrl state is the key's
            let ctrl = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x11) } < 0;
            if ctrl && Path::new(&it.data).is_dir() {
                app.new_window(Some(&it.data));
            } else {
                app.open_repo(&PathBuf::from(&it.data));
            }
        });
    }

    pub fn new_window(&self, folder: Option<&str>) {
        if let Ok(exe) = std::env::current_exe() {
            let mut c = std::process::Command::new(exe);
            match folder {
                Some(f) => c.arg(f),
                // an empty window, like VS Code, not this window's folder again
                None => c.arg("--new-window"),
            };
            let _ = c.spawn();
        }
    }

    pub fn clone_repo(&mut self) {
        self.input("Provide repository URL", "", |app, url| {
            let url = url.trim().to_string();
            if url.is_empty() {
                return;
            }
            let Some(parent) = win::pick_folder(app.hwnd, "Choose a folder to clone the repository into") else { return };
            let name = url.trim_end_matches('/').trim_end_matches(".git").rsplit(['/', '\\', ':']).next().unwrap_or("repo").to_string();
            let target = PathBuf::from(&parent).join(&name);
            let tgt = target.clone();
            let tx = app.tx.clone();
            let hwnd = app.hwnd as isize;
            app.ops.push("Cloning".into());
            unsafe {
                SetTimer(app.hwnd, TIMER_SPIN, 120, None);
            }
            let then: Then = Box::new(move |app: &mut App| app.open_repo(&tgt));
            std::thread::spawn(move || {
                let r = git::exec(Path::new(&parent), &["clone", "--progress", &url, &target.display().to_string()]);
                let _ = tx.send(JobResult::Op { title: "Cloning".into(), root: None, result: r, then: Some(then), on_err: None });
                post(hwnd);
            });
        });
    }

    pub fn init_repo(&mut self) {
        let folder = self.pending_init.clone().or_else(|| win::pick_folder(self.hwnd, "Choose a folder to initialize a Git repository in").map(PathBuf::from));
        let Some(f) = folder else { return };
        let r = git::exec(&f, &["init", "-b", "main"]);
        match r {
            Ok(_) => self.open_repo(&f),
            Err(e) => self.error(e.message),
        }
    }

    pub fn toggle_theme(&mut self) {
        self.st.dark = !self.st.dark;
        self.st.save();
        self.t = if self.st.dark { Theme::dark() } else { Theme::light() };
        self.apply_theme();
    }

    pub fn apply_theme(&mut self) {
        win::set_title_colors(self.hwnd, self.t.dark, self.t.title, self.t.fg, self.t.border);
        win::set_app_dark_mode(self.t.dark);
        win::set_control_theme(self.commit_edit, self.t.dark);
        win::set_control_theme(self.overlay_edit, self.t.dark);
        for h in [self.find.find_edit, self.find.repl_edit, self.srch.find_edit, self.srch.repl_edit] {
            if !h.is_null() {
                win::set_control_theme(h, self.t.dark);
                unsafe { windows_sys::Win32::Graphics::Gdi::InvalidateRect(h, std::ptr::null(), 1) };
            }
        }
        unsafe {
            if !self.edit_brush.is_null() {
                windows_sys::Win32::Graphics::Gdi::DeleteObject(self.edit_brush as _);
            }
            self.edit_brush = windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(self.t.input_bg);
            windows_sys::Win32::Graphics::Gdi::InvalidateRect(self.commit_edit, std::ptr::null(), 1);
        }
        self.invalidate();
    }

    /// Command Palette. Like VS Code, the commands run from it last come first ("recently
    /// used"), then all the others ("other commands"); typing filters both groups in place.
    pub fn palette(&mut self) {
        self.st.refresh_recent();
        let claude = crate::term::CLAUDE.find().is_some();
        let codex = crate::term::CODEX.find().is_some();
        let installed = |c: &Cmd| match c {
            Cmd::ClaudeOpen | Cmd::ClaudeNew => claude,
            Cmd::CodexOpen | Cmd::CodexNew => codex,
            _ => true,
        };
        let available: Vec<&(Cmd, &str, &str)> = COMMANDS.iter().filter(|(c, _, _)| *c != Cmd::Palette && installed(c)).collect();
        let recent: Vec<&(Cmd, &str, &str)> = self.st.recent_cmds.iter().filter_map(|r| available.iter().find(|(_, t, _)| *t == r.as_str()).copied()).collect();
        let item = |&(c, t, k): &(Cmd, &str, &str)| PickItem { label: t.to_string(), desc: String::new(), icon: '\0', data: format!("{}", c as u32), sep: k.to_string() };
        // the group's name goes on its first row, after the shortcut
        let group = |it: &mut PickItem, name: &str| it.sep = if it.sep.is_empty() { name.to_string() } else { format!("{}    {name}", it.sep) };
        let mut items: Vec<PickItem> = recent.iter().map(|e| item(e)).collect();
        let first_other = items.len();
        let mut other: Vec<PickItem> = available.iter().filter(|e| !recent.contains(e)).map(|e| item(e)).collect();
        other.sort_by(|a, b| a.label.cmp(&b.label));
        items.extend(other);
        if first_other > 0 {
            group(&mut items[0], "recently used");
            if let Some(it) = items.get_mut(first_other) {
                group(it, "other commands");
            }
        }
        self.open_pick("Type the name of a command to run.", items, crate::quickopen::PickMode::Palette, |app, it| {
            let id: u32 = it.data.parse().unwrap_or(0);
            if let Some(&(c, t, _)) = COMMANDS.iter().find(|(c, _, _)| *c as u32 == id) {
                app.st.add_recent_cmd(t);
                app.st.save();
                app.exec(c);
            }
        });
    }

    pub fn exec(&mut self, c: Cmd) {
        let needs_repo = !matches!(c, Cmd::Palette | Cmd::OpenFolder | Cmd::OpenRecent | Cmd::NewWindow | Cmd::Clone | Cmd::Init | Cmd::ToggleOutput | Cmd::CloseEditor | Cmd::CloseOtherEditors | Cmd::CloseAllEditors | Cmd::KeepEditor | Cmd::PinEditor | Cmd::UnpinEditor | Cmd::NextEditor | Cmd::PrevEditor | Cmd::TogglePanelMax | Cmd::PanelRestore | Cmd::PanelFullWidth | Cmd::PanelFullHeight | Cmd::PanelFullScreen | Cmd::ToggleGraphSection | Cmd::ToggleBranchesSection | Cmd::ToggleTheme | Cmd::ToggleSidebar | Cmd::ShowExplorer | Cmd::ShowScm | Cmd::ToggleSidebarPos | Cmd::ToggleTreeView | Cmd::About | Cmd::ClearOutput | Cmd::ToggleInline | Cmd::ToggleCompact | Cmd::NextChange | Cmd::PrevChange | Cmd::ConfigureUser | Cmd::ToggleTerminal | Cmd::NewTerminal | Cmd::NewTerminalEditor | Cmd::ToggleEditorTerminal | Cmd::MoveTerminalToEditor | Cmd::MoveTerminalToPanel | Cmd::KillTerminal | Cmd::FocusNextTerminal | Cmd::FocusPrevTerminal | Cmd::RenameTerminal | Cmd::SelectShell | Cmd::ClearTerminal | Cmd::Find | Cmd::Replace | Cmd::FindNext | Cmd::FindPrev | Cmd::FindNextSel | Cmd::FindPrevSel | Cmd::ReplaceAll | Cmd::ToggleWordWrap);
        // the Explorer and the editor work in a folder without a repository too
        let folder_ok = matches!(c, Cmd::GotoFile | Cmd::GotoLine | Cmd::Save | Cmd::SaveAll | Cmd::RevertFile | Cmd::ExplorerNewFile | Cmd::ExplorerNewFolder | Cmd::ExplorerRename | Cmd::ExplorerDelete | Cmd::ExplorerCollapse | Cmd::OpenExplorer | Cmd::RevealInExplorer | Cmd::OpenTerminal | Cmd::Refresh | Cmd::ClaudeOpen | Cmd::ClaudeNew | Cmd::CodexOpen | Cmd::CodexNew | Cmd::FindInFiles | Cmd::ReplaceInFiles | Cmd::ClearSearch);
        if needs_repo && self.repo.is_none() && !(folder_ok && self.pending_init.is_some()) {
            self.info("There are no available repositories. Open a folder first.");
            return;
        }
        match c {
            Cmd::Palette => self.palette(),
            Cmd::Refresh => self.refresh(),
            Cmd::Commit => self.commit(false, false, 0),
            Cmd::CommitAmend => self.commit(true, false, 0),
            Cmd::CommitAll => self.commit(false, true, 0),
            Cmd::CommitPush => self.commit(false, false, 1),
            Cmd::CommitSync => self.commit(false, false, 2),
            Cmd::UndoCommit => self.undo_commit(),
            Cmd::StageAll => self.stage_all(),
            Cmd::UnstageAll => self.unstage_all(),
            Cmd::DiscardAll => self.discard_all(),
            Cmd::Checkout => self.checkout_picker(),
            Cmd::Branch => self.create_branch(None),
            Cmd::BranchFrom => self.create_branch_from(),
            Cmd::RenameBranch => self.rename_branch(),
            Cmd::DeleteBranch => self.delete_branch(None),
            Cmd::Merge => self.merge_picker(),
            Cmd::Rebase => self.rebase_picker(),
            Cmd::MergeAbort | Cmd::RebaseAbort => self.abort_op(),
            Cmd::RebaseContinue => self.git_op("Continue Rebase", vec!["-c".into(), "core.editor=true".into(), "rebase".into(), "--continue".into()]),
            Cmd::Fetch => self.fetch(false, false),
            Cmd::FetchPrune => self.fetch(true, false),
            Cmd::FetchAll => self.fetch(false, true),
            Cmd::Pull => self.pull(false),
            Cmd::PullRebase => self.pull(true),
            Cmd::Push => self.push(false),
            Cmd::PushForce => self.push(true),
            Cmd::PushTags => self.git_op("Pushing", vec!["push".into(), "--tags".into()]),
            Cmd::Sync => self.sync(),
            Cmd::Publish => self.publish(),
            Cmd::AddRemote => self.add_remote(),
            Cmd::RemoveRemote => self.remove_remote(),
            Cmd::Stash => self.stash(false, false),
            Cmd::StashUntracked => self.stash(true, false),
            Cmd::StashStaged => self.stash(false, true),
            Cmd::StashPop => {
                if self.repo.as_ref().map(|r| r.stashes.is_empty()).unwrap_or(true) {
                    self.info("There are no stashes in the repository.");
                } else {
                    self.stash_apply(0, true);
                }
            }
            Cmd::StashPopPick => self.stash_pick("Pick a stash to pop", |app, i| app.stash_apply(i, true)),
            Cmd::StashApply => self.stash_pick("Pick a stash to apply", |app, i| app.stash_apply(i, false)),
            Cmd::StashDrop => self.stash_pick("Pick a stash to drop", |app, i| {
                app.confirm(&format!("Are you sure you want to drop the stash #{i}?"), "", &["Yes"], move |app, _| app.git_op("Drop Stash", vec!["stash".into(), "drop".into(), format!("stash@{{{i}}}")]));
            }),
            Cmd::StashView => self.stash_pick("Pick a stash to view", |app, i| app.stash_view(i)),
            Cmd::TagCreate => self.create_tag(None),
            Cmd::TagDelete => self.delete_tag(),
            Cmd::CherryPick => self.input("Please provide the commit hash", "", |app, h| {
                if !h.trim().is_empty() {
                    app.git_op("Cherry Pick", vec!["cherry-pick".into(), h.trim().to_string()]);
                }
            }),
            Cmd::ConfigureUser => self.configure_user(),
            Cmd::OpenFolder => self.open_folder(),
            Cmd::OpenRecent => self.open_recent(),
            Cmd::NewWindow => self.new_window(None),
            Cmd::Clone => self.clone_repo(),
            Cmd::Init => self.init_repo(),
            Cmd::OpenExplorer => {
                if let Some(r) = self.folder() {
                    win::shell_open(&r.display().to_string());
                }
            }
            Cmd::RevealInExplorer => match self.tab_reveal_path(self.tab_active) {
                Some(p) => self.reveal_in_explorer(&p),
                None => self.info("There is no file in the active editor to reveal."),
            },
            Cmd::OpenTerminal => {
                if let Some(r) = self.folder() {
                    let d = r.display().to_string();
                    win::shell_exec("cmd.exe", "", &d);
                }
            }
            Cmd::ToggleOutput => {
                if self.st.output_visible && !self.st.panel_terminal {
                    self.st.output_visible = false;
                } else {
                    self.st.output_visible = true;
                    self.st.panel_terminal = false;
                }
                if self.focus == Focus::Terminal && !self.term_editor_focus {
                    self.focus = self.side_focus();
                }
                self.st.save();
                self.layout();
            }
            Cmd::ToggleTerminal => self.toggle_terminal(),
            Cmd::NewTerminal => self.new_terminal(None),
            Cmd::NewTerminalEditor => self.new_editor_terminal(None),
            Cmd::ToggleEditorTerminal => self.toggle_editor_terminal(),
            Cmd::MoveTerminalToEditor => match self.panel_term() {
                Some(i) => self.move_terminal_to_editor(i),
                None => self.new_editor_terminal(None),
            },
            Cmd::MoveTerminalToPanel => match self.editor_term() {
                Some(i) => self.move_terminal_to_panel(i),
                None => self.info("The active editor is not a terminal."),
            },
            Cmd::KillTerminal => self.kill_terminal(),
            Cmd::FocusNextTerminal => self.focus_next_terminal(1),
            Cmd::FocusPrevTerminal => self.focus_next_terminal(-1),
            Cmd::RenameTerminal => {
                if self.term_target().is_none() {
                    self.show_terminal(true);
                }
                if let Some(i) = self.term_target() {
                    self.rename_terminal(i);
                }
            }
            Cmd::SelectShell => self.select_shell(),
            Cmd::ClaudeOpen => self.open_agent(&crate::term::CLAUDE),
            Cmd::ClaudeNew => self.new_agent_session(&crate::term::CLAUDE),
            Cmd::CodexOpen => self.open_agent(&crate::term::CODEX),
            Cmd::CodexNew => self.new_agent_session(&crate::term::CODEX),
            Cmd::ClearTerminal => {
                if let Some(t) = self.term_mut() {
                    t.screen.scrollback.clear();
                }
                self.term_write(b"\x0c");
            }
            Cmd::ToggleTheme => self.toggle_theme(),
            Cmd::ToggleTreeView => {
                self.st.tree_view = !self.st.tree_view;
                self.st.save();
                self.rebuild_changes();
                self.clamp_scroll(Area::Changes);
                self.invalidate();
            }
            Cmd::CloseEditor => self.close_active_tab(),
            Cmd::CloseOtherEditors => self.close_other_tabs(self.tab_active),
            Cmd::CloseAllEditors => self.close_all_tabs(),
            Cmd::KeepEditor => self.keep_tab(self.tab_active),
            Cmd::PinEditor => self.set_tab_pinned(self.tab_active, true),
            Cmd::UnpinEditor => self.set_tab_pinned(self.tab_active, false),
            Cmd::NextEditor => self.cycle_tab(1),
            Cmd::PrevEditor => self.cycle_tab(-1),
            Cmd::TogglePanelMax => self.toggle_panel_max(),
            Cmd::PanelRestore => self.set_panel_mode(PanelMode::Normal),
            Cmd::PanelFullWidth => self.set_panel_mode(PanelMode::FullWidth),
            Cmd::PanelFullHeight => self.set_panel_mode(PanelMode::FullHeight),
            Cmd::PanelFullScreen => self.set_panel_mode(PanelMode::FullScreen),
            Cmd::ToggleGraphSection => self.toggle_graph_section(),
            Cmd::ToggleBranchesSection => self.toggle_branches_section(),
            Cmd::ToggleSidebarPos => {
                self.st.sidebar_right = !self.st.sidebar_right;
                self.st.save();
                self.layout();
            }
            Cmd::ToggleInline => {
                self.st.inline_diff = !self.st.inline_diff;
                self.st.save();
                self.rebuild_rows();
            }
            Cmd::ToggleCompact => {
                self.st.compact_diff = !self.st.compact_diff;
                self.st.save();
                self.reload_view();
            }
            Cmd::ToggleGraphAll => {
                self.st.graph_all = !self.st.graph_all;
                self.st.save();
                self.log_key.clear();
                self.load_log(false);
            }
            Cmd::NextChange => self.goto_change(1),
            Cmd::PrevChange => self.goto_change(-1),
            Cmd::FocusCommit => {
                if !self.st.sidebar_visible || self.st.side_view != 0 {
                    self.st.side_view = 0;
                    self.set_sidebar_visible(true);
                }
                unsafe {
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(self.commit_edit);
                }
            }
            Cmd::ToggleSidebar => self.set_sidebar_visible(!self.st.sidebar_visible),
            Cmd::ShowExplorer | Cmd::ShowScm => {
                let v = if c == Cmd::ShowExplorer { 1 } else { 0 };
                if !(self.st.sidebar_visible && self.st.side_view == v) {
                    self.set_side_view(v);
                }
                self.focus = if v == 1 { Focus::Explorer } else { Focus::Changes };
            }
            Cmd::FindInFiles => self.find_in_files(false),
            Cmd::ReplaceInFiles => self.find_in_files(true),
            Cmd::ClearSearch => self.search_clear(),
            Cmd::GotoFile => self.goto_file(),
            Cmd::GotoLine => self.goto_line_pick(),
            Cmd::ExplorerCollapse => self.explorer_collapse_all(),
            Cmd::ToggleWordWrap => self.toggle_word_wrap(),
            Cmd::Find => self.find_open(false),
            Cmd::Replace => self.find_open(true),
            Cmd::FindNext => self.find_move(false),
            Cmd::FindPrev => self.find_move(true),
            Cmd::FindNextSel => self.find_selection(false),
            Cmd::FindPrevSel => self.find_selection(true),
            // without the widget there is no replacement text yet: it opens instead
            Cmd::ReplaceAll if self.find_visible() && self.find.replace => self.find_replace_all(),
            Cmd::ReplaceAll => self.find_open(true),
            Cmd::Save => self.save_active(),
            Cmd::SaveAll => self.save_all(),
            Cmd::RevertFile => self.revert_file(),
            Cmd::ExplorerNewFile => self.explorer_new(self.explorer_target(), false),
            Cmd::ExplorerNewFolder => self.explorer_new(self.explorer_target(), true),
            Cmd::ExplorerRename => {
                if let Some(i) = self.ex.sel_index() {
                    self.explorer_rename(i);
                }
            }
            Cmd::ExplorerDelete => {
                if let Some(i) = self.ex.sel_index() {
                    self.explorer_delete(i);
                }
            }
            Cmd::ClearOutput => {
                git::clear_log();
            }
            Cmd::About => {
                let v = self.git_version.clone().unwrap_or_else(|| "not found".into());
                self.confirm(&format!("DevDeck {}", env!("CARGO_PKG_VERSION")), &format!("A native, ultra-light Git client modeled on the VS Code Source Control view.\n\nGit: {v}\nRenderer: Win32 / GDI"), &[], |_, _| {});
            }
        }
        self.invalidate();
    }

    pub fn reload_view(&mut self) {
        let change = match &self.view {
            MainView::Diff(d) if d.back.is_none() => d.change.clone(),
            MainView::File(f) => f.change.clone(),
            _ => None,
        };
        if let Some(c) = change {
            self.load_change_view(c, true);
        } else if let MainView::Diff(d) = &self.view {
            if let Some(b) = &d.back {
                if let MainView::Commit(cv) = b.as_ref() {
                    if let Some(i) = cv.sel {
                        self.go_back();
                        self.open_commit_file(i);
                    }
                }
            }
        }
    }

    pub fn goto_change(&mut self, dir: i32) {
        let lh = self.line_h();
        let body_h = crate::win::rh(&self.l.main_body) - self.s(26);
        let scroll = self.scroll[Area::Main as usize];
        // the changes of a diff, or the conflicts of a file with merge conflicts (their action
        // rows: conflict j's is row start + j)
        let (starts, nav): (Vec<usize>, Option<usize>) = match &self.view {
            MainView::Diff(d) => (d.blocks.iter().map(|b| b.first_row).collect(), d.nav),
            MainView::File(f) => (f.conflicts.iter().enumerate().map(|(j, c)| c.start + j).collect(), f.nav),
            _ => return,
        };
        if starts.is_empty() {
            return;
        }
        // like VS Code, go from the change visited last while it is on screen, else from a
        // third down the view (where a visited change is placed)
        // in rows of the view (visual rows with word wrap)
        let wm = if self.wrap_map().is_some() { self.wrap.as_ref() } else { None };
        let row_of = |v: i32| crate::wrap::row_at(wm, (v / lh.max(1)).max(0) as usize).0;
        let (top, bottom) = (row_of(scroll), row_of(scroll + body_h));
        let cur = nav.filter(|&r| r >= top && r < bottom).unwrap_or(row_of(scroll + body_h / 3));
        let target = if dir > 0 { starts.iter().copied().find(|&s| s > cur).unwrap_or(starts[0]) } else { starts.iter().rev().copied().find(|&s| s < cur).unwrap_or(*starts.last().unwrap()) };
        match &mut self.view {
            MainView::Diff(d) => d.nav = Some(target),
            MainView::File(f) => f.nav = Some(target),
            _ => {}
        }
        let y = (crate::wrap::vrow(wm, target) as i32 * lh - body_h / 3).max(0);
        self.scroll[Area::Main as usize] = y;
        self.clamp_scroll(Area::Main);
        self.invalidate();
    }

    pub fn line_h(&self) -> i32 {
        self.fonts.mono_h + self.s(3)
    }
}

pub fn short_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

pub fn sanitize_branch(s: &str) -> String {
    let mut out = String::new();
    for ch in s.trim().chars() {
        out.push(match ch {
            ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\' => '-',
            c => c,
        });
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    out.trim_matches(|c| c == '/' || c == '.' || c == '-').trim_end_matches(".lock").to_string()
}

/// The operation in progress as (label, git subcommand), e.g. ("Cherry Pick", "cherry-pick").
/// Rebase first: a conflicted rebase step can also leave merge/cherry-pick state behind.
pub fn op_in_progress(s: &Status) -> Option<(&'static str, &'static str)> {
    if s.rebasing {
        Some(("Rebase", "rebase"))
    } else if s.cherry_picking {
        Some(("Cherry Pick", "cherry-pick"))
    } else if s.reverting {
        Some(("Revert", "revert"))
    } else if s.merging {
        Some(("Merge", "merge"))
    } else {
        None
    }
}

/// `git stash pop|apply --index`, retried without --index only when the staged state can't be
/// restored. Other failures (e.g. a merge that stopped on conflicts) already touched the tree,
/// so a retry would only bury the real error under "needs merge".
pub fn stash_restore(root: &Path, verb: &str, r: Option<&str>) -> GitResult<String> {
    let mut a = vec!["stash", verb, "--index"];
    a.extend(r);
    git::exec(root, &a).or_else(|e| {
        if !e.stderr.contains("Try without --index") {
            return Err(e);
        }
        a.remove(2);
        git::exec(root, &a)
    })
}

/// Resolves conflicted files to one side ("--ours"/"--theirs"). When that side deleted the file
/// (no stage for it, so checkout would fail) the resolution is the deletion.
pub fn resolve_side(root: &Path, files: &[(String, String)], side: &str) -> GitResult<String> {
    for (p, xy) in files {
        // per git's unmerged codes: these lack stage 2 (ours) / stage 3 (theirs)
        let gone = if side == "--ours" { matches!(xy.as_str(), "DD" | "UA" | "DU") } else { matches!(xy.as_str(), "DD" | "AU" | "UD") };
        if gone {
            git::exec(root, &["--literal-pathspecs", "rm", "-q", "--", p])?;
        } else {
            git::exec(root, &["--literal-pathspecs", "checkout", side, "--", p])?;
            git::exec(root, &["--literal-pathspecs", "add", "--", p])?;
        }
    }
    Ok(String::new())
}

/// Splits paths into groups that fit on one command line (CreateProcess takes at most 32,767
/// characters, and long, deep paths reach that well before a few hundred of them).
fn path_chunks(paths: &[String]) -> Vec<&[String]> {
    let mut v = Vec::new();
    let (mut start, mut len) = (0, 0);
    for (i, p) in paths.iter().enumerate() {
        let n = p.encode_utf16().count() + 3;
        if i > start && len + n > 24_000 {
            v.push(&paths[start..i]);
            start = i;
            len = 0;
        }
        len += n;
    }
    if start < paths.len() {
        v.push(&paths[start..]);
    }
    v
}

/// Restores tracked files and recycles untracked ones; errors when some couldn't be removed
/// (e.g. locked by another process) instead of failing silently.
pub fn discard_paths(root: &Path, tracked: &[String], untracked: &[String]) -> GitResult<String> {
    for chunk in path_chunks(tracked) {
        // literal: a file named app/[id]/x must not also match app/i/x
        let mut a = vec!["--literal-pathspecs", "checkout", "-q", "--"];
        a.extend(chunk.iter().map(|s| s.as_str()));
        git::exec(root, &a)?;
    }
    if untracked.is_empty() {
        return Ok(String::new());
    }
    let abs: Vec<PathBuf> = untracked.iter().map(|p| root.join(p)).collect();
    let exists = |p: &&PathBuf| std::fs::symlink_metadata(p).is_ok();
    // declined deleting for good (no Recycle Bin there): not asked again for each file
    if recycle_many(&abs) == Some(false) {
        return Err(GitError::new("The files were not deleted."));
    }
    // the shell stops at the first failure: give the rest their own try
    let left: Vec<&PathBuf> = abs.iter().filter(exists).filter(|p| !recycle(p)).collect();
    if left.is_empty() {
        return Ok(String::new());
    }
    let names: Vec<String> = left.iter().take(3).map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string()).collect();
    let more = if left.len() > 3 { ", ..." } else { "" };
    Err(GitError::new(format!("Could not move {} file(s) to the Recycle Bin (in use, or the path is too long): {}{more}", left.len(), names.join(", "))))
}

/// Moves many files to the Recycle Bin with one shell call (one call per file is slow).
/// Some(true) when all went, Some(false) when the user cancelled, None on another failure.
pub fn recycle_many(paths: &[PathBuf]) -> Option<bool> {
    use windows_sys::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FO_DELETE, SHFILEOPSTRUCTW};
    if paths.is_empty() {
        return Some(true);
    }
    // double-NUL terminated list of NUL separated paths, with backslashes
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.display().to_string().replace('/', "\\").encode_utf16());
        from.push(0);
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        // where the bin isn't available (network drives, too big) the shell asks before deleting for good
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT | FOF_WANTNUKEWARNING) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if op.fAnyOperationsAborted != 0 {
        Some(false)
    } else {
        (r == 0).then_some(true)
    }
}

/// Moves a file or folder to the Recycle Bin; false when that failed. Never falls back to
/// deleting for good: callers ask first, then use `delete_permanently`.
pub fn recycle(path: &Path) -> bool {
    use windows_sys::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FO_DELETE, SHFILEOPSTRUCTW};
    // the shell wants backslashes
    let mut from: Vec<u16> = path.display().to_string().replace('/', "\\").encode_utf16().collect();
    from.push(0);
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        // where the bin isn't available (network drives, too big) the shell asks before deleting for good
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT | FOF_WANTNUKEWARNING) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    r == 0 && op.fAnyOperationsAborted == 0 && std::fs::symlink_metadata(path).is_err()
}

/// Deletes a file or folder for good. Returns false when it still exists afterwards.
pub fn delete_permanently(path: &Path) -> bool {
    let _ = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    std::fs::symlink_metadata(path).is_err()
}

pub fn diff_view_from(text: &str, f: &FileChange, kind: DiffKind, inline: bool, title: &str) -> MainView {
    let fd = diff::parse(text);
    if fd.binary {
        return MainView::Message(title.into(), "The file is not displayed because it is binary.".into());
    }
    if fd.hunks.is_empty() {
        let has = |p: &str| fd.header.iter().any(|h| h.starts_with(p));
        let msg = if has("new file mode") || f.kind == Kind::Untracked {
            "Empty file added."
        } else if has("deleted file mode") {
            "Empty file deleted."
        } else {
            "No textual changes (mode, rename or whitespace only)."
        };
        return MainView::Message(title.into(), msg.into());
    }
    let (rows, blocks) = diff::build_rows(&fd, inline);
    let max_cols = fd.hunks.iter().flat_map(|h| h.lines.iter()).map(|l| l.text.chars().count() + l.text.matches('\t').count() * 3).max().unwrap_or(0);
    MainView::Diff(DiffView { title: title.into(), desc: f.path.clone(), path: f.path.clone(), kind, change: Some(f.clone()), fd, rows, blocks, sel: None, back: None, max_cols, inline, nav: None, tsel: None, uid: Default::default() })
}

pub fn build_change_view(root: &Path, f: &FileChange, ctx: &str, inline: bool) -> MainView {
    let name = short_name(&f.path).to_string();
    // like the file viewer, a huge file (a log, a dump) isn't diffed with full context on
    // every refresh
    let size = std::fs::metadata(root.join(&f.path)).map(|m| m.len()).unwrap_or(0);
    if size > crate::explorer::MAX_VIEW_BYTES {
        return MainView::Message(name, format!("The file is not displayed because it is too large ({:.1} MB).", size as f64 / 1048576.0));
    }
    match (f.group, f.kind) {
        (Group::Merge, _) => {
            let bytes = std::fs::read(root.join(&f.path)).unwrap_or_default();
            if git::is_binary(&bytes) {
                return MainView::Message(name, "Binary file with conflicts. Stage it to mark as resolved.".into());
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let lines: Vec<String> = text.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
            let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
            let conflicts = diff::parse_conflicts(&refs);
            MainView::File(FileView { title: format!("{name} (Merge Conflicts)"), path: f.path.clone(), lines, conflicts, change: Some(f.clone()), mtime: None, ed: Default::default(), nav: None })
        }
        (Group::Changes, Kind::Untracked) => {
            let o = git::run(root, &["--literal-pathspecs", "diff", "--no-index", "--no-color", "--no-ext-diff", "--no-textconv", ctx, "--", "/dev/null", &f.path], None, false);
            diff_view_from(&o.text(), f, DiffKind::Untracked, inline, &format!("{name} (Untracked)"))
        }
        (Group::Changes, _) => {
            let o = git::run(root, &["--literal-pathspecs", "diff", "--no-color", "--no-ext-diff", "--no-textconv", ctx, "--", &f.path], None, false);
            let suffix = if f.kind == Kind::Deleted { "Deleted" } else { "Working Tree" };
            diff_view_from(&o.text(), f, DiffKind::WorkingTree, inline, &format!("{name} ({suffix})"))
        }
        (Group::Staged, _) => {
            let mut a = vec!["--literal-pathspecs", "diff", "--cached", "--no-color", "--no-ext-diff", "--no-textconv", "-M", ctx, "--"];
            if let Some(o) = &f.orig {
                a.push(o);
            }
            a.push(&f.path);
            let o = git::run(root, &a, None, false);
            diff_view_from(&o.text(), f, DiffKind::Index, inline, &format!("{name} (Index)"))
        }
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    fn change(path: &str) -> FileChange {
        FileChange { path: path.into(), orig: None, kind: Kind::Modified, group: Group::Changes, xy: ".M".into() }
    }

    /// Renders the rows as "depth:label" strings.
    fn rows(paths: &[&str], collapsed: &[&str]) -> Vec<String> {
        let files: Vec<FileChange> = paths.iter().map(|p| change(p)).collect();
        let mut node = DirNode::default();
        for (i, f) in files.iter().enumerate() {
            node.insert(&f.path, i);
        }
        let collapsed: HashSet<(u8, String)> = collapsed.iter().map(|p| (group_id(Group::Changes), p.to_string())).collect();
        let (mut r, mut d, mut dirs) = (Vec::new(), Vec::new(), Vec::new());
        node.emit(Group::Changes, "", 0, &files, &collapsed, &mut r, &mut d, &mut dirs);
        assert_eq!(r.len(), d.len());
        r.iter()
            .zip(&d)
            .map(|(row, depth)| match row {
                CRow::Dir(i) => format!("{depth}:{}/", dirs[*i].label),
                CRow::File(i) => format!("{depth}:{}", short_name(&files[*i].path)),
                CRow::Group(_) => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn folders_first_sorted_and_compacted() {
        let got = rows(&["src/ui.rs", "README.md", "src/app.rs", "docs/a/b/c.md", "Cargo.toml", "src/deep/x/y.rs"], &[]);
        assert_eq!(got, vec!["0:docs/a/b/", "1:c.md", "0:src/", "1:deep/x/", "2:y.rs", "1:app.rs", "1:ui.rs", "0:Cargo.toml", "0:README.md"]);
    }

    #[test]
    fn collapsed_folders_hide_their_contents() {
        let got = rows(&["src/ui.rs", "src/deep/x/y.rs", "top.txt"], &["src/deep/x"]);
        assert_eq!(got, vec!["0:src/", "1:deep/x/", "1:ui.rs", "0:top.txt"]);
        let got = rows(&["src/ui.rs", "src/deep/x/y.rs", "top.txt"], &["src"]);
        assert_eq!(got, vec!["0:src/", "0:top.txt"]);
    }
}
