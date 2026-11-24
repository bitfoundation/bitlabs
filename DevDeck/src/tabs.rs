//! Editor tabs for the main area (diffs, files, commits), with VS Code's preview tab:
//! a single click opens a preview tab (italic title) that the next open replaces; a double
//! click, double-clicking the tab or "Keep Open" keeps it. Pinned tabs stay at the left
//! of the strip, show a pin instead of the close button and survive "Close Others" /
//! "Close All", `Ctrl+W` and a middle click. Tabs are reordered by dragging them.

use crate::app::*;
use crate::ui::Menu;
use crate::win::{self, MenuItem};

pub struct EditorTab {
    /// The tab's view. For the active tab the real view lives in `App::view` and this is
    /// `MainView::Empty`.
    pub view: MainView,
    /// Identity used to reuse a tab that already shows the same thing.
    pub key: String,
    pub preview: bool,
    /// Pinned tabs come first in `App::tabs`.
    pub pinned: bool,
    pub scroll: i32,
    pub hscroll: i32,
    /// `view_gen` of the load that fills this tab (results for other tabs are routed by it).
    pub gen: u64,
}

/// (title, description) shown for a view.
pub fn view_title(v: &MainView) -> (String, String) {
    match v {
        MainView::Diff(d) => (d.title.clone(), d.desc.clone()),
        MainView::File(f) => (f.title.clone(), f.path.clone()),
        MainView::Commit(c) => (format!("{} ({})", c.commit.subject(), &c.commit.hash[..8.min(c.commit.hash.len())]), if c.stash.is_some() { "Stash".into() } else { "Commit".into() }),
        MainView::Message(t, _) => (t.clone(), String::new()),
        MainView::Loading(t) => (t.clone(), "Loading...".into()),
        MainView::Terminal(_) => ("Terminal".into(), "Terminal".into()),
        MainView::Empty => (String::new(), String::new()),
    }
}

impl App {
    pub fn tab_view(&self, i: usize) -> &MainView {
        if i == self.tab_active {
            &self.view
        } else {
            &self.tabs[i].view
        }
    }

    /// (title, description) of tab `i`; terminal tabs show the terminal's live title.
    pub fn tab_title(&self, i: usize) -> (String, String) {
        match self.tab_view(i) {
            MainView::Terminal(id) => match self.term_index(*id) {
                Some(k) => (self.term_display_name(k), "Terminal".into()),
                None => ("Terminal".into(), "Terminal".into()),
            },
            v => view_title(v),
        }
    }

    /// Picks the tab the next view goes into: a tab that already has `key` (activated and
    /// refreshed; returns true, so the caller doesn't load it again), else the preview tab
    /// (replaced), else a new tab after the active one. The caller then fills `self.view`.
    pub fn open_tab(&mut self, key: String, pin: bool) -> bool {
        if let Some(i) = self.tabs.iter().position(|t| t.key == key) {
            self.activate_tab(i);
            if pin {
                self.tabs[i].preview = false;
            }
            self.note_history();
            return true;
        }
        // like VS Code there is one preview tab: a preview open replaces it wherever it is (a
        // kept-open file only replaces it when it is the active tab)
        let reuse = if pin { Some(self.tab_active).filter(|&i| self.tabs.get(i).is_some_and(|t| t.preview)) } else { self.tabs.iter().position(|t| t.preview) };
        if let Some(i) = reuse {
            self.activate_tab_raw(i);
            let t = &mut self.tabs[i];
            t.key = key;
            t.preview = !pin;
        } else {
            // park the current view in its tab (a view without a tab, like the empty page, is dropped)
            let cur = std::mem::replace(&mut self.view, MainView::Empty);
            if let Some(t) = self.tabs.get_mut(self.tab_active) {
                t.view = cur;
                t.scroll = self.scroll[Area::Main as usize];
                t.hscroll = self.hscroll;
            }
            // new tabs go after the active one, but never among the pinned tabs
            let at = if self.tabs.is_empty() { 0 } else { (self.tab_active + 1).max(self.pinned_count()) };
            self.tabs.insert(at, EditorTab { view: MainView::Empty, key, preview: !pin, pinned: false, scroll: 0, hscroll: 0, gen: 0 });
            self.tab_active = at;
        }
        self.scroll[Area::Main as usize] = 0;
        self.hscroll = 0;
        self.note_history();
        false
    }

