//! Branches view in the side bar: local branches, remote branches grouped by remote, and tags,
//! with a context menu of branch actions.

use std::collections::HashSet;

use crate::app::*;
use crate::git::{self, GitError, RefInfo, RefKind};
use crate::ui::Menu;
use crate::win::{self, MenuItem};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BRow {
    /// Collapsible group. `key` is "local", "remote:<name>" or "tags".
    Group { key: String, label: String, count: usize },
    /// A ref (index into `Repo::refs`).
    Ref(usize),
}

/// Splits a remote ref name ("origin/feature/x") into (remote, branch), preferring the longest
/// configured remote name that matches.
pub fn split_remote(remotes: &[String], name: &str) -> (String, String) {
    let best = remotes.iter().filter(|r| name.len() > r.len() && name.starts_with(r.as_str()) && name.as_bytes()[r.len()] == b'/').max_by_key(|r| r.len());
    match best {
        Some(r) => (r.clone(), name[r.len() + 1..].to_string()),
        None => match name.split_once('/') {
            Some((a, b)) => (a.to_string(), b.to_string()),
            None => (String::new(), name.to_string()),
        },
    }
}

fn push_group(rows: &mut Vec<BRow>, collapsed: &HashSet<String>, key: String, label: String, idx: Vec<usize>) {
    let open = !collapsed.contains(&key);
    rows.push(BRow::Group { key, label, count: idx.len() });
    if open {
        rows.extend(idx.into_iter().map(BRow::Ref));
    }
}

/// Rows of the Branches view: Local, one group per remote, then Tags (only when there are any).
/// Branches are sorted by name; tags keep their order (newest first).
pub fn build_rows(refs: &[RefInfo], remotes: &[String], collapsed: &HashSet<String>) -> Vec<BRow> {
    let by_name = |v: &mut Vec<usize>| v.sort_by(|&a, &b| refs[a].name.to_lowercase().cmp(&refs[b].name.to_lowercase()));
    let mut rows = Vec::new();
    let mut local: Vec<usize> = (0..refs.len()).filter(|&i| refs[i].kind == RefKind::Head).collect();
    by_name(&mut local);
    push_group(&mut rows, collapsed, "local".into(), "Local".into(), local);
    let mut by_remote: Vec<(String, Vec<usize>)> = remotes.iter().map(|r| (r.clone(), Vec::new())).collect();
    for (i, rf) in refs.iter().enumerate().filter(|(_, r)| r.kind == RefKind::Remote) {
        let (remote, _) = split_remote(remotes, &rf.name);
        match by_remote.iter_mut().find(|x| x.0 == remote) {
            Some(x) => x.1.push(i),
            None => by_remote.push((remote, vec![i])),
        }
    }
    for (remote, mut idx) in by_remote {
        by_name(&mut idx);
        push_group(&mut rows, collapsed, format!("remote:{remote}"), remote, idx);
    }
    let tags: Vec<usize> = (0..refs.len()).filter(|&i| refs[i].kind == RefKind::Tag).collect();
    if !tags.is_empty() {
        push_group(&mut rows, collapsed, "tags".into(), "Tags".into(), tags);
    }
    rows
}

/// Short tracking text for a local branch: "↑1 ↓2", "gone" or "".
pub fn track_label(track: &str) -> String {
    if track == "gone" {
        return "gone".into();
    }
    let mut out = Vec::new();
    for part in track.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            out.push(format!("{n}\u{2191}"));
        } else if let Some(n) = part.strip_prefix("behind ") {
            out.push(format!("{n}\u{2193}"));
        }
    }
    out.join(" ")
}

/// Local branches whose upstream is gone (deleted on the remote and pruned), sorted by name,
/// except `current` (the checked-out branch can't be deleted).
pub fn gone_branches(refs: &[RefInfo], current: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = refs.iter().filter(|r| r.kind == RefKind::Head && r.track == "gone" && Some(r.name.as_str()) != current).map(|r| r.name.clone()).collect();
    v.sort_by_key(|n| n.to_lowercase());
    v
}

/// Branch names from `git branch -d` errors like "error: the branch 'x' is not fully merged".
pub fn unmerged_from_stderr(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|l| l.contains("not fully merged"))
        .filter_map(|l| {
            let (_, rest) = l.split_once('\'')?;
            let (name, _) = rest.split_once('\'')?;
            Some(name.to_string())
        })
        .collect()
}

/// "a, b, c" (at most `max` names, then "and N more").
fn name_list(names: &[String], max: usize) -> String {
    let mut s = names.iter().take(max).map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ");
    if names.len() > max {
        s += &format!(" and {} more", names.len() - max);
    }
    s
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "es" }
}

