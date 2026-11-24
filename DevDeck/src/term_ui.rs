//! Integrated terminal: session lifecycle, input, selection and painting. A terminal lives
//! either in the bottom panel or in an editor tab (VS Code's "Terminal in Editor Area").

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};

use crate::app::*;
use crate::term::{self, Color, Pty, Screen, Shell, TermEvent, BOLD, DIM, INVERSE, UNDERLINE};
use crate::win::{self, rect, rh, rw, Gfx};

/// Selection as (anchor, cursor) absolute (line, col) positions.
pub type TermSel = Option<((usize, usize), (usize, usize))>;

pub struct TermSession {
    /// Stable identity (indexes shift when terminals are killed); editor tabs use `term:{id}`.
    pub id: u64,
    pub pty: Option<Pty>,
    pub rx: Receiver<TermEvent>,
    pub screen: Screen,
    pub shell: Shell,
    pub exited: Option<String>,
    /// Name given with "Rename...", shown instead of the profile name.
    pub custom_name: Option<String>,
    /// Shown in an editor tab instead of the panel.
    pub editor: bool,
    /// History scroll offset in lines (0 = following the output).
    pub scroll: usize,
    pub sel: TermSel,
    /// Working directory it was started in when not the open folder ("Open in Integrated
    /// Terminal"); a restart after the shell exits starts there again.
    pub cwd: Option<PathBuf>,
}

impl TermSession {
    pub fn name(&self) -> &str {
        self.custom_name.as_deref().unwrap_or(&self.shell.name)
    }
}

/// Editor tab key of terminal `id`.
pub fn term_key(id: u64) -> String {
    format!("term:{id}")
}

impl App {
    pub fn terminal_visible(&self) -> bool {
        self.st.output_visible && self.st.panel_terminal
    }

    pub fn term_index(&self, id: u64) -> Option<usize> {
        self.terms.iter().position(|t| t.id == id)
    }

    /// The terminal shown in the active editor tab.
    pub fn editor_term(&self) -> Option<usize> {
        match self.view {
            MainView::Terminal(id) => self.term_index(id),
            _ => None,
        }
    }

    /// The terminal shown in the panel (`term_active`, when it is a panel terminal).
    pub fn panel_term(&self) -> Option<usize> {
        self.terms.get(self.term_active).filter(|t| !t.editor).map(|_| self.term_active)
    }

    pub fn panel_term_count(&self) -> usize {
        self.terms.iter().filter(|t| !t.editor).count()
    }

    /// True when the terminal that gets input is the one in the editor area.
    fn target_in_editor(&self) -> bool {
        self.term_editor_focus && self.editor_term().is_some()
    }

    /// The terminal keyboard input and terminal commands go to: the editor terminal when it
    /// was focused last (and is still shown), else the panel's.
    pub fn term_target(&self) -> Option<usize> {
        if self.target_in_editor() {
            self.editor_term()
        } else {
            self.panel_term()
        }
    }

    pub fn term(&self) -> Option<&TermSession> {
        self.term_target().map(|i| &self.terms[i])
    }

    pub fn term_mut(&mut self) -> Option<&mut TermSession> {
        self.term_target().map(|i| &mut self.terms[i])
    }

    /// Whether keys and characters go to a terminal.
    pub fn term_has_focus(&self) -> bool {
        if self.focus != Focus::Terminal || self.overlay.is_some() {
            return false;
        }
        if self.term_editor_focus {
            self.editor_term().is_some()
        } else {
            self.terminal_visible()
        }
    }

    pub fn term_cell(&self) -> (i32, i32) {
        (self.fonts.mono_w.max(1), self.fonts.mono_h.max(1))
    }

    /// Width of the terminal tab list, shown on the right of the panel once there are
    /// two or more panel terminals (VS Code's default "hide when there is a single terminal").
    pub fn term_tabs_w(&self) -> i32 {
        if self.panel_term_count() < 2 {
            return 0;
        }
        self.s(160).min(rw(&self.l.output_body) * 35 / 100)
    }

    pub fn term_tabs_rect(&self) -> RECT {
        let b = self.l.output_body;
        rect(b.right - self.term_tabs_w(), b.top, b.right, b.bottom)
    }

    /// Body of the editor area, or of the panel without its tab list.
    pub fn term_area_of(&self, editor: bool) -> RECT {
        if editor {
            return self.l.main_body;
        }
        let b = self.l.output_body;
        rect(b.left, b.top, b.right - self.term_tabs_w(), b.bottom)
    }

    /// Terminal grid area inside `term_area_of`.
    pub fn term_rect_of(&self, editor: bool) -> RECT {
        let b = self.term_area_of(editor);
        rect(b.left + self.s(20), b.top + self.s(2), b.right - self.s(14), b.bottom - self.s(2))
    }

    /// Grid area of the terminal that gets input.
    pub fn term_rect(&self) -> RECT {
        self.term_rect_of(self.target_in_editor())
    }

    /// Grid size for a terminal in the editor area or the panel; None while that area is hidden.
    fn grid_size_of(&self, editor: bool) -> Option<(usize, usize)> {
        let r = self.term_rect_of(editor);
        if rw(&r) <= 0 || rh(&r) <= 0 {
            return None;
        }
        let (cw, ch) = self.term_cell();
        Some((((rw(&r) / cw).max(20)) as usize, ((rh(&r) / ch).max(3)) as usize))
    }

