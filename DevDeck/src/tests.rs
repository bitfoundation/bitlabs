//! Integration tests against real temporary git repositories.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::app::{build_change_view, DiffKind, MainView};
use crate::diff::{self, LineId, LK};
use crate::git::{self, Group, Kind};

struct TempRepo(PathBuf);

impl TempRepo {
    fn new(name: &str) -> TempRepo {
        let p = std::env::temp_dir().join(format!("devdeck-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        git::exec(&p, &["init", "-q", "-b", "main"]).unwrap();
        git::exec(&p, &["config", "user.name", "Test"]).unwrap();
        git::exec(&p, &["config", "user.email", "t@example.com"]).unwrap();
        git::exec(&p, &["config", "core.autocrlf", "false"]).unwrap();
        // a global signing setup would make the commits fail or ask for a passphrase
        git::exec(&p, &["config", "commit.gpgsign", "false"]).unwrap();
        git::exec(&p, &["config", "tag.gpgsign", "false"]).unwrap();
        TempRepo(p)
    }
    fn write(&self, f: &str, c: &str) {
        std::fs::write(self.0.join(f), c).unwrap();
    }
    fn read(&self, f: &str) -> String {
        std::fs::read_to_string(self.0.join(f)).unwrap()
    }
    fn root(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn diff_of(root: &Path, f: &git::FileChange) -> diff::FileDiff {
    match build_change_view(root, f, "-U3", false) {
        MainView::Diff(d) => d.fd,
        _ => panic!("expected diff view"),
    }
}

/// Selects the change lines whose text matches `pred`.
fn select(fd: &diff::FileDiff, pred: impl Fn(&str) -> bool) -> HashSet<LineId> {
    let mut s = HashSet::new();
    for (hi, h) in fd.hunks.iter().enumerate() {
        for (li, l) in h.lines.iter().enumerate() {
            if matches!(l.kind, LK::Add | LK::Del) && pred(&l.text) {
                s.insert((hi, li));
            }
        }
    }
    s
}

fn index_content(root: &Path, f: &str) -> String {
    git::exec(root, &["show", &format!(":{f}")]).unwrap()
}

#[test]
fn status_and_partial_staging() {
    let r = TempRepo::new("stage");
    let base: String = (1..=20).map(|i| format!("line{i}\n")).collect();
    r.write("f.txt", &base);
    git::exec(r.root(), &["add", "."]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "base"]).unwrap();

    let modified = base.replace("line2\n", "CHANGED2\n").replace("line18\n", "CHANGED18\n");
    r.write("f.txt", &modified);
    r.write("new file.txt", "a\nb\nc\n");

    let s = git::status(r.root()).unwrap();
    assert_eq!(s.branch.as_deref(), Some("main"));
    let f = s.files.iter().find(|f| f.path == "f.txt").unwrap().clone();
    assert_eq!((f.group, f.kind), (Group::Changes, Kind::Modified));
    let u = s.files.iter().find(|f| f.path == "new file.txt").unwrap().clone();
    assert_eq!(u.kind, Kind::Untracked);

    // stage only the first change
    let fd = diff_of(r.root(), &f);
    let sel = select(&fd, |t| t.contains('2') && !t.contains("18"));
    let patch = diff::build_patch(&fd, &sel, false).unwrap();
    git::exec_input(r.root(), &["apply", "--cached", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    let idx = index_content(r.root(), "f.txt");
    assert!(idx.contains("CHANGED2") && !idx.contains("CHANGED18"), "index:\n{idx}");

    // unstage it again from the staged diff (reverse apply)
    let s = git::status(r.root()).unwrap();
    let staged = s.files.iter().find(|f| f.path == "f.txt" && f.group == Group::Staged).unwrap().clone();
    let fd = diff_of(r.root(), &staged);
    let sel = select(&fd, |t| t.contains("CHANGED2") || t == "line2");
    let patch = diff::build_patch(&fd, &sel, true).unwrap();
    git::exec_input(r.root(), &["apply", "--cached", "-R", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    assert_eq!(index_content(r.root(), "f.txt"), base);

    // revert only the second change in the working tree
    let fd = diff_of(r.root(), &f);
    let sel = select(&fd, |t| t.contains("18"));
    let patch = diff::build_patch(&fd, &sel, true).unwrap();
    git::exec_input(r.root(), &["apply", "-R", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    let wt = r.read("f.txt");
    assert!(wt.contains("CHANGED2") && wt.contains("line18\n") && !wt.contains("CHANGED18"), "worktree:\n{wt}");

    // stage two of three lines of an untracked file
    let fd = match build_change_view(r.root(), &u, "-U3", false) {
        MainView::Diff(d) => {
            assert_eq!(d.kind, DiffKind::Untracked);
            d.fd
        }
        _ => panic!(),
    };
    let sel = select(&fd, |t| t == "a" || t == "c");
    let patch = diff::build_patch(&fd, &sel, false).unwrap();
    git::exec_input(r.root(), &["apply", "--cached", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    assert_eq!(index_content(r.root(), "new file.txt"), "a\nc\n");
}

#[test]
fn log_refs_stash_and_conflicts() {
    let r = TempRepo::new("log");
    r.write("a.txt", "one\n");
    git::exec(r.root(), &["add", "."]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "first"]).unwrap();
    git::exec(r.root(), &["checkout", "-q", "-b", "side"]).unwrap();
    r.write("a.txt", "side\n");
    git::exec(r.root(), &["commit", "-q", "-am", "side change"]).unwrap();
    git::exec(r.root(), &["checkout", "-q", "main"]).unwrap();
    r.write("a.txt", "main\n");
    git::exec(r.root(), &["commit", "-q", "-am", "main change"]).unwrap();
    git::exec(r.root(), &["tag", "v1"]).unwrap();

    let (commits, more) = git::log(r.root(), &["--branches".into(), "--tags".into(), "HEAD".into()], 0, 50);
    assert_eq!(commits.len(), 3);
    assert!(!more);
    assert!(commits.iter().any(|c| c.refs.iter().any(|d| d.name == "main" && d.current)));
    assert!(commits.iter().any(|c| c.refs.iter().any(|d| d.name == "v1")));
    let rows = crate::graph::compute(&commits, Some("main"), None, None);
    assert_eq!(rows.len(), 3);

    let refs = git::refs(r.root());
    assert!(refs.iter().any(|x| x.name == "side"));

    // conflict
    let o = git::run(r.root(), &["merge", "side"], None, true);
    assert!(!o.ok());
    let e = git::error_from(&o);
    assert!(e.message.contains("merge conflicts"), "{}", e.message);
    let s = git::status(r.root()).unwrap();
    assert!(s.merging);
    let c = s.files.iter().find(|f| f.group == Group::Merge).unwrap().clone();
    match build_change_view(r.root(), &c, "-U3", false) {
        MainView::File(f) => assert_eq!(f.conflicts.len(), 1),
        _ => panic!("expected conflict view"),
    }
    git::exec(r.root(), &["merge", "--abort"]).unwrap();

    // stash
    r.write("a.txt", "dirty\n");
    git::exec(r.root(), &["stash", "push", "-m", "wip"]).unwrap();
    let st = git::stashes(r.root());
    assert_eq!(st.len(), 1);
    assert!(st[0].message.contains("wip"));

    // commit files of the merge-base child
    let (files, parent) = git::commit_files(r.root(), &commits[0].hash);
    assert!(parent.is_some());
    assert_eq!(files[0].path, "a.txt");
}

#[test]
fn partial_patches_of_added_deleted_renamed_and_no_newline_files() {
    let r = TempRepo::new("partial");
    r.write("del.txt", "d1\nd2\nd3\n");
    r.write("ren.txt", "r1\nr2\nr3\nr4\nr5\nr6\nr7\nr8\n");
    r.write("nonl.txt", "a\nb");
    git::exec(r.root(), &["add", "."]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "base"]).unwrap();
    let file = |s: &git::Status, p: &str, g: Group| s.files.iter().find(|f| f.path == p && f.group == g).unwrap().clone();

    // stage one line of a deleted file: the file stays, minus that line
    std::fs::remove_file(r.root().join("del.txt")).unwrap();
    let s = git::status(r.root()).unwrap();
    let fd = diff_of(r.root(), &file(&s, "del.txt", Group::Changes));
    let patch = diff::build_patch(&fd, &select(&fd, |t| t == "d2"), false).unwrap();
    git::exec_input(r.root(), &["apply", "--cached", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    assert_eq!(index_content(r.root(), "del.txt"), "d1\nd3\n");

    // unstage one line of a staged new file: the file stays staged, minus that line
    r.write("new.txt", "n1\nn2\nn3\n");
    git::exec(r.root(), &["add", "new.txt"]).unwrap();
    let s = git::status(r.root()).unwrap();
    let fd = diff_of(r.root(), &file(&s, "new.txt", Group::Staged));
    let patch = diff::build_patch(&fd, &select(&fd, |t| t == "n2"), true).unwrap();
    git::exec_input(r.root(), &["apply", "--cached", "-R", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    assert_eq!(index_content(r.root(), "new.txt"), "n1\nn3\n");

    // unstage one line of a staged rename: the rename is kept
    git::exec(r.root(), &["mv", "ren.txt", "moved.txt"]).unwrap();
    r.write("moved.txt", "r1\nr2\nr3\nr4\nR5\nr6\nr7\nr8\nr9\n");
    git::exec(r.root(), &["add", "moved.txt"]).unwrap();
    let s = git::status(r.root()).unwrap();
    let fd = diff_of(r.root(), &file(&s, "moved.txt", Group::Staged));
    let patch = diff::build_patch(&fd, &select(&fd, |t| t == "r9"), true).unwrap();
    git::exec_input(r.root(), &["apply", "--cached", "-R", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    assert_eq!(index_content(r.root(), "moved.txt"), "r1\nr2\nr3\nr4\nR5\nr6\nr7\nr8\n");
    assert!(git::run(r.root(), &["show", ":ren.txt"], None, true).stdout.is_empty());

    // stage only a line added after a last line that had no newline
    r.write("nonl.txt", "a\nB\nc\n");
    let s = git::status(r.root()).unwrap();
    let fd = diff_of(r.root(), &file(&s, "nonl.txt", Group::Changes));
    let patch = diff::build_patch(&fd, &select(&fd, |t| t == "c"), false).unwrap();
    git::exec_input(r.root(), &["apply", "--cached", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    assert_eq!(index_content(r.root(), "nonl.txt"), "a\nb\nc\n");

    // unstage only the removed last line (it had no newline) while the added line stays:
    // the restored line keeps a newline instead of being glued to the next one
    r.write("nonl2.txt", "a\nb");
    git::exec(r.root(), &["add", "nonl2.txt"]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "nonl2"]).unwrap();
    r.write("nonl2.txt", "a\nB\n");
    git::exec(r.root(), &["add", "nonl2.txt"]).unwrap();
    let s = git::status(r.root()).unwrap();
    let fd = diff_of(r.root(), &file(&s, "nonl2.txt", Group::Staged));
    let patch = diff::build_patch(&fd, &select(&fd, |t| t == "b"), true).unwrap();
    git::exec_input(r.root(), &["apply", "--cached", "-R", "--recount", "-"], Some(patch.as_bytes())).unwrap();
    assert_eq!(index_content(r.root(), "nonl2.txt"), "a\nb\nB\n");
}

#[test]
fn accept_side_on_modify_delete_conflict() {
    let r = TempRepo::new("side");
    r.write("a.txt", "one\n");
    git::exec(r.root(), &["add", "."]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "base"]).unwrap();
    git::exec(r.root(), &["checkout", "-q", "-b", "side"]).unwrap();
    git::exec(r.root(), &["rm", "-q", "a.txt"]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "delete"]).unwrap();
    git::exec(r.root(), &["checkout", "-q", "main"]).unwrap();
    r.write("a.txt", "main\n");
    git::exec(r.root(), &["commit", "-q", "-am", "modify"]).unwrap();

    let conflicts = |r: &TempRepo| -> Vec<(String, String)> {
        assert!(!git::run(r.root(), &["merge", "side"], None, true).ok());
        let s = git::status(r.root()).unwrap();
        s.files.iter().filter(|f| f.group == Group::Merge).map(|f| (f.path.clone(), f.xy.clone())).collect()
    };
    let c = conflicts(&r);
    assert_eq!(c, vec![("a.txt".to_string(), "UD".to_string())]);
    // ours modified it: keep the file
    crate::app::resolve_side(r.root(), &c, "--ours").unwrap();
    assert_eq!(git::status(r.root()).unwrap().count(Group::Merge), 0);
    assert_eq!(r.read("a.txt"), "main\n");
    git::exec(r.root(), &["merge", "--abort"]).unwrap();

    // theirs deleted it: the resolution is the deletion
    let c = conflicts(&r);
    crate::app::resolve_side(r.root(), &c, "--theirs").unwrap();
    let s = git::status(r.root()).unwrap();
    assert_eq!(s.count(Group::Merge), 0);
    assert!(!r.root().join("a.txt").exists());
    assert!(s.files.iter().any(|f| f.path == "a.txt" && f.group == Group::Staged && f.kind == Kind::Deleted));
}

#[test]
fn bracketed_paths_are_literal() {
    let r = TempRepo::new("literal");
    for d in ["app/[id]", "app/i"] {
        std::fs::create_dir_all(r.root().join(d)).unwrap();
        r.write(&format!("{d}/page.tsx"), "base\n");
    }
    git::exec(r.root(), &["add", "."]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "base"]).unwrap();
    r.write("app/[id]/page.tsx", "x\n");
    r.write("app/i/page.tsx", "y\n");
    // as a glob, [id] would also match app/i/page.tsx and discard it too
    crate::app::discard_paths(r.root(), &["app/[id]/page.tsx".to_string()], &[]).unwrap();
    assert_eq!(r.read("app/[id]/page.tsx"), "base\n");
    assert_eq!(r.read("app/i/page.tsx"), "y\n");
}

#[test]
fn stash_restore_keeps_the_real_error() {
    let r = TempRepo::new("stashpop");
    let base: String = (1..=10).map(|i| format!("line{i}\n")).collect();
    r.write("a.txt", &base);
    git::exec(r.root(), &["add", "."]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "base"]).unwrap();

    // staged change whose patch no longer fits the index, but merges cleanly: retried without --index
    r.write("a.txt", &base.replace("line6\n", "STASHED6\n"));
    git::exec(r.root(), &["add", "a.txt"]).unwrap();
    git::exec(r.root(), &["stash", "push", "-q"]).unwrap();
    r.write("a.txt", &base.replace("line3\n", "HEAD3\n"));
    git::exec(r.root(), &["commit", "-q", "-am", "head3"]).unwrap();
    let o = git::run(r.root(), &["stash", "apply", "--index"], None, true);
    assert!(!o.ok() && o.stderr.contains("Try without --index"), "{}", o.stderr);
    crate::app::stash_restore(r.root(), "pop", None).unwrap();
    let t = r.read("a.txt");
    assert!(t.contains("HEAD3") && t.contains("STASHED6"), "{t}");
    git::exec(r.root(), &["reset", "-q", "--hard"]).unwrap();

    // unstaged change conflicting with a newer HEAD: the first attempt already merged with conflicts
    r.write("a.txt", "stashed\n");
    git::exec(r.root(), &["stash", "push", "-q"]).unwrap();
    r.write("a.txt", "head\n");
    git::exec(r.root(), &["commit", "-q", "-am", "head"]).unwrap();
    let e = crate::app::stash_restore(r.root(), "pop", None).unwrap_err();
    assert!(e.message.contains("merge conflicts"), "{}", e.message);
    assert!(!e.stderr.contains("needs merge"), "{}", e.stderr);
}

#[test]
fn diff_text_selection_copies_one_side() {
    let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n same\n-old line\n+new line\n tail\n";
    let f = git::FileChange { path: "f".into(), orig: None, kind: Kind::Modified, group: Group::Changes, xy: ".M".into() };
    let MainView::Diff(mut d) = crate::app::diff_view_from(text, &f, DiffKind::WorkingTree, false, "f") else { panic!() };
    let row = |t: &str| d.rows.iter().position(|r| r.left.or(r.right).is_some_and(|(h, l)| d.fd.hunks[h].lines[l].text == t)).unwrap();
    let (same, changed) = (row("same"), row("old line"));
    // from "me" on the first line to "old" on the original side
    d.tsel = Some(crate::app::TextSel { side: 0, anchor: (changed, 3), cur: (same, 2) });
    assert_eq!(d.selected_text().as_deref(), Some("me\r\nold"));
    // the same rows on the modified side
    d.tsel = Some(crate::app::TextSel { side: 1, anchor: (same, 2), cur: (changed, 3) });
    assert_eq!(d.selected_text().as_deref(), Some("me\r\nnew"));
    d.tsel = Some(crate::app::TextSel { side: 1, anchor: (same, 2), cur: (same, 2) });
    assert_eq!(d.selected_text(), None);
}

#[test]
fn fuzzy_scores_word_starts_and_runs() {
    use crate::fuzzy::{pattern, score};
    let s = |q: &str, t: &str| score(&pattern(q), t, None);
    let pos = |q: &str, t: &str| {
        let mut p = Vec::new();
        score(&pattern(q), t, Some(&mut p)).unwrap();
        p
    };
    assert_eq!(s("xyz", "app.rs"), None);
    assert_eq!(s("", "app.rs"), Some(0));
    // case-insensitive, in order
    assert!(s("APP", "app.rs").is_some());
    assert_eq!(s("pa", "app.rs"), None);
    // a prefix beats a match in the middle of a word
    assert!(s("app", "app.rs") > s("app", "wrapper.rs"));
    // consecutive beats scattered
    assert!(s("main", "main.rs") > s("main", "my_awesome_init.rs"));
    // word starts: "ui" in "src/ui.rs" is picked over the "u...i" of "build"
    assert_eq!(pos("ui", "build/ui.rs"), vec![6, 7]);
    // camelCase starts
    assert_eq!(pos("fb", "fooBar.ts"), vec![0, 3]);
    // '\' typed for '/'
    assert_eq!(pos("src\\a", "src/a.rs"), vec![0, 1, 2, 3, 4]);
    // non-ASCII text keeps char positions
    assert_eq!(pos("üb", "Über.txt"), vec![0, 1]);
    assert_eq!(crate::fuzzy::substring_positions(&[pattern("ile")], "Go to File..."), vec![7, 8, 9]);
}

#[test]
fn quick_open_ranks_names_over_paths() {
    use crate::quickopen::{highlights, rank, split_line};
    let words = |q: &str| q.split_whitespace().map(crate::fuzzy::pattern).collect::<Vec<_>>();
    let r = |q: &str, p: &str| rank(&words(q), p.rsplit('/').next().unwrap(), p);
    // the name matches: above any path-only match
    assert!(r("app", "src/app.rs") > r("app", "app/src/main.rs"));
    // the whole name or stem: above a longer name
    assert!(r("app", "src/app.rs") > r("app", "src/apples.rs"));
    assert!(r("app.rs", "x/app.rs") > r("app.rs", "x/app_rs.rs"));
    // every word must match, in the name or the path
    assert!(r("src ui", "src/ui.rs").is_some());
    assert!(r("src ui", "lib/ui.rs").is_none());
    assert!(r("src/ui", "src/ui.rs").is_some());
    // highlights: name chars in the label, folder chars in the description
    assert_eq!(highlights(&words("ui"), "ui.rs", "src", "src/ui.rs"), (vec![0, 1], vec![]));
    assert_eq!(highlights(&words("src/ui"), "ui.rs", "src", "src/ui.rs"), (vec![0, 1], vec![0, 1, 2]));
    // name:line[:column]
    assert_eq!(split_line("app.rs:12"), ("app.rs", Some((12, None))));
    assert_eq!(split_line("app.rs:12:5"), ("app.rs", Some((12, Some(5)))));
    assert_eq!(split_line("app.rs:12,5"), ("app.rs", Some((12, Some(5)))));
    assert_eq!(split_line(":40"), ("", Some((40, None))));
    assert_eq!(split_line("app.rs:"), ("app.rs", None));
    assert_eq!(split_line("C:\\dev\\x"), ("C:\\dev\\x", None));
    assert_eq!(split_line("app"), ("app", None));
}

#[test]
fn quick_open_lists_files() {
    let r = TempRepo::new("quickopen");
    std::fs::create_dir_all(r.root().join("src")).unwrap();
    r.write("src/main.rs", "fn main() {}\n");
    r.write("gone.txt", "x\n");
    r.write(".gitignore", "target/\n");
    git::exec(r.root(), &["add", "."]).unwrap();
    git::exec(r.root(), &["commit", "-q", "-m", "init"]).unwrap();
    std::fs::remove_file(r.root().join("gone.txt")).unwrap();
    r.write("new.txt", "untracked\n");
    std::fs::create_dir_all(r.root().join("target")).unwrap();
    r.write("target/out.bin", "ignored\n");
    // tracked and untracked; not deleted, not ignored; sorted
    assert_eq!(crate::quickopen::list_files(r.root(), true), vec![".gitignore", "new.txt", "src/main.rs"]);
    // a folder without a repository: every file outside hidden folders
    let mut all = crate::quickopen::list_files(r.root(), false);
    all.retain(|p| !p.starts_with("target/"));
    assert_eq!(all, vec![".gitignore", "new.txt", "src/main.rs"]);
}