fn kind_prefix(k: RefKind) -> &'static str {
    match k {
        RefKind::Head => "H:",
        RefKind::Remote => "R:",
        RefKind::Tag => "T:",
    }
}

impl App {
    pub fn rebuild_branches(&mut self) {
        self.brows = match &self.repo {
            Some(r) => build_rows(&r.refs, &r.status.remotes, &self.collapsed_bgroups),
            None => Vec::new(),
        };
        // the multi-selection keeps only the refs still listed (and not in a collapsed group)
        if !self.branch_multi.is_empty() {
            let shown: HashSet<String> = (0..self.brows.len()).filter(|&i| matches!(self.brows[i], BRow::Ref(_))).filter_map(|i| self.brow_key(i)).collect();
            self.branch_multi.retain(|k| shown.contains(k));
        }
    }

    /// Whether the Branches row `i` is part of the multi-selection.
    pub fn branch_row_selected(&self, i: usize) -> bool {
        !self.branch_multi.is_empty() && self.brow_key(i).is_some_and(|k| self.branch_multi.contains(&k))
    }

    /// The selected refs, in row order.
    pub fn selected_refs(&self) -> Vec<RefInfo> {
        let Some(repo) = &self.repo else { return Vec::new() };
        self.brows
            .iter()
            .filter_map(|b| if let BRow::Ref(ri) = b { repo.refs.get(*ri) } else { None })
            .filter(|r| self.branch_multi.contains(&format!("{}{}", kind_prefix(r.kind), r.name)))
            .cloned()
            .collect()
    }

    /// Click / arrow selection: Ctrl toggles the row, Shift selects the range from the anchor,
    /// neither selects the row alone.
    pub fn select_branch_row_ex(&mut self, i: usize, ctrl: bool, shift: bool) {
        let Some(key) = self.brow_key(i) else { return };
        let is_ref = matches!(self.brows.get(i), Some(BRow::Ref(_)));
        if ctrl && !shift {
            // a group row only takes the focus
            if is_ref {
                if !self.branch_multi.remove(&key) {
                    self.branch_multi.insert(key.clone());
                }
                self.branch_anchor = Some(key.clone());
            }
        } else if shift {
            let a = self.branch_anchor.as_ref().and_then(|k| (0..self.brows.len()).find(|&r| self.brow_key(r).as_ref() == Some(k))).unwrap_or(i);
            let (lo, hi) = (a.min(i), a.max(i));
            self.branch_multi = (lo..=hi).filter(|&r| matches!(self.brows[r], BRow::Ref(_))).filter_map(|r| self.brow_key(r)).collect();
        } else {
            self.branch_multi.clear();
            if is_ref {
                self.branch_multi.insert(key.clone());
            }
            self.branch_anchor = Some(key.clone());
        }
        self.branch_sel = Some(key);
        self.focus = Focus::Branches;
    }

    /// Ctrl+A: selects every listed ref.
    pub fn select_all_branch_rows(&mut self) {
        self.branch_multi = (0..self.brows.len()).filter(|&r| matches!(self.brows[r], BRow::Ref(_))).filter_map(|r| self.brow_key(r)).collect();
        self.focus = Focus::Branches;
        self.invalidate();
    }

    /// Stable key of a row, used to keep the selection across refreshes.
    pub fn brow_key(&self, i: usize) -> Option<String> {
        match self.brows.get(i)? {
            BRow::Group { key, .. } => Some(format!("G:{key}")),
            BRow::Ref(ri) => self.repo.as_ref()?.refs.get(*ri).map(|r| format!("{}{}", kind_prefix(r.kind), r.name)),
        }
    }

    pub fn branch_sel_index(&self) -> Option<usize> {
        // compared in place: runs on every paint, over every tag and remote branch
        let k = self.branch_sel.as_ref()?;
        let refs = &self.repo.as_ref()?.refs;
        self.brows.iter().position(|b| match b {
            BRow::Group { key, .. } => k.strip_prefix("G:") == Some(key.as_str()),
            BRow::Ref(ri) => refs.get(*ri).is_some_and(|r| k.strip_prefix(kind_prefix(r.kind)) == Some(r.name.as_str())),
        })
    }

    pub fn select_branch_row(&mut self, i: usize) {
        self.select_branch_row_ex(i, false, false);
    }

    pub fn branch_ref(&self, i: usize) -> Option<RefInfo> {
        match self.brows.get(i)? {
            BRow::Ref(ri) => self.repo.as_ref()?.refs.get(*ri).cloned(),
            _ => None,
        }
    }