    pub fn shells(&mut self) -> Vec<Shell> {
        if self.shells.is_empty() {
            self.shells = term::detect_shells();
        }
        self.shells.clone()
    }

    fn default_shell(&mut self) -> Shell {
        let shells = self.shells();
        shells.iter().find(|s| s.name == self.st.terminal_shell).or_else(|| shells.first()).cloned().unwrap_or(Shell { name: "Command Prompt".into(), cmd: "cmd.exe".into() })
    }

    /// Starts `shell` on a new pseudo console sized to the area it will be shown in, in `dir`
    /// (None: the open folder).
    fn spawn_session(&mut self, shell: Shell, editor: bool, dir: Option<PathBuf>) -> TermSession {
        let cwd = dir.clone().or_else(|| self.folder()).or_else(|| std::env::var_os("USERPROFILE").map(Into::into)).unwrap_or_else(|| ".".into());
        let (cols, rows) = self.grid_size_of(editor).unwrap_or((80, 24));
        let id = self.term_next_id;
        self.term_next_id += 1;
        match Pty::spawn(&shell.cmd, &cwd, cols as u16, rows as u16, self.hwnd, WM_TERM, self.term_pending.clone()) {
            Ok((pty, rx)) => TermSession { id, pty: Some(pty), rx, screen: Screen::new(cols, rows), shell, exited: None, custom_name: None, editor, scroll: 0, sel: None, cwd: dir },
            Err(e) => {
                let mut screen = Screen::new(cols, rows);
                screen.feed(format!("\x1b[31m{e}\x1b[0m\r\n").as_bytes());
                let (_, rx) = std::sync::mpsc::channel();
                self.error(e.clone());
                TermSession { id, pty: None, rx, screen, shell, exited: Some(e), custom_name: None, editor, scroll: 0, sel: None, cwd: dir }
            }
        }
    }

    /// Opens a new panel terminal (the default profile when `shell` is None) and makes it active.
    pub fn start_terminal(&mut self, shell: Option<Shell>) {
        self.start_terminal_in(shell, None);
    }

    /// `start_terminal` in `dir` (None: the open folder).
    fn start_terminal_in(&mut self, shell: Option<Shell>, dir: Option<PathBuf>) {
        let shell = shell.unwrap_or_else(|| self.default_shell());
        let s = self.spawn_session(shell, false, dir);
        self.terms.push(s);
        self.activate_terminal(self.terms.len() - 1);
        // the tab list appears with the second terminal, which narrows every grid
        self.resize_terminal();
        self.invalidate();
    }

    /// Shows the terminal panel and opens a new terminal in it.
    pub fn new_terminal(&mut self, shell: Option<Shell>) {
        self.reveal_terminal_panel(true);
        self.start_terminal(shell);
    }

    /// Explorer's "Open in Integrated Terminal": shows the panel and opens a new terminal
    /// (default profile) in `dir`.
    pub fn new_terminal_in(&mut self, dir: PathBuf) {
        self.reveal_terminal_panel(true);
        self.start_terminal_in(None, Some(dir));
    }

    /// Opens a new terminal in an editor tab ("Terminal: Create New Terminal in Editor Area").
    pub fn new_editor_terminal(&mut self, shell: Option<Shell>) {
        let shell = shell.unwrap_or_else(|| self.default_shell());
        let s = self.spawn_session(shell, true, None);
        let id = s.id;
        self.terms.push(s);
        self.show_editor_terminal(id);
    }

    /// Opens (or activates) the editor tab of terminal `id` and focuses it.
    fn show_editor_terminal(&mut self, id: u64) {
        // Ctrl+Shift+` goes back to the tab that was shown before the first terminal tab
        if !matches!(self.view, MainView::Terminal(_)) {
            self.term_editor_prev = self.tabs.get(self.tab_active).map(|t| t.key.clone());
        }
        self.term_editor_last = Some(id);
        // the editor area is hidden while the panel covers it
        if matches!(self.panel_mode(), PanelMode::FullHeight | PanelMode::FullScreen) {
            self.st.panel_mode = PanelMode::Normal as u8;
            self.st.save();
        }
        if !self.open_tab_beside(term_key(id)) {
            self.view = MainView::Terminal(id);
        }
        self.focus = Focus::Terminal;
        self.term_editor_focus = true;
        unsafe {
            SetFocus(self.hwnd);
        }
        self.layout();
        self.resize_terminal();
        self.invalidate();
    }

    /// Ctrl+Shift+`, the editor area's Ctrl+`: focuses the terminal tab that is shown, else
    /// brings back the terminal tab used last (or the first one), else opens a new terminal
    /// in the editor area. When a terminal tab already has focus it goes back to the tab that
    /// was active before, or to the side bar.
    pub fn toggle_editor_terminal(&mut self) {
        if let Some(i) = self.editor_term() {
            if self.focus != Focus::Terminal || !self.term_editor_focus {
                let id = self.terms[i].id;
                self.show_editor_terminal(id);
                return;
            }
            let back = self.term_editor_prev.clone().and_then(|k| self.tabs.iter().position(|t| t.key == k));
            match back {
                Some(ti) if ti != self.tab_active => self.activate_tab(ti),
                _ if self.st.sidebar_visible => {
                    self.focus = self.side_focus();
                    self.invalidate();
                }
                _ => {}
            }
            return;
        }
        let in_tab = |a: &App, id: u64| a.terms.iter().any(|t| t.id == id && t.editor) && a.tabs.iter().any(|t| t.key == term_key(id));
        let last = self.term_editor_last.filter(|&id| in_tab(self, id));
        let first = || self.tabs.iter().find_map(|t| t.key.strip_prefix("term:").and_then(|n| n.parse().ok())).filter(|&id| in_tab(self, id));
        match last.or_else(first) {
            Some(id) => self.show_editor_terminal(id),
            None => self.new_editor_terminal(None),
        }
    }