    /// Like `open_tab` with `pin`, but a preview tab stays open next to the new tab instead
    /// of being replaced (for terminals, which aren't "the next file").
    pub fn open_tab_beside(&mut self, key: String) -> bool {
        let old = self.tab_active;
        let preview = self.tabs.get(old).map(|t| t.preview).unwrap_or(false);
        if preview {
            self.tabs[old].preview = false;
        }
        let found = self.open_tab(key, true);
        if preview {
            if let Some(t) = self.tabs.get_mut(old) {
                t.preview = true;
            }
        }
        found
    }

    /// Records that the load with `gen` fills the active tab.
    pub fn mark_tab_gen(&mut self, gen: u64) {
        if let Some(t) = self.tabs.get_mut(self.tab_active) {
            t.gen = gen;
        }
    }

    /// Switches tabs without refreshing the newly active view.
    fn activate_tab_raw(&mut self, i: usize) {
        if i >= self.tabs.len() || i == self.tab_active {
            return;
        }
        let cur = std::mem::replace(&mut self.view, MainView::Empty);
        if let Some(t) = self.tabs.get_mut(self.tab_active) {
            t.view = cur;
            t.scroll = self.scroll[Area::Main as usize];
            t.hscroll = self.hscroll;
        }
        self.tab_active = i;
        let t = &mut self.tabs[i];
        self.view = std::mem::replace(&mut t.view, MainView::Empty);
        self.scroll[Area::Main as usize] = t.scroll;
        self.hscroll = t.hscroll;
        self.note_history();
    }

    pub fn activate_tab(&mut self, i: usize) {
        if i >= self.tabs.len() || i == self.tab_active {
            return;
        }
        self.activate_tab_raw(i);
        self.refresh_tab_view();
        self.clamp_scroll(Area::Main);
        self.sync_term_focus();
        self.invalidate();
    }

    /// Brings the active view up to date after it was in the background: working tree diffs
    /// are reloaded (keeping the scroll position), other diffs follow the inline setting.
    fn refresh_tab_view(&mut self) {
        let change = match &self.view {
            MainView::Diff(d) if d.kind != DiffKind::Commit && d.back.is_none() => d.change.clone(),
            MainView::File(f) => f.change.clone(),
            _ => None,
        };
        if let Some(c) = change {
            let still = self.status().and_then(|s| s.files.iter().find(|f| f.path == c.path && f.group == c.group).cloned());
            if let Some(f) = still {
                self.load_change_view(f, true);
                return;
            }
        }
        if matches!(&self.view, MainView::Diff(d) if d.inline != self.st.inline_diff) {
            self.rebuild_rows();
        }
        self.reload_text_view_if_changed();
    }

    /// Closes tab `i`, asking first when its file has unsaved changes.
    pub fn close_tab(&mut self, i: usize) {
        if self.tab_dirty(i) {
            let key = self.tabs[i].key.clone();
            self.confirm_unsaved(vec![key.clone()], move |a| {
                if let Some(i) = a.tabs.iter().position(|t| t.key == key) {
                    a.close_tab_now(i);
                }
            });
            return;
        }
        self.close_tab_now(i);
    }

    /// Closes tab `i` (unsaved changes are dropped); closing a terminal tab kills its
    /// terminal (unless it was just moved to the panel).
    pub fn close_tab_now(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let term = match self.tab_view(i) {
            MainView::Terminal(id) => Some(*id),
            _ => None,
        };
        if i == self.tab_active {
            self.tabs.remove(i);
            if self.tabs.is_empty() {
                self.tab_active = 0;
                self.view = MainView::Empty;
                self.scroll[Area::Main as usize] = 0;
                self.hscroll = 0;
            } else {
                // like VS Code: the tab to the right takes over, else the one to the left
                let n = i.min(self.tabs.len() - 1);
                let t = &mut self.tabs[n];
                self.view = std::mem::replace(&mut t.view, MainView::Empty);
                self.scroll[Area::Main as usize] = t.scroll;
                self.hscroll = t.hscroll;
                self.tab_active = n;
                self.refresh_tab_view();
            }
        } else {
            self.tabs.remove(i);
            if i < self.tab_active {
                self.tab_active -= 1;
            }
        }
        if let Some(k) = term.and_then(|id| self.term_index(id)).filter(|&k| self.terms[k].editor) {
            self.remove_term(k);
        }
        self.sync_term_focus();
        self.clamp_scroll(Area::Main);
        self.invalidate();
    }