    fn current_branch(&self) -> Option<String> {
        self.status().and_then(|s| s.branch.clone())
    }

    pub fn remotes(&self) -> Vec<String> {
        self.status().map(|s| s.remotes.clone()).unwrap_or_default()
    }

    /// Collapses / expands the group at row `i` (`expand`: None toggles).
    pub fn toggle_branch_group(&mut self, i: usize, expand: Option<bool>) {
        let Some(BRow::Group { key, .. }) = self.brows.get(i).cloned() else { return };
        let collapsed = self.collapsed_bgroups.contains(&key);
        let collapse = expand.map(|e| !e).unwrap_or(!collapsed);
        self.select_branch_row(i);
        if collapse == collapsed {
            return;
        }
        if collapse {
            self.collapsed_bgroups.insert(key);
        } else {
            self.collapsed_bgroups.remove(&key);
        }
        self.rebuild_branches();
        self.clamp_scroll(Area::Branches);
        self.invalidate();
    }

    pub fn set_all_branch_groups(&mut self, expand: bool) {
        let keys: Vec<String> = self.brows.iter().filter_map(|r| if let BRow::Group { key, .. } = r { Some(key.clone()) } else { None }).collect();
        if expand {
            self.collapsed_bgroups.clear();
        } else {
            self.collapsed_bgroups.extend(keys);
        }
        self.rebuild_branches();
        self.clamp_scroll(Area::Branches);
        self.invalidate();
    }

    pub fn toggle_branches_section(&mut self) {
        self.st.branches_open = !self.st.branches_open;
        if !self.st.branches_open && self.focus == Focus::Branches {
            self.focus = Focus::Changes;
        }
        self.st.save();
        self.layout();
    }

    pub fn toggle_graph_section(&mut self) {
        self.st.graph_open = !self.st.graph_open;
        if !self.st.graph_open && self.focus == Focus::Graph {
            self.focus = Focus::Changes;
        }
        self.st.save();
        self.layout();
    }

    /// Enter / double click: toggle a group, check out a ref.
    pub fn branch_activate(&mut self, i: usize) {
        if matches!(self.brows.get(i), Some(BRow::Group { .. })) {
            self.toggle_branch_group(i, None);
            return;
        }
        let Some(rf) = self.branch_ref(i) else { return };
        if rf.kind == RefKind::Head && Some(&rf.name) == self.current_branch().as_ref() {
            return;
        }
        self.checkout(format!("{}{}", kind_prefix(rf.kind), rf.name));
    }

    // ------------------------------------------------------------ actions

    pub fn rename_branch_named(&mut self, old: String) {
        self.input("Please provide a new branch name", &old.clone(), move |app, name| {
            let name = sanitize_branch(&name);
            if !name.is_empty() && name != old {
                app.git_op("Rename Branch", vec!["branch".into(), "-m".into(), old.clone(), name]);
            }
        });
    }

    /// Pushes a local branch to its upstream, or publishes it when it has none.
    pub fn push_branch(&mut self, rf: RefInfo) {
        if Some(&rf.name) == self.current_branch().as_ref() {
            self.push(false);
            return;
        }
        let remotes = self.remotes();
        if remotes.is_empty() {
            self.confirm("Your repository has no remotes configured to push to.", "", &["Add Remote"], |app, _| app.add_remote());
            return;
        }
        match &rf.upstream {
            Some(up) => {
                let (remote, b) = split_remote(&remotes, up);
                self.git_op("Pushing", vec!["push".into(), remote, format!("{}:{b}", rf.name)]);
            }
            None => self.publish_branch(rf.name),
        }
    }

    pub fn publish_branch(&mut self, name: String) {
        let remotes = self.remotes();
        let go = move |app: &mut App, remote: String| app.git_op("Publishing", vec!["push".into(), "-u".into(), remote, name.clone()]);
        if remotes.len() == 1 {
            go(self, remotes[0].clone());
        } else {
            let items = remotes.iter().map(|r| pi(r.clone(), "", '\u{E753}', r.clone())).collect();
            self.pick("Pick a remote to publish the branch to:", items, move |app, it| go(app, it.data.clone()));
        }
    }

    /// Fast-forwards a local branch that isn't checked out from its upstream (`git fetch remote b:name`).
    pub fn fast_forward_branch(&mut self, rf: RefInfo) {
        let Some(up) = rf.upstream.clone() else { return };
        let (remote, b) = split_remote(&self.remotes(), &up);
        self.git_op("Fetching", vec!["fetch".into(), remote, format!("{b}:{}", rf.name)]);
    }

