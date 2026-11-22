//! Tiny key=value settings file in %APPDATA%\DevDeck\settings.ini.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Settings {
    pub dark: bool,
    pub inline_diff: bool,
    pub compact_diff: bool,
    /// Word wrap in file views and diffs.
    pub word_wrap: bool,
    pub sidebar_w: i32,
    /// Activity bar and side bar on the right edge (VS Code "Toggle Primary Side Bar Position").
    pub sidebar_right: bool,
    /// Primary side bar shown (VS Code "Toggle Primary Side Bar Visibility", Ctrl+B).
    pub sidebar_visible: bool,
    /// Side bar view: 0 Source Control, 1 Explorer.
    pub side_view: u8,
    /// Show the Changes list as a folder tree (VS Code "View as Tree").
    pub tree_view: bool,
    pub graph_split: f32,
    /// Branches section expanded, and its list height (unscaled pixels).
    pub branches_open: bool,
    /// Graph section expanded.
    pub graph_open: bool,
    /// Bottom panel size (`PanelMode` as u8) and the maximized mode the toggle button restores.
    pub panel_mode: u8,
    pub panel_max: u8,
    pub branches_h: i32,
    pub output_h: i32,
    pub output_visible: bool,
    pub graph_all: bool,
    pub smart_commit: Option<bool>,
    pub confirm_sync: bool,
    pub recent: Vec<String>,
    /// Titles of the commands run from the Command Palette, most recent first.
    pub recent_cmds: Vec<String>,
    /// Recent folders and commands added in this window (most recent first) and folders it
    /// removed: saving applies them to the lists in the file, which other windows update too.
    pub added: Vec<String>,
    pub added_cmds: Vec<String>,
    pub forget: Vec<String>,
    pub panel_terminal: bool,
    pub terminal_shell: String,
    pub window: Option<(i32, i32, i32, i32, bool)>,
    /// The values as loaded or last saved: saving writes only what this window changed since
    /// and keeps the rest from the file, so closing one window doesn't undo a setting (theme,
    /// layout ...) another window changed meanwhile.
    base: std::cell::RefCell<Vec<(&'static str, String)>>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dark: true,
            inline_diff: false,
            compact_diff: false,
            word_wrap: false,
            sidebar_w: 320,
            sidebar_right: false,
            sidebar_visible: true,
            side_view: 0,
            tree_view: false,
            graph_split: 0.55,
            branches_open: true,
            graph_open: true,
            panel_mode: 0,
            panel_max: 2,
            branches_h: 140,
            output_h: 180,
            output_visible: false,
            graph_all: false,
            smart_commit: None,
            confirm_sync: true,
            recent: Vec::new(),
            recent_cmds: Vec::new(),
            added: Vec::new(),
            added_cmds: Vec::new(),
            forget: Vec::new(),
            panel_terminal: false,
            terminal_shell: String::new(),
            window: None,
            base: Default::default(),
        }
    }
}