    /// `Ctrl+W` / `Ctrl+F4`: closes the active tab unless it is pinned (like VS Code, a pinned
    /// tab is only closed from its menu, or after unpinning it).
    pub fn close_active_tab(&mut self) {
        if self.tabs.get(self.tab_active).map(|t| !t.pinned).unwrap_or(false) {
            self.close_tab(self.tab_active);
        }
    }

    /// Closes the tabs with the given keys (asking first when some have unsaved changes).
    fn close_keys(&mut self, keys: Vec<String>) {
        let dirty: Vec<String> = keys.iter().filter(|k| self.tabs.iter().position(|t| &t.key == *k).map(|i| self.tab_dirty(i)).unwrap_or(false)).cloned().collect();
        if !dirty.is_empty() {
            self.confirm_unsaved(dirty, move |a| a.close_keys_now(&keys));
            return;
        }
        self.close_keys_now(&keys);
    }

    fn close_keys_now(&mut self, keys: &[String]) {
        // the active tab goes last, so the views of the tabs in between aren't reloaded
        let active = self.tabs.get(self.tab_active).map(|t| t.key.clone());
        let mut keys = keys.to_vec();
        keys.sort_by_key(|k| Some(k) == active.as_ref());
        for k in &keys {
            if let Some(i) = self.tabs.iter().position(|t| &t.key == k) {
                self.close_tab_now(i);
            }
        }
        self.invalidate();
    }

    /// Keys of the unpinned tabs among `range`, other than `except`.
    fn unpinned_keys(&self, range: impl Iterator<Item = usize>, except: usize) -> Vec<String> {
        range.filter(|&i| i != except && !self.tabs[i].pinned).map(|i| self.tabs[i].key.clone()).collect()
    }

    /// "Close Others": closes every unpinned tab but `keep`.
    pub fn close_other_tabs(&mut self, keep: usize) {
        if keep >= self.tabs.len() {
            return;
        }
        self.activate_tab(keep);
        let keys = self.unpinned_keys(0..self.tabs.len(), keep);
        self.close_keys(keys);
    }

    /// "Close to the Right": closes the unpinned tabs right of `of`.
    pub fn close_tabs_right(&mut self, of: usize) {
        if of >= self.tabs.len() {
            return;
        }
        if self.tab_active > of && !self.tabs[self.tab_active].pinned {
            self.activate_tab(of);
        }
        let keys = self.unpinned_keys(of + 1..self.tabs.len(), usize::MAX);
        self.close_keys(keys);
    }

    /// "Close All": closes every unpinned tab.
    pub fn close_all_tabs(&mut self) {
        let keys = self.unpinned_keys(0..self.tabs.len(), usize::MAX);
        self.close_keys(keys);
    }

    /// Keeps a preview tab open (the next file no longer replaces it).
    pub fn keep_tab(&mut self, i: usize) {
        if let Some(t) = self.tabs.get_mut(i) {
            t.preview = false;
            self.invalidate();
        }
    }

    pub fn pinned_count(&self) -> usize {
        self.tabs.iter().take_while(|t| t.pinned).count()
    }

    /// Pins tab `i` (it moves to the end of the pinned tabs and is kept open) or unpins it
    /// (it moves to the start of the unpinned tabs).
    pub fn set_tab_pinned(&mut self, i: usize, pinned: bool) {
        let Some(t) = self.tabs.get_mut(i) else { return };
        if t.pinned == pinned {
            return;
        }
        t.pinned = pinned;
        t.preview = false;
        // either way the tab lands on the boundary between the pinned and the other tabs
        let to = self.tabs.iter().enumerate().filter(|&(k, t)| k != i && t.pinned).count();
        self.move_tab(i, to);
        self.invalidate();
    }