    pub fn set_upstream(&mut self, name: String) {
        let items = self.ref_items(false, true, false, false);
        if items.is_empty() {
            self.warn("There are no remote branches to track. Fetch or publish first.");
            return;
        }
        self.pick(&format!("Select the upstream branch for '{name}'"), items, move |app, it| {
            app.git_op("Set Upstream", vec!["branch".into(), format!("--set-upstream-to={}", &it.data[2..]), name.clone()]);
        });
    }

    /// Deletes every local branch whose remote branch is gone (`git branch -d`), then offers to
    /// force delete the ones that aren't fully merged (common after squash merges).
    pub fn delete_gone_branches(&mut self) {
        let Some(repo) = &self.repo else { return };
        let cur = self.current_branch();
        let names = gone_branches(&repo.refs, cur.as_deref());
        if names.is_empty() {
            let cur_gone = repo.refs.iter().any(|r| r.kind == RefKind::Head && r.track == "gone" && Some(&r.name) == cur.as_ref());
            self.info(if cur_gone {
                "Only the current branch has a gone remote branch. Check out another branch to delete it."
            } else {
                "There are no local branches whose remote branch is gone. Fetch (Prune) to update the remote branches."
            });
            return;
        }
        let n = names.len();
        self.delete_branches(names, &format!("Are you sure you want to delete {n} local branch{} whose remote branch is gone?", plural(n)));
    }

    /// Asks to delete the local branches `names` (`git branch -d`, offering to force delete the
    /// ones that aren't fully merged) or to force delete them all (`git branch -D`).
    pub fn delete_branches(&mut self, names: Vec<String>, msg: &str) {
        if names.is_empty() {
            return;
        }
        let detail = name_list(&names, 15);
        self.confirm(msg, &detail, &["Delete Branches", "Force Delete Branches"], move |app, choice| {
            if choice == 1 {
                app.git_op("Delete Branches", [vec!["branch".to_string(), "-D".into()], names].concat());
                return;
            }
            let on_err: OnErr = Box::new(|app: &mut App, e: &GitError| {
                let unmerged = unmerged_from_stderr(&e.stderr);
                if unmerged.is_empty() {
                    return false;
                }
                let k = unmerged.len();
                let msg = format!("{k} branch{} {} not fully merged. Force delete {}?", plural(k), if k == 1 { "is" } else { "are" }, if k == 1 { "it" } else { "them" });
                app.confirm(&msg, &format!("{}\nCommits only on these branches may be lost.", name_list(&unmerged, 15)), &["Force Delete"], move |app, _| {
                    app.git_op("Delete Branches", [vec!["branch".to_string(), "-D".into()], unmerged].concat());
                });
                true
            });
            app.op(
                "Delete Branches",
                move |root| {
                    let mut a: Vec<&str> = vec!["branch", "-d"];
                    a.extend(names.iter().map(|s| s.as_str()));
                    git::exec(root, &a)
                },
                None,
                Some(on_err),
            );
        });
    }

    /// Runs several git commands in order as one op, stopping at the first failure.
    pub fn git_ops(&mut self, title: &str, cmds: Vec<Vec<String>>) {
        self.op(
            title,
            move |root| {
                let mut out = String::new();
                for args in &cmds {
                    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                    out += &git::exec(root, &a)?;
                }
                Ok(out)
            },
            None,
            None,
        );
    }

    /// Deletes remote branches ("origin/x"), one `git push <remote> --delete ...` per remote.
    pub fn delete_remote_branches(&mut self, fulls: Vec<String>) {
        let remotes = self.remotes();
        let mut by_remote: Vec<(String, Vec<String>)> = Vec::new();
        for f in &fulls {
            let (r, b) = split_remote(&remotes, f);
            match by_remote.iter_mut().find(|x| x.0 == r) {
                Some(x) => x.1.push(b),
                None => by_remote.push((r, vec![b])),
            }
        }
        let n = fulls.len();
        let msg = format!("Are you sure you want to delete {n} remote branch{}?", plural(n));
        self.confirm(&msg, &format!("{}\nThey will be deleted on the remote.", name_list(&fulls, 15)), &["Delete Remote Branches"], move |app, _| {
            let cmds = by_remote.into_iter().map(|(r, bs)| [vec!["push".to_string(), r, "--delete".into()], bs].concat()).collect();
            app.git_ops("Delete Remote Branches", cmds);
        });
    }