    /// Moves terminal `i` from the panel into an editor tab.
    pub fn move_terminal_to_editor(&mut self, i: usize) {
        let Some(t) = self.terms.get_mut(i) else { return };
        let id = t.id;
        if !t.editor {
            t.editor = true;
            t.sel = None;
            self.fix_term_active(i);
            // like closing the last terminal, moving it away hides the panel
            if self.panel_term().is_none() && self.terminal_visible() {
                self.st.output_visible = false;
                self.st.save();
            }
        }
        self.show_editor_terminal(id);
    }

    /// Moves terminal `i` from its editor tab back into the panel.
    pub fn move_terminal_to_panel(&mut self, i: usize) {
        let Some(t) = self.terms.get_mut(i) else { return };
        if t.editor {
            t.editor = false;
            t.sel = None;
            let key = term_key(t.id);
            // the session is a panel terminal now, so closing the tab keeps it running
            if let Some(ti) = self.tabs.iter().position(|t| t.key == key) {
                self.close_tab(ti);
            }
        }
        self.term_active = i;
        self.show_terminal(true);
        self.resize_terminal();
    }

    /// Restarts the terminal at `i` with the same profile (after its shell exited).
    fn restart_terminal(&mut self, i: usize) {
        let Some(old) = self.terms.get(i) else { return };
        let (shell, name, id, editor, dir) = (old.shell.clone(), old.custom_name.clone(), old.id, old.editor, old.cwd.clone());
        let mut s = self.spawn_session(shell, editor, dir);
        s.custom_name = name;
        s.id = id;
        self.terms[i] = s; // drops (closes) the old pseudo console
        self.invalidate();
    }

    /// Shows terminal `i`: in the panel, or its editor tab for an editor terminal.
    pub fn activate_terminal(&mut self, i: usize) {
        let Some(t) = self.terms.get(i) else { return };
        if t.editor {
            let id = t.id;
            self.show_editor_terminal(id);
            return;
        }
        self.term_active = i;
        self.invalidate();
    }

    /// Points `term_active` at a panel terminal: the current one, else the closest one at or
    /// after `near`, else before it.
    fn fix_term_active(&mut self, near: usize) {
        if self.panel_term().is_some() {
            return;
        }
        let n = self.terms.len();
        let after = (near.min(n)..n).find(|&k| !self.terms[k].editor);
        let before = (0..near.min(n)).rev().find(|&k| !self.terms[k].editor);
        self.term_active = after.or(before).unwrap_or(0);
    }

    /// Activates the next (`delta` 1) or previous (`delta` -1) panel terminal, wrapping around.
    pub fn focus_next_terminal(&mut self, delta: i32) {
        let panel: Vec<usize> = (0..self.terms.len()).filter(|&k| !self.terms[k].editor).collect();
        if panel.len() > 1 {
            let pos = panel.iter().position(|&k| k == self.term_active).unwrap_or(0) as i32;
            self.term_active = panel[(pos + delta).rem_euclid(panel.len() as i32) as usize];
        }
        self.show_terminal(true);
    }

    pub fn rename_terminal(&mut self, i: usize) {
        let Some(t) = self.terms.get(i) else { return };
        let (current, id) = (t.name().to_string(), t.id);
        self.input("Enter a new name for the terminal", &current, move |app, name| {
            if let Some(k) = app.term_index(id) {
                let name = name.trim();
                app.terms[k].custom_name = if name.is_empty() { None } else { Some(name.to_string()) };
            }
            app.invalidate();
        });
    }

    /// Makes the terminal panel visible without starting a terminal.
    fn reveal_terminal_panel(&mut self, focus: bool) {
        self.st.output_visible = true;
        self.st.panel_terminal = true;
        self.st.save();
        self.layout();
        if focus {
            self.focus = Focus::Terminal;
            self.term_editor_focus = false;
            unsafe {
                SetFocus(self.hwnd);
            }
        }
    }

    pub fn show_terminal(&mut self, focus: bool) {
        self.reveal_terminal_panel(focus);
        if self.panel_term().is_none() {
            if self.panel_term_count() > 0 {
                self.fix_term_active(self.term_active);
            } else {
                self.start_terminal(None);
            }
        }
        self.resize_terminal();
        self.invalidate();
    }

    pub fn toggle_terminal(&mut self) {
        if self.terminal_visible() && self.focus == Focus::Terminal && !self.term_editor_focus {
            self.st.output_visible = false;
            self.st.save();
            self.focus = self.side_focus();
            self.layout();
        } else {
            self.show_terminal(true);
        }
    }

    /// Kills the terminal that gets input (the active editor terminal or the panel's).
    pub fn kill_terminal(&mut self) {
        if let Some(i) = self.term_target() {
            self.kill_terminal_at(i);
        }
    }