fn path() -> PathBuf {
    let base = PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into()));
    let p = base.join("DevDeck").join("settings.ini");
    // carry the settings over from the app's old name ("GitGui") once
    if !p.exists() {
        let old = base.join("GitGui").join("settings.ini");
        if old.exists() {
            let _ = std::fs::create_dir_all(base.join("DevDeck"));
            let _ = std::fs::copy(&old, &p);
        }
    }
    p
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Ok(text) = std::fs::read_to_string(path()) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let b = v == "1" || v == "true";
            match k.trim() {
                "dark" => s.dark = b,
                "inline_diff" => s.inline_diff = b,
                "compact_diff" => s.compact_diff = b,
                "wordwrap" => s.word_wrap = b,
                "sidebar_w" => s.sidebar_w = v.parse().unwrap_or(320).clamp(180, 2000),
                "sidebar_right" => s.sidebar_right = b,
                "sidebar_visible" => s.sidebar_visible = b,
                "side_view" => s.side_view = v.parse().unwrap_or(0).min(2),
                "tree_view" => s.tree_view = b,
                "graph_split" => s.graph_split = v.parse().unwrap_or(0.55f32).clamp(0.1, 0.9),
                "branches_open" => s.branches_open = b,
                "graph_open" => s.graph_open = b,
                "panel_mode" => s.panel_mode = v.parse().unwrap_or(0).min(3),
                "panel_max" => s.panel_max = v.parse().unwrap_or(2).clamp(1, 3),
                "branches_h" => s.branches_h = v.parse().unwrap_or(140).clamp(44, 2000),
                "output_h" => s.output_h = v.parse().unwrap_or(180).clamp(60, 2000),
                "output_visible" => s.output_visible = b,
                "graph_all" => s.graph_all = b,
                "smart_commit" => s.smart_commit = if v.is_empty() { None } else { Some(b) },
                "confirm_sync" => s.confirm_sync = b,
                "panel_terminal" => s.panel_terminal = b,
                "terminal_shell" => s.terminal_shell = v.to_string(),
                "recent" => s.recent = v.split('|').filter(|x| !x.is_empty()).map(String::from).collect(),
                "recent_cmds" => s.recent_cmds = v.split('|').filter(|x| !x.is_empty()).map(String::from).collect(),
                "window" => {
                    let p: Vec<i32> = v.split(',').filter_map(|x| x.parse().ok()).collect();
                    if p.len() == 5 {
                        s.window = Some((p[0], p[1], p[2], p[3], p[4] != 0));
                    }
                }
                _ => {}
            }
        }
        *s.base.borrow_mut() = s.values();
        s
    }

    /// Every setting but the recent lists, as written to the file.
    fn values(&self) -> Vec<(&'static str, String)> {
        let b = |x: bool| if x { "1" } else { "0" }.to_string();
        let mut v = vec![
            ("dark", b(self.dark)),
            ("inline_diff", b(self.inline_diff)),
            ("compact_diff", b(self.compact_diff)),
            ("wordwrap", b(self.word_wrap)),
            ("sidebar_w", self.sidebar_w.to_string()),
            ("sidebar_right", b(self.sidebar_right)),
            ("sidebar_visible", b(self.sidebar_visible)),
            ("side_view", self.side_view.to_string()),
            ("tree_view", b(self.tree_view)),
            ("graph_split", self.graph_split.to_string()),
            ("branches_open", b(self.branches_open)),
            ("graph_open", b(self.graph_open)),
            ("panel_mode", self.panel_mode.to_string()),
            ("panel_max", self.panel_max.to_string()),
            ("branches_h", self.branches_h.to_string()),
            ("output_h", self.output_h.to_string()),
            ("output_visible", b(self.output_visible)),
            ("graph_all", b(self.graph_all)),
            ("smart_commit", self.smart_commit.map(b).unwrap_or_default()),
            ("confirm_sync", b(self.confirm_sync)),
            ("panel_terminal", b(self.panel_terminal)),
            ("terminal_shell", self.terminal_shell.clone()),
        ];
        if let Some((x, y, w, h, m)) = self.window {
            v.push(("window", format!("{x},{y},{w},{h},{}", if m { 1 } else { 0 })));
        }
        v
    }

    pub fn save(&self) {
        let p = path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let disk_text = std::fs::read_to_string(&p).unwrap_or_default();
        let disk: Vec<(&str, &str)> = disk_text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v)).collect();
        let (recent, recent_cmds) = self.merged_recent();
        let mine = self.values();
        let mut base = self.base.borrow_mut();
        let mut out = String::new();
        for (k, v) in &mine {
            let changed = base.iter().find(|(bk, _)| bk == k).map(|(_, bv)| bv != v).unwrap_or(true);
            let val = if changed { v.as_str() } else { disk.iter().find(|(dk, _)| dk == k).map(|(_, dv)| *dv).unwrap_or(v) };
            out += &format!("{k}={val}\n");
        }
        out += &format!("recent={}\n", recent.join("|"));
        out += &format!("recent_cmds={}\n", recent_cmds.join("|"));
        *base = mine;
        // write a temporary file and rename it over the settings, so a window starting at the
        // same moment never reads a half-written file and falls back to the defaults
        let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
        // flushed before the rename: after a power loss the rename could otherwise survive
        // without the data, leaving an empty settings file
        let written = std::fs::File::create(&tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(out.as_bytes())?;
            f.sync_all()
        });
        // the rename fails while another program (antivirus, indexer) has the file open: retry once
        if written.is_err() || (std::fs::rename(&tmp, &p).is_err() && {
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::fs::rename(&tmp, &p).is_err()
        }) {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// The recent folders and commands: the file's lists (other windows add to and remove from
    /// them too) with this window's own changes applied on top.
    fn merged_recent(&self) -> (Vec<String>, Vec<String>) {
        let disk = Settings::load();
        let merge = |mine: &[String], theirs: &[String], max: usize| {
            let mut v: Vec<String> = Vec::new();
            for t in mine.iter().chain(theirs) {
                if !v.iter().chain(&self.forget).any(|m| m.eq_ignore_ascii_case(t)) {
                    v.push(t.clone());
                }
            }
            v.truncate(max);
            v
        };
        (merge(&self.added, &disk.recent, 15), merge(&self.added_cmds, &disk.recent_cmds, 10))
    }

    /// Picks up the recent folders and commands other windows saved (before showing them).
    pub fn refresh_recent(&mut self) {
        (self.recent, self.recent_cmds) = self.merged_recent();
    }

    pub fn remove_recent(&mut self, folder: &str) {
        self.recent.retain(|r| !r.eq_ignore_ascii_case(folder));
        self.added.retain(|r| !r.eq_ignore_ascii_case(folder));
        self.forget.push(folder.to_string());
    }

    pub fn add_recent(&mut self, folder: &str) {
        for v in [&mut self.recent, &mut self.added, &mut self.forget] {
            v.retain(|r| !r.eq_ignore_ascii_case(folder));
        }
        self.recent.insert(0, folder.to_string());
        self.recent.truncate(15);
        self.added.insert(0, folder.to_string());
        self.added.truncate(15);
    }

    pub fn add_recent_cmd(&mut self, title: &str) {
        self.recent_cmds.retain(|r| r != title);
        self.recent_cmds.insert(0, title.to_string());
        self.recent_cmds.truncate(10);
        self.added_cmds.retain(|r| r != title);
        self.added_cmds.insert(0, title.to_string());
        self.added_cmds.truncate(10);
    }
}