    pub fn delete_tags(&mut self, names: Vec<String>) {
        let n = names.len();
        let msg = format!("Are you sure you want to delete {n} tag{}?", if n == 1 { "" } else { "s" });
        self.confirm(&msg, &name_list(&names, 15), &["Delete Tags"], move |app, _| {
            app.git_op("Delete Tags", [vec!["tag".to_string(), "-d".into()], names].concat());
        });
    }

    pub fn push_tags_named(&mut self, names: Vec<String>) {
        let remotes = self.remotes();
        let go = move |app: &mut App, remote: String| {
            let specs: Vec<String> = names.iter().map(|n| format!("refs/tags/{n}")).collect();
            app.git_op("Pushing", [vec!["push".to_string(), remote], specs].concat());
        };
        match remotes.len() {
            0 => self.warn("This repository has no remotes configured to push to."),
            1 => go(self, remotes[0].clone()),
            _ => {
                let items = remotes.iter().map(|r| pi(r.clone(), "", '\u{E753}', r.clone())).collect();
                self.pick("Pick a remote to push the tags to:", items, move |app, it| go(app, it.data.clone()));
            }
        }
    }

    /// Pushes local branches to their upstreams, one `git push <remote> a:x b:y` per remote.
    pub fn push_branches(&mut self, refs: Vec<RefInfo>) {
        let remotes = self.remotes();
        let mut by_remote: Vec<(String, Vec<String>)> = Vec::new();
        for rf in &refs {
            let Some(up) = &rf.upstream else { continue };
            let (r, b) = split_remote(&remotes, up);
            let spec = format!("{}:{b}", rf.name);
            match by_remote.iter_mut().find(|x| x.0 == r) {
                Some(x) => x.1.push(spec),
                None => by_remote.push((r, vec![spec])),
            }
        }
        let cmds = by_remote.into_iter().map(|(r, specs)| [vec!["push".to_string(), r], specs].concat()).collect();
        self.git_ops("Pushing", cmds);
    }