    /// Kills one terminal instance; an editor terminal's tab is closed with it.
    pub fn kill_terminal_at(&mut self, i: usize) {
        let Some(t) = self.terms.get(i) else { return };
        if t.editor {
            let key = term_key(t.id);
            if let Some(ti) = self.tabs.iter().position(|t| t.key == key) {
                self.close_tab(ti); // removes the session too
                return;
            }
        }
        self.remove_term(i);
    }

    /// Drops terminal `i` (closing its pseudo console). Removing the last panel terminal
    /// also hides the panel.
    pub fn remove_term(&mut self, i: usize) {
        if i >= self.terms.len() {
            return;
        }
        let was_panel = !self.terms[i].editor;
        self.terms.remove(i);
        if i < self.term_active {
            self.term_active -= 1;
        }
        self.fix_term_active(i);
        if was_panel && self.panel_term_count() == 0 && self.terminal_visible() {
            self.st.output_visible = false;
            self.st.save();
            if self.focus == Focus::Terminal && !self.term_editor_focus {
                self.focus = self.side_focus();
            }
            self.layout();
        }
        // the panel's tab list disappears with the second to last terminal
        self.resize_terminal();
        self.invalidate();
    }

    /// Kills editor terminals whose tab is gone (after closing several tabs at once).
    pub fn prune_editor_terms(&mut self) {
        let mut k = 0;
        while k < self.terms.len() {
            let key = term_key(self.terms[k].id);
            if self.terms[k].editor && !self.tabs.iter().any(|t| t.key == key) {
                self.remove_term(k);
            } else {
                k += 1;
            }
        }
    }

    /// Keeps the keyboard focus in step with the active editor tab: a terminal tab that
    /// becomes active while the editor area has focus takes it, and focus leaves a terminal
    /// tab that is no longer shown.
    pub fn sync_term_focus(&mut self) {
        let shown = self.editor_term().is_some();
        if let MainView::Terminal(id) = self.view {
            self.term_editor_last = Some(id);
        }
        if self.focus == Focus::Terminal && self.term_editor_focus && !shown {
            self.focus = Focus::Main;
        } else if self.focus == Focus::Main && shown {
            self.focus = Focus::Terminal;
            self.term_editor_focus = true;
        }
    }

    /// "Claude Code: Open" / "Codex: Open" / "Cursor Agent: Open": focuses the agent's running
    /// session (the terminal tab used last first, then any other, in the editor area or the
    /// panel), else starts one in an editor tab in the open folder.
    pub fn open_agent(&mut self, agent: &term::Agent) {
        let running = |t: &TermSession| t.shell.name == agent.name && t.exited.is_none();
        let last = self.term_editor_last.and_then(|id| self.term_index(id)).filter(|&i| running(&self.terms[i]));
        let Some(i) = last.or_else(|| self.terms.iter().position(running)) else {
            self.new_agent_session(agent);
            return;
        };
        if !self.terms[i].editor {
            self.reveal_terminal_panel(true);
        }
        self.activate_terminal(i);
        self.resize_terminal();
        self.invalidate();
    }

    /// Ctrl+Shift+A "Claude Code: New Session", Ctrl+Shift+O "Codex: New Session", Ctrl+Shift+U
    /// "Cursor Agent: New Session": starts the agent in a new editor tab.
    pub fn new_agent_session(&mut self, agent: &term::Agent) {
        match agent.find() {
            Some(shell) => self.new_editor_terminal(Some(shell)),
            None => self.agent_missing(agent),
        }
    }

    /// Tells that `agent` is not installed and how to install it: its WinGet or PowerShell
    /// command (run it in a terminal, or copy it) and its download page.
    fn agent_missing(&mut self, agent: &term::Agent) {
        let cmd = agent.install_command();
        let actions = vec![
            ToastAction { label: "Run in Terminal".into(), act: ToastAct::Run(agent.installer()) },
            ToastAction { label: "Copy Command".into(), act: ToastAct::Copy(cmd.clone()) },
            ToastAction { label: "Copy Link".into(), act: ToastAct::Copy(agent.url.into()) },
        ];
        self.push_toast(Toast {
            kind: ToastKind::Warning,
            message: format!("{} is not installed (no {} found). Install it with {}, then start a new session again:", agent.name, agent.exe_names(), agent.install_tool()),
            expires: std::time::Instant::now(),
            code: Some(cmd),
            link: Some(agent.url.into()),
            actions,
            copied: None,
        });
    }

    pub fn select_shell(&mut self) {
        let current = self.default_shell().name;
        let items = self
            .shells()
            .into_iter()
            .map(|s| {
                let mut it = pi(s.name.clone(), s.cmd.clone(), '\u{E756}', s.name.clone());
                if s.name == current {
                    it.sep = "default".into();
                }
                it
            })
            .collect();
        // like VS Code, this only changes the profile used by new terminals
        self.pick("Select your default terminal profile", items, |app, it| {
            app.st.terminal_shell = it.data.clone();
            app.st.save();
            app.show_terminal(true);
        });
    }

    /// Resizes every terminal's emulator and pseudo console to the area it is shown in.
    pub fn resize_terminal(&mut self) {
        let panel = if self.terminal_visible() { self.grid_size_of(false) } else { None };
        let editor = self.grid_size_of(true);
        for t in &mut self.terms {
            let Some((cols, rows)) = (if t.editor { editor } else { panel }) else { continue };
            if t.screen.cols != cols || t.screen.rows != rows {
                t.screen.resize(cols, rows);
                t.scroll = t.scroll.min(t.screen.scrollback.len());
                if let Some(p) = &t.pty {
                    p.resize(cols as u16, rows as u16);
                }
            }
        }
    }