    /// Moves tab `from` to index `to` (the tabs between shift over), keeping the active tab
    /// active.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        let n = self.tabs.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let t = self.tabs.remove(from);
        self.tabs.insert(to, t);
        let a = self.tab_active;
        self.tab_active = if a == from {
            to
        } else if from < a && a <= to {
            a - 1
        } else if to <= a && a < from {
            a + 1
        } else {
            a
        };
        self.invalidate();
    }

    /// The indices tab `i` can be dragged to (inclusive): its own group, pinned or not.
    pub fn tab_drag_range(&self, i: usize) -> (usize, usize) {
        let pc = self.pinned_count();
        if self.tabs.get(i).map(|t| t.pinned).unwrap_or(false) {
            (0, pc.saturating_sub(1))
        } else {
            (pc, self.tabs.len().saturating_sub(1))
        }
    }

    /// Activates the next (`delta` 1) or previous (-1) tab, wrapping around.
    pub fn cycle_tab(&mut self, delta: i32) {
        let n = self.tabs.len() as i32;
        if n > 1 {
            self.activate_tab((self.tab_active as i32 + delta).rem_euclid(n) as usize);
        }
    }

    /// Context menu of tab `i`.
    pub fn tab_menu(&mut self, i: usize, m: &mut Menu) -> Vec<MenuItem> {
        let n = self.tabs.len();
        let preview = self.tabs.get(i).map(|t| t.preview).unwrap_or(false);
        let pinned = self.tabs.get(i).map(|t| t.pinned).unwrap_or(false);
        let others = (0..n).any(|k| k != i && !self.tabs[k].pinned);
        let right = (i + 1..n).any(|k| !self.tabs[k].pinned);
        let any = self.tabs.iter().any(|t| !t.pinned);
        let path = match self.tab_view(i) {
            MainView::Diff(d) => Some(d.path.clone()),
            MainView::File(f) => Some(f.path.clone()),
            _ => None,
        };
        let mut v = vec![
            m.it(if pinned { "Close" } else { "Close\tCtrl+W" }, move |a| a.close_tab(i)),
            m.it_if("Close Others", others, move |a| a.close_other_tabs(i)),
            m.it_if("Close to the Right", right, move |a| a.close_tabs_right(i)),
            m.it_if("Close All	Ctrl+Shift+W", any, |a| a.close_all_tabs()),
            MenuItem::Sep,
            m.it_if("Keep Open", preview, move |a| a.keep_tab(i)),
            if pinned { m.it("Unpin", move |a| a.set_tab_pinned(i, false)) } else { m.it("Pin", move |a| a.set_tab_pinned(i, true)) },
        ];
        if let MainView::Terminal(id) = *self.tab_view(i) {
            v.push(MenuItem::Sep);
            v.push(m.it("Move Terminal into Panel", move |a| {
                if let Some(k) = a.term_index(id) {
                    a.move_terminal_to_panel(k);
                }
            }));
            v.push(m.it("Rename Terminal...", move |a| {
                if let Some(k) = a.term_index(id) {
                    a.rename_terminal(k);
                }
            }));
        }
        if let (Some(p), Some(root)) = (path.filter(|p| !p.is_empty()), self.folder()) {
            // a Windows path (the relative path is '/' separated)
            let abs = root.join(&p).display().to_string().replace('/', "\\");
            // a deleted file is not in the Explorer
            let on_disk = self.tab_reveal_path(i).is_some();
            let rel = p.clone();
            v.push(MenuItem::Sep);
            v.push(m.it_if("Reveal in Explorer\tAlt+Shift+R", on_disk, move |a| a.reveal_in_explorer(&rel)));
            v.push(m.it("Copy Path", move |a| win::set_clipboard(a.hwnd, &abs)));
            v.push(m.it("Copy Relative Path", move |a| win::set_clipboard(a.hwnd, &p)));
        }
        v
    }
}