    /// Delete key: deletes the selected refs (one kind at a time).
    pub fn delete_selected_refs(&mut self) {
        let sel = self.selected_refs();
        let Some(first) = sel.first() else { return };
        if sel.iter().any(|r| r.kind != first.kind) {
            self.info("Select only local branches, remote branches or tags to delete them together.");
            return;
        }
        let kind = first.kind;
        let cur = self.current_branch();
        let names: Vec<String> = sel.iter().filter(|r| !(r.kind == RefKind::Head && Some(&r.name) == cur.as_ref())).map(|r| r.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        match kind {
            RefKind::Head => {
                let d = names.len();
                self.delete_branches(names, &format!("Are you sure you want to delete {d} branch{}?", plural(d)));
            }
            RefKind::Remote => self.delete_remote_branches(names),
            RefKind::Tag => self.delete_tags(names),
        }
    }

    pub fn delete_remote_branch(&mut self, full: String) {
        let (remote, b) = split_remote(&self.remotes(), &full);
        self.confirm(&format!("Are you sure you want to delete the remote branch '{full}'?"), &format!("The branch '{b}' will be deleted on the remote '{remote}'."), &["Delete Remote Branch"], move |app, _| {
            app.git_op("Delete Remote Branch", vec!["push".into(), remote.clone(), "--delete".into(), b.clone()]);
        });
    }

    pub fn delete_tag_named(&mut self, name: String) {
        self.confirm(&format!("Are you sure you want to delete the tag '{name}'?"), "", &["Delete Tag"], move |app, _| {
            app.git_op("Delete Tag", vec!["tag".into(), "-d".into(), name.clone()]);
        });
    }

    pub fn push_tag(&mut self, name: String) {
        let remotes = self.remotes();
        let go = move |app: &mut App, remote: String| app.git_op("Pushing", vec!["push".into(), remote, format!("refs/tags/{name}")]);
        match remotes.len() {
            0 => self.warn("This repository has no remotes configured to push to."),
            1 => go(self, remotes[0].clone()),
            _ => {
                let items = remotes.iter().map(|r| pi(r.clone(), "", '\u{E753}', r.clone())).collect();
                self.pick("Pick a remote to push the tag to:", items, move |app, it| go(app, it.data.clone()));
            }
        }
    }

    pub fn remove_remote_named(&mut self, remote: String) {
        self.confirm(&format!("Are you sure you want to remove the remote '{remote}'?"), "Its remote-tracking branches are removed too.", &["Remove Remote"], move |app, _| {
            app.git_op("Remove Remote", vec!["remote".into(), "remove".into(), remote.clone()]);
        });
    }

    // ------------------------------------------------------------ menus

    /// Context menu of a multi-selection of refs (`sel`, two or more).
    fn multi_branch_menu(&mut self, sel: Vec<RefInfo>, m: &mut Menu) -> Vec<MenuItem> {
        let mut v = Vec::new();
        let cur = self.current_branch();
        let of_kind = |k: RefKind| sel.iter().filter(|r| r.kind == k).cloned().collect::<Vec<RefInfo>>();
        let (heads, remotes, tags) = (of_kind(RefKind::Head), of_kind(RefKind::Remote), of_kind(RefKind::Tag));
        let is_cur = |r: &RefInfo| r.kind == RefKind::Head && Some(&r.name) == cur.as_ref();
        let has_cur = sel.iter().any(is_cur);
        let names: Vec<String> = sel.iter().map(|r| r.name.clone()).collect();
        // merge them all into the current branch (an octopus merge)
        let target = cur.clone().unwrap_or_else(|| "HEAD".into());
        let nn = names.clone();
        v.push(m.it_if(&format!("Merge {} into '{target}'", sel.len()), !has_cur, move |a| a.git_op("Merge", [vec!["merge".to_string()], nn].concat())));
        if !heads.is_empty() {
            let k = heads.len();
            v.push(MenuItem::Sep);
            let pushable = heads.iter().all(|r| r.upstream.is_some() && r.track != "gone");
            let h = heads.clone();
            v.push(m.it_if(&format!("Push {k} Branch{}", plural(k)), pushable, move |a| a.push_branches(h)));
            let tracked: Vec<String> = heads.iter().filter(|r| r.upstream.is_some()).map(|r| r.name.clone()).collect();
            v.push(m.it_if(&format!("Unset Upstream of {k} Branch{}", plural(k)), !tracked.is_empty(), move |a| {
                let cmds = tracked.into_iter().map(|n| vec!["branch".to_string(), "--unset-upstream".into(), n]).collect();
                a.git_ops("Unset Upstream", cmds);
            }));
            v.push(MenuItem::Sep);
            let deletable: Vec<String> = heads.iter().filter(|r| !is_cur(r)).map(|r| r.name.clone()).collect();
            let d = deletable.len();
            let label = if has_cur { format!("Delete {d} Branch{} (Except the Current One)...\tDel", plural(d)) } else { format!("Delete {d} Branch{}...\tDel", plural(d)) };
            v.push(m.it_if(&label, d > 0, move |a| a.delete_branches(deletable, &format!("Are you sure you want to delete {d} branch{}?", plural(d)))));
        }
        if !remotes.is_empty() {
            let k = remotes.len();
            let r: Vec<String> = remotes.iter().map(|r| r.name.clone()).collect();
            v.push(MenuItem::Sep);
            v.push(m.it(&format!("Delete {k} Remote Branch{}...", plural(k)), move |a| a.delete_remote_branches(r)));
        }
        if !tags.is_empty() {
            let k = tags.len();
            let s = if k == 1 { "" } else { "s" };
            let t: Vec<String> = tags.iter().map(|r| r.name.clone()).collect();
            let t2 = t.clone();
            v.push(MenuItem::Sep);
            v.push(m.it(&format!("Push {k} Tag{s}"), move |a| a.push_tags_named(t)));
            v.push(m.it(&format!("Delete {k} Tag{s}..."), move |a| a.delete_tags(t2)));
        }
        v.push(MenuItem::Sep);
        let text = names.join("\n");
        v.push(m.it("Copy Names", move |a| win::set_clipboard(a.hwnd, &text)));
        let shas = sel.iter().map(|r| r.sha.clone()).collect::<Vec<_>>().join("\n");
        v.push(m.it("Copy Commit IDs", move |a| win::set_clipboard(a.hwnd, &shas)));
        v.push(MenuItem::Sep);
        v.push(m.it("Select All\tCtrl+A", |a| a.select_all_branch_rows()));
        v.push(m.it("Collapse All", |a| a.set_all_branch_groups(false)));
        v.push(m.it("Expand All", |a| a.set_all_branch_groups(true)));
        v
    }

    /// Context menu of the Branches row `i`.
    pub fn branch_menu(&mut self, i: usize, m: &mut Menu) -> Vec<MenuItem> {
        if self.branch_multi.len() > 1 && self.branch_row_selected(i) {
            let sel = self.selected_refs();
            if sel.len() > 1 {
                return self.multi_branch_menu(sel, m);
            }
        }
        let mut v = Vec::new();
        let common = |m: &mut Menu, v: &mut Vec<MenuItem>| {
            v.push(MenuItem::Sep);
            v.push(m.it("Collapse All", |a| a.set_all_branch_groups(false)));
            v.push(m.it("Expand All", |a| a.set_all_branch_groups(true)));
        };
        match self.brows.get(i).cloned() {
            Some(BRow::Group { key, .. }) => {
                if key == "local" {
                    v.push(m.cmd("Create Branch...", Cmd::Branch));
                    v.push(m.cmd("Create Branch From...", Cmd::BranchFrom));
                    v.push(MenuItem::Sep);
                    v.push(m.cmd("Delete Branches With Gone Remotes...", Cmd::DeleteGoneBranches));
                } else if key == "tags" {
                    v.push(m.cmd("Create Tag...", Cmd::TagCreate));
                    v.push(m.cmd("Push Tags", Cmd::PushTags));
                } else if let Some(remote) = key.strip_prefix("remote:") {
                    let (r1, r2, r3) = (remote.to_string(), remote.to_string(), remote.to_string());
                    v.push(m.it("Fetch", move |a| a.git_op("Fetching", vec!["fetch".into(), r1])));
                    v.push(m.it("Fetch (Prune)", move |a| a.git_op("Fetching", vec!["fetch".into(), "--prune".into(), r2])));
                    v.push(MenuItem::Sep);
                    v.push(m.cmd("Add Remote...", Cmd::AddRemote));
                    v.push(m.it("Remove Remote", move |a| a.remove_remote_named(r3)));
                }
                common(m, &mut v);
            }
            Some(BRow::Ref(_)) => {
                let Some(rf) = self.branch_ref(i) else { return v };
                let cur = self.current_branch();
                let is_cur = rf.kind == RefKind::Head && Some(&rf.name) == cur.as_ref();
                let name = rf.name.clone();
                let n = || name.clone();
                let spec = format!("{}{}", kind_prefix(rf.kind), rf.name);
                v.push(m.it_if("Checkout\tEnter", !is_cur, move |a| a.checkout(spec)));
                let nn = n();
                v.push(m.it("Checkout (Detached)", move |a| a.checkout(format!("D:{nn}"))));
                v.push(MenuItem::Sep);
                let nn = n();
                v.push(m.it("Create Branch From...", move |a| a.create_branch(Some(nn))));
                let nn = n();
                v.push(m.it("Create Tag...", move |a| a.create_tag(Some(nn))));
                v.push(MenuItem::Sep);
                let target = cur.clone().unwrap_or_else(|| "HEAD".into());
                let nn = n();
                v.push(m.it_if(&format!("Merge into '{target}'"), !is_cur, move |a| a.git_op("Merge", vec!["merge".into(), nn])));
                if rf.kind != RefKind::Tag {
                    let nn = n();
                    v.push(m.it_if(&format!("Rebase '{target}' onto '{}'", rf.name), !is_cur && cur.is_some(), move |a| a.git_op("Rebase", vec!["rebase".into(), nn])));
                }
                match rf.kind {
                    RefKind::Head => {
                        v.push(MenuItem::Sep);
                        let r1 = rf.clone();
                        v.push(m.it(if rf.upstream.is_some() { "Push" } else { "Publish Branch" }, move |a| a.push_branch(r1)));
                        if is_cur {
                            v.push(m.it_if("Pull", rf.upstream.is_some(), |a| a.pull(false)));
                        } else {
                            let r2 = rf.clone();
                            let label = match &rf.upstream {
                                Some(u) => format!("Fast-Forward from '{u}'"),
                                None => "Fast-Forward from Upstream".into(),
                            };
                            v.push(m.it_if(&label, rf.upstream.is_some() && rf.track != "gone", move |a| a.fast_forward_branch(r2)));
                        }
                        let nn = n();
                        v.push(m.it("Set Upstream Branch...", move |a| a.set_upstream(nn)));
                        let nn = n();
                        v.push(m.it_if("Unset Upstream Branch", rf.upstream.is_some(), move |a| a.git_op("Unset Upstream", vec!["branch".into(), "--unset-upstream".into(), nn])));
                        v.push(MenuItem::Sep);
                        let nn = n();
                        v.push(m.it("Rename...\tF2", move |a| a.rename_branch_named(nn)));
                        let nn = n();
                        v.push(m.it_if("Delete", !is_cur, move |a| a.delete_branch(Some(nn))));
                        let nn = n();
                        v.push(m.it_if("Force Delete...", !is_cur, move |a| {
                            a.confirm(&format!("Are you sure you want to force delete the branch '{nn}'?"), "Commits only on this branch may be lost.", &["Force Delete"], move |a, _| {
                                a.git_op("Delete Branch", vec!["branch".into(), "-D".into(), nn.clone()]);
                            })
                        }));
                    }
                    RefKind::Remote => {
                        v.push(MenuItem::Sep);
                        let nn = n();
                        v.push(m.it("Delete Remote Branch...", move |a| a.delete_remote_branch(nn)));
                    }
                    RefKind::Tag => {
                        v.push(MenuItem::Sep);
                        let nn = n();
                        v.push(m.it("Push Tag", move |a| a.push_tag(nn)));
                        let nn = n();
                        v.push(m.it("Delete Tag...", move |a| a.delete_tag_named(nn)));
                    }
                }
                v.push(MenuItem::Sep);
                let nn = n();
                v.push(m.it("Copy Name", move |a| win::set_clipboard(a.hwnd, &nn)));
                let sha = rf.sha.clone();
                v.push(m.it("Copy Commit ID", move |a| win::set_clipboard(a.hwnd, &sha)));
            }
            None => {
                v.push(m.cmd("Create Branch...", Cmd::Branch));
                v.push(m.cmd("Fetch From All Remotes", Cmd::FetchAll));
                v.push(MenuItem::Sep);
                v.push(m.cmd("Delete Branches With Gone Remotes...", Cmd::DeleteGoneBranches));
                common(m, &mut v);
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(kind: RefKind, name: &str) -> RefInfo {
        RefInfo { kind, name: name.into(), sha: "0123456789abcdef".into(), upstream: None, track: String::new(), subject: String::new(), date: 0 }
    }

    #[test]
    fn groups_local_remotes_tags() {
        let refs = vec![r(RefKind::Head, "zeta"), r(RefKind::Remote, "origin/main"), r(RefKind::Head, "Alpha"), r(RefKind::Tag, "v2"), r(RefKind::Remote, "up/stream/x"), r(RefKind::Tag, "v1")];
        let remotes = vec!["origin".to_string(), "up/stream".to_string()];
        let rows = build_rows(&refs, &remotes, &HashSet::new());
        let labels: Vec<String> = rows
            .iter()
            .map(|b| match b {
                BRow::Group { label, count, .. } => format!("[{label} {count}]"),
                BRow::Ref(i) => refs[*i].name.clone(),
            })
            .collect();
        assert_eq!(labels, ["[Local 2]", "Alpha", "zeta", "[origin 1]", "origin/main", "[up/stream 1]", "up/stream/x", "[Tags 2]", "v2", "v1"]);
        let collapsed: HashSet<String> = ["local".to_string(), "tags".to_string()].into();
        let rows = build_rows(&refs, &remotes, &collapsed);
        assert_eq!(rows.len(), 6);
    }

    #[test]
    fn remote_split_and_track() {
        let remotes = vec!["origin".to_string(), "origin/mirror".to_string()];
        assert_eq!(split_remote(&remotes, "origin/mirror/main"), ("origin/mirror".into(), "main".into()));
        assert_eq!(split_remote(&remotes, "origin/feature/x"), ("origin".into(), "feature/x".into()));
        assert_eq!(split_remote(&[], "fork/dev"), ("fork".into(), "dev".into()));
        assert_eq!(track_label("ahead 1, behind 2"), "1\u{2191} 2\u{2193}");
        assert_eq!(track_label("gone"), "gone");
        assert_eq!(track_label(""), "");
    }

    #[test]
    fn gone_and_unmerged() {
        let mut refs = vec![r(RefKind::Head, "main"), r(RefKind::Head, "zed"), r(RefKind::Head, "Alpha"), r(RefKind::Head, "keep"), r(RefKind::Remote, "origin/gone")];
        refs[0].track = "gone".into();
        refs[1].track = "gone".into();
        refs[2].track = "gone".into();
        refs[3].track = "ahead 1".into();
        refs[4].track = "gone".into();
        assert_eq!(gone_branches(&refs, Some("main")), ["Alpha", "zed"]);
        assert_eq!(gone_branches(&refs, None), ["Alpha", "main", "zed"]);
        let err = "Deleted branch a (was 123).
error: the branch 'feat/x' is not fully merged.
hint: If you are sure you want to delete it, run 'git branch -D feat/x'
error: The branch 'y' is not fully merged.";
        assert_eq!(unmerged_from_stderr(err), ["feat/x", "y"]);
        assert_eq!(name_list(&["a".into(), "b".into(), "c".into()], 2), "'a', 'b' and 1 more");
    }
}