    /// Drains the output of every terminal (hidden ones keep running in the background).
    pub fn on_term_data(&mut self) {
        self.term_pending.store(false, Ordering::SeqCst);
        let log = std::env::var_os("DEVDECK_TERM_LOG");
        let mut any = false;
        // only output of a terminal on screen repaints: one in the background (a dev server, a
        // Claude Code session in another tab) must not repaint the window at its output rate
        let shown = [self.panel_term().filter(|_| self.terminal_visible()), self.editor_term()];
        let mut more = false;
        for (i, t) in self.terms.iter_mut().enumerate() {
            let (before, trimmed) = (t.screen.scrolled_total, t.screen.trimmed);
            let mut exited = false;
            // a budget per message: a flood of output (cat of a big log) lets the window paint
            // and take input in between
            let mut budget = 1usize << 20;
            while let Ok(ev) = t.rx.try_recv() {
                if shown.contains(&Some(i)) {
                    any = true;
                }
                match ev {
                    TermEvent::Data(d) => {
                        if let Some(path) = &log {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                                let _ = f.write_all(&d);
                            }
                        }
                        t.screen.feed(&d);
                        budget = budget.saturating_sub(d.len());
                    }
                    TermEvent::Exited => exited = true,
                }
                if budget == 0 {
                    more = true;
                    break;
                }
            }
            // a full-screen app has no history to be scrolled into
            if t.screen.in_alt_screen() {
                t.scroll = 0;
            }
            if !t.screen.replies.is_empty() {
                let r = std::mem::take(&mut t.screen.replies);
                if let Some(p) = &t.pty {
                    p.write(&r);
                }
            }
            if exited && t.exited.is_none() {
                let code = t.pty.as_ref().and_then(|p| p.exit_code());
                let msg = match code {
                    Some(c) => format!("The terminal process \"{}\" terminated with exit code: {c}.", t.shell.name),
                    None => format!("The terminal process \"{}\" terminated.", t.shell.name),
                };
                t.screen.feed(format!("\r\n\x1b[7m * {msg} Press any key to restart the terminal. \x1b[0m\r\n").as_bytes());
                t.exited = Some(msg);
                any = true;
            }
            // keep the viewport stable when scrolled up into history
            if t.scroll > 0 {
                let added = (t.screen.scrolled_total - before) as usize;
                t.scroll = (t.scroll + added).min(t.screen.scrollback.len());
            }
            // lines dropped from a full history shift the absolute indexes the selection uses
            let d = (t.screen.trimmed - trimmed) as usize;
            if d > 0 {
                t.sel = t.sel.and_then(|(a, b)| (a.0 >= d && b.0 >= d).then(|| ((a.0 - d, a.1), (b.0 - d, b.1))));
            }
        }
        if more && !self.term_pending.swap(true, Ordering::SeqCst) {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(self.hwnd, WM_TERM, 0, 0);
            }
        }
        if any {
            self.invalidate();
        }
    }

    pub fn term_write(&mut self, data: &[u8]) {
        let Some(i) = self.term_target() else { return };
        if self.terms[i].exited.is_some() {
            self.restart_terminal(i);
            return;
        }
        let t = &mut self.terms[i];
        if let Some(p) = &t.pty {
            p.write(data);
        }
        t.scroll = 0;
        t.sel = None;
        self.invalidate();
    }

    pub fn term_paste(&mut self) {
        let Some(text) = win::get_clipboard(self.hwnd) else { return };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = self.term().map(|t| t.screen.bracketed_paste).unwrap_or(false);
        // ESC can't be in pasted text: an `ESC[201~` in it would end the paste early and run
        // the rest as typed commands
        let data = if bracketed { format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")) } else { text };
        self.term_write(data.as_bytes());
    }

    pub fn term_selection_text(&self) -> Option<String> {
        let t = self.term()?;
        let (a, b) = t.sel?;
        let (s, e) = if a <= b { (a, b) } else { (b, a) };
        if s == e {
            return None;
        }
        let mut out = Vec::new();
        for line in s.0..=e.0 {
            let from = if line == s.0 { s.1 } else { 0 };
            let to = if line == e.0 { e.1 } else { t.screen.cols };
            out.push(t.screen.line_text(line, from, to).trim_end().to_string());
        }
        Some(out.join("\r\n"))
    }

    pub fn term_copy(&mut self) -> bool {
        match self.term_selection_text() {
            Some(text) => {
                win::set_clipboard(self.hwnd, &text);
                if let Some(t) = self.term_mut() {
                    t.sel = None;
                }
                self.invalidate();
                true
            }
            None => false,
        }
    }

    /// Keyboard input while a terminal has focus. Returns true when handled.
    pub fn term_key(&mut self, vk: u16, ctrl: bool, shift: bool, alt: bool) -> bool {
        let in_editor = self.target_in_editor();
        // workbench shortcuts that win over the shell (like VS Code's defaults)
        let cmd = match (vk, ctrl, shift) {
            (0x50, true, true) | (0x70, false, false) => Some(Cmd::Palette),
            (0x50, true, false) if !alt => Some(Cmd::GotoFile),
            (0xC0, true, false) if alt => Some(Cmd::NewTerminal),
            (0xC0, true, false) => Some(Cmd::ToggleTerminal),
            (0xC0, true, true) => Some(Cmd::ToggleEditorTerminal),
            (0x4D, true, true) => Some(Cmd::TogglePanelMax),
            (0x4A, true, false) => Some(Cmd::ToggleOutput),
            (0x47, true, true) => Some(Cmd::FocusCommit),
            (0x42, true, false) if !alt => Some(Cmd::ToggleSidebar),
            (0x45, true, true) => Some(Cmd::ShowExplorer),
            (0x46, true, true) => Some(Cmd::FindInFiles),
            (0x48, true, true) => Some(Cmd::ReplaceInFiles),
            (0x41, true, true) => Some(Cmd::ClaudeNew),
            (0x4F, true, true) => Some(Cmd::CodexNew),
            (0x55, true, true) => Some(Cmd::CursorNew),
            (0x09, true, false) => Some(Cmd::NextEditor),
            (0x09, true, true) => Some(Cmd::PrevEditor),
            // Ctrl+W / Ctrl+F4 close a terminal tab like any other tab
            (0x57 | 0x73, true, false) if in_editor => Some(Cmd::CloseEditor),
            // Ctrl+Shift+W closes all editor tabs, from the panel too
            (0x57, true, true) => Some(Cmd::CloseAllEditors),
            // in an editor tab these switch editors, in the panel they switch terminals
            (0x22, true, false) => Some(if in_editor { Cmd::NextEditor } else { Cmd::FocusNextTerminal }),
            (0x21, true, false) => Some(if in_editor { Cmd::PrevEditor } else { Cmd::FocusPrevTerminal }),
            _ => None,
        };
        // keys handled here must not also reach the shell as the WM_CHAR TranslateMessage queued
        // (Ctrl+B ^B, Ctrl+J LF, Ctrl+Shift+M CR...)
        if let Some(c) = cmd {
            win::drop_char(self.hwnd);
            self.exec(c);
            return true;
        }
        if ctrl && vk == 0x43 && (shift || self.term().map(|t| t.sel.is_some()).unwrap_or(false)) {
            // Ctrl+C copies when there is a selection, Ctrl+Shift+C always copies
            if self.term_copy() || shift {
                win::drop_char(self.hwnd);
                return true;
            }
        }
        if (ctrl && vk == 0x56) || (shift && vk == 0x2D) {
            win::drop_char(self.hwnd);
            self.term_paste();
            return true;
        }
        // xterm keys the character messages cannot tell apart
        let seq: Option<&[u8]> = match vk {
            // Shift+Tab: back-tab (Claude Code / Codex / Cursor Agent mode cycling, PSReadLine completion)
            0x09 if shift && !ctrl => Some(b"\x1b[Z"),
            // Shift+Enter: a new line in the prompt, as Claude Code's /terminal-setup binds it
            // (Codex reads it as Alt+Enter, which inserts a new line there too)
            0x0D if shift && !ctrl && !alt && self.term().is_some_and(|t| term::is_agent(&t.shell.name)) => Some(b"\x1b\r"),
            0x20 if ctrl && !shift && !alt => Some(b"\0"),
            _ => None,
        };
        if let Some(seq) = seq {
            win::drop_char(self.hwnd);
            self.term_write(seq);
            return true;
        }
        if shift && (vk == 0x21 || vk == 0x22) {
            let page = self.term().map(|t| t.screen.rows).unwrap_or(10);
            self.term_scroll_by(if vk == 0x21 { page as i32 } else { -(page as i32) });
            return true;
        }
        let app_cursor = self.term().map(|t| t.screen.app_cursor).unwrap_or(false);
        if let Some(seq) = term::key_sequence(vk, ctrl, shift, alt, app_cursor) {
            self.term_write(&seq);
            return true;
        }
        false
    }

    pub fn term_char(&mut self, ch: u16, alt: bool) {
        let s: String = if (0xD800..0xDC00).contains(&ch) {
            self.term_surrogate = ch;
            return;
        } else if (0xDC00..0xE000).contains(&ch) {
            let hi = std::mem::take(&mut self.term_surrogate);
            String::from_utf16_lossy(&[hi, ch])
        } else if ch == 0x08 {
            // Backspace (WM_CHAR 0x08) sends DEL like xterm; Ctrl+H (also 0x08) stays ^H
            let backspace = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x08) } < 0;
            if backspace { "\x7f" } else { "\x08" }.into()
        } else if ch == 0x7F {
            // Ctrl+Backspace (WM_CHAR 0x7F) sends ^H like xterm.js: deletes a word in shells
            "\x08".into()
        } else {
            String::from_utf16_lossy(&[ch])
        };
        let mut data = Vec::new();
        if alt {
            data.push(0x1b);
        }
        data.extend_from_slice(s.as_bytes());
        self.term_write(&data);
    }

    /// Scrolls the terminal that gets input.
    pub fn term_scroll_by(&mut self, lines: i32) {
        if let Some(i) = self.term_target() {
            self.term_scroll_at(i, lines);
        }
    }

    /// Scrolls terminal `i` through its history (`lines` > 0 goes up).
    pub fn term_scroll_at(&mut self, i: usize, lines: i32) {
        let Some(t) = self.terms.get_mut(i) else { return };
        if t.screen.in_alt_screen() {
            // full-screen apps (less, vim...) get arrow keys instead
            let seq: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
            let n = lines.unsigned_abs().min(10);
            let data: Vec<u8> = seq.repeat(n as usize);
            if let Some(p) = &t.pty {
                p.write(&data);
            }
            return;
        }
        let max = t.screen.scrollback.len() as i32;
        t.scroll = (t.scroll as i32 + lines).clamp(0, max) as usize;
        self.invalidate();
    }

    /// Absolute (line, col) under a client point, in the terminal that gets input.
    pub fn term_pos_at(&self, x: i32, y: i32) -> Option<(usize, usize)> {
        let t = self.term()?;
        let r = self.term_rect();
        let (cw, ch) = self.term_cell();
        let col = ((x - r.left).max(0) / cw).min(t.screen.cols as i32) as usize;
        let row = ((y - r.top).max(0) / ch).min(t.screen.rows as i32 - 1) as usize;
        let top = t.screen.total_lines() - t.screen.rows - t.scroll.min(t.screen.scrollback.len());
        Some((top + row, col))
    }

    pub fn term_select_word(&mut self, x: i32, y: i32) {
        let Some((line, col)) = self.term_pos_at(x, y) else { return };
        let Some(t) = self.term_mut() else { return };
        let Some(l) = t.screen.line(line) else { return };
        let is_word = |c: char| !c.is_whitespace() && !"\"'()[]{}<>|,;`".contains(c);
        if col >= l.len() || !is_word(l[col].ch) {
            return;
        }
        let mut s = col;
        while s > 0 && is_word(l[s - 1].ch) {
            s -= 1;
        }
        let mut e = col;
        while e < l.len() && is_word(l[e].ch) {
            e += 1;
        }
        t.sel = Some(((line, s), (line, e)));
        self.invalidate();
    }

    /// Title for terminal `i`: its name, plus the title the shell set when that says more.
    pub fn term_display_name(&self, i: usize) -> String {
        let Some(t) = self.terms.get(i) else { return "Terminal".into() };
        let title = t.screen.title.trim();
        // shells often set the title to their own executable path; show the profile name then
        let is_path = title.to_ascii_lowercase().ends_with(".exe") || title.contains(":\\");
        if title.is_empty() || is_path || t.custom_name.is_some() {
            t.name().to_string()
        } else {
            format!("{} \u{2022} {}", t.name(), title)
        }
    }

    // ------------------------------------------------------------- painting

    fn term_color(&self, c: Color, fg: bool, flags: u8, bg_default: u32) -> u32 {
        match c {
            Color::Default => {
                if fg {
                    self.t.fg
                } else {
                    bg_default
                }
            }
            Color::Idx(i) => {
                let i = if fg && flags & BOLD != 0 && i < 8 { i + 8 } else { i };
                term::ansi_color(i, self.t.dark)
            }
            Color::Rgb(r, g, b) => crate::theme::hex(((r as u32) << 16) | ((g as u32) << 8) | b as u32),
        }
    }

    /// Paints the panel's terminal and its tab list.
    pub fn paint_terminal(&mut self, g: &Gfx) {
        let area = self.term_area_of(false);
        self.hits.push((area, Hit::TermBody));
        self.paint_term_tabs(g);
        let focused = self.focus == Focus::Terminal && !self.term_editor_focus && unsafe { GetFocus() } == self.hwnd;
        match self.panel_term() {
            Some(i) => self.paint_term_session(g, i, false, focused),
            None => g.text_in(&self.term_rect_of(false), "Starting terminal...", self.fonts.ui, self.t.fg_dim, win::DT_LINE),
        }
    }

    /// Paints terminal `i` in the editor area.
    pub fn paint_editor_terminal(&mut self, g: &Gfx, i: usize) {
        let area = self.term_area_of(true);
        self.hits.push((area, Hit::EditorTerm));
        let focused = self.focus == Focus::Terminal && self.term_editor_focus && unsafe { GetFocus() } == self.hwnd;
        self.paint_term_session(g, i, true, focused);
    }

    fn paint_term_session(&self, g: &Gfx, i: usize, editor: bool, focused: bool) {
        let Some(t) = self.terms.get(i) else { return };
        let area = self.term_area_of(editor);
        let r = self.term_rect_of(editor);
        let bg_default = if editor { self.t.editor } else { self.t.sidebar };
        let (cw, chh) = self.term_cell();
        let sc = &t.screen;
        let scroll = t.scroll.min(sc.scrollback.len());
        let top = sc.total_lines() - sc.rows - scroll;
        let sel = t.sel.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        let saved = g.clip(&area);
        let sel_bg = if self.t.dark { crate::theme::hex(0x264F78) } else { crate::theme::hex(0xADD6FF) };
        let mut run: Vec<char> = Vec::with_capacity(sc.cols);
        for row in 0..sc.rows {
            let li = top + row;
            let Some(line) = sc.line(li) else { continue };
            let y = r.top + row as i32 * chh;
            let mut c = 0usize;
            while c < sc.cols {
                let cell = line.get(c).copied().unwrap_or_default();
                let in_sel = |col: usize| sel.map(|(s, e)| (li, col) >= s && (li, col) < e && s != e).unwrap_or(false);
                let selected = in_sel(c);
                // collect a run of cells with identical attributes
                run.clear();
                let start = c;
                while c < sc.cols {
                    let cc = line.get(c).copied().unwrap_or_default();
                    if cc.fg != cell.fg || cc.bg != cell.bg || cc.flags != cell.flags || in_sel(c) != selected {
                        break;
                    }
                    run.push(cc.ch);
                    c += 1;
                }
                let (mut fg, mut bg) = (self.term_color(cell.fg, true, cell.flags, bg_default), self.term_color(cell.bg, false, cell.flags, bg_default));
                if cell.flags & INVERSE != 0 {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.flags & DIM != 0 {
                    fg = crate::theme::blend_colorref(bg, fg, 0.6);
                }
                if selected {
                    bg = sel_bg;
                }
                let x = r.left + start as i32 * cw;
                let run_rect = rect(x, y, x + run.len() as i32 * cw, y + chh);
                if bg != bg_default {
                    g.fill(&run_rect, bg);
                }
                if run.iter().any(|ch| *ch != ' ') {
                    g.text_cells(x, y, &run, cw, self.fonts.mono, fg, &run_rect);
                }
                if cell.flags & UNDERLINE != 0 {
                    g.hline(run_rect.left, run_rect.right, y + chh - 2, fg);
                }
            }
        }
        // the link under the pointer while Ctrl is held
        if let Some(l) = self.link.filter(|l| l.at == crate::link::At::Term(t.id) && l.line >= top && l.line < top + sc.rows) {
            g.hline(r.left + l.s as i32 * cw, r.left + l.e as i32 * cw, r.top + (l.line - top) as i32 * chh + chh - 2, self.t.fg);
        }
        // cursor
        if sc.cursor_visible && scroll == 0 && t.exited.is_none() {
            let x = r.left + sc.cx as i32 * cw;
            let y = r.top + sc.cy as i32 * chh;
            // on a wide char the cursor covers both its cells
            let wide = sc.lines[sc.cy].get(sc.cx + 1).is_some_and(|c| c.ch == '\0');
            let cr = rect(x, y, x + if wide { 2 * cw } else { cw }, y + chh);
            if focused {
                let cell = sc.lines[sc.cy].get(sc.cx).copied().unwrap_or_default();
                g.fill(&cr, self.t.fg);
                if cell.ch != ' ' {
                    g.text_cells(x, y, &[cell.ch], cw, self.fonts.mono, bg_default, &cr);
                }
            } else {
                g.frame(&cr, self.t.fg);
            }
        }
        g.unclip(saved);
        // scrollbar for history
        let total = sc.total_lines() as i32;
        let rows = sc.rows as i32;
        if total > rows {
            let h = rh(&area);
            let th = ((h as f32 * rows as f32 / total as f32) as i32).max(self.s(20));
            let pos = total - rows - scroll as i32;
            let ty = area.top + ((h - th) as f32 * pos as f32 / (total - rows) as f32) as i32;
            g.fill(&rect(area.right - self.s(10), ty, area.right, ty + th), self.t.scroll_thumb);
        }
    }

    /// Tab list on the right of the panel (only with two or more panel terminals).
    fn paint_term_tabs(&mut self, g: &Gfx) {
        if self.panel_term_count() < 2 {
            return;
        }
        let tr = self.term_tabs_rect();
        g.vline(tr.left, tr.top, tr.bottom, self.t.border);
        let rh22 = self.s(22);
        let focused = self.focus == Focus::Terminal && !self.term_editor_focus && unsafe { GetFocus() } == self.hwnd;
        let saved = g.clip(&tr);
        let names: Vec<(usize, String, bool)> = self.terms.iter().enumerate().filter(|(_, t)| !t.editor).map(|(k, t)| (k, t.name().to_string(), t.exited.is_some())).collect();
        for (n, (i, name, exited)) in names.into_iter().enumerate() {
            let row = rect(tr.left + 1, tr.top + n as i32 * rh22, tr.right, tr.top + (n as i32 + 1) * rh22);
            if row.top >= tr.bottom {
                break;
            }
            let active = i == self.term_active;
            let hovered = matches!(self.hover, Hit::TermTab(x) | Hit::TermTabKill(x) if x == i);
            if active {
                g.fill(&row, if focused { self.t.sel } else { self.t.sel_inactive });
            } else if hovered {
                g.fill(&row, self.t.hover);
            }
            self.hits.push((row, Hit::TermTab(i)));
            let fg = if exited { self.t.fg_dim } else { self.t.fg };
            g.icon(&rect(row.left + self.s(8), row.top, row.left + self.s(26), row.bottom), '\u{E756}', self.fonts.icon_small, fg);
            let kill_w = if hovered || active { self.s(24) } else { 0 };
            g.text_in(&rect(row.left + self.s(30), row.top, row.right - self.s(6) - kill_w, row.bottom), &name, self.fonts.small, fg, win::DT_LINE);
            if kill_w > 0 {
                let kr = rect(row.right - self.s(24), row.top + self.s(1), row.right - self.s(4), row.bottom - self.s(1));
                let hit = Hit::TermTabKill(i);
                if self.hover == hit {
                    g.round_rect(&kr, self.s(5), self.t.toolbar_hover, None);
                }
                g.icon(&kr, '\u{E74D}', self.fonts.icon_small, self.t.fg);
                self.hits.push((kr, hit));
            }
        }
        g.unclip(saved);
    }
}
