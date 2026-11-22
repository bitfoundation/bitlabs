//! Links in text, like VS Code: URLs and file paths (with an optional `:line:col` or
//! `(line,col)` suffix) in the terminal, the editor, diffs and the Output panel. Hovering one
//! with Ctrl held underlines it, and Ctrl+click opens it: a URL in the browser, a file of the
//! open folder in an editor tab (at its line), a folder in the Explorer view, anything else
//! with its default app.

use std::path::{Path, PathBuf};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL;
use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, HTCLIENT, WM_MOUSEMOVE, WM_SETCURSOR};

use crate::app::*;
use crate::win::{self, contains};

/// Where the hovered link is painted, and its line and char range there.
#[derive(Clone, Copy, PartialEq)]
pub struct Link {
    pub at: At,
    pub line: usize,
    pub s: usize,
    pub e: usize,
}

#[derive(Clone, Copy, PartialEq)]
pub enum At {
    /// A terminal, by id.
    Term(u64),
    Editor,
    /// A diff side (0 left / inline, 1 right).
    Diff(u8),
    Output,
}

pub enum Target {
    Url(String),
    Path(PathBuf, Option<(usize, Option<usize>)>),
}

const SCHEMES: [&str; 6] = ["https://", "http://", "ftp://", "file://", "mailto:", "www."];

/// Files opened with File Explorer's selection instead of their default app, which would run
/// them.
const RUNNABLE: &str = " exe com bat cmd ps1 psm1 msi msp vbs vbe js jse wsf wsh scr lnk url reg hta cpl msc jar pif application appref-ms ";

/// The link candidate around char `col` of `line`: its char range and whether it's a URL (a
/// path only counts when it exists, see `target`).
pub fn find(line: &[char], col: usize) -> Option<(usize, usize, bool)> {
    let ok = |c: char| c == '\0' || !(c.is_whitespace() || c.is_control() || "\"'<>`|*?".contains(c));
    if !ok(*line.get(col)?) {
        return None;
    }
    let (mut s, mut e) = (col, col + 1);
    while s > 0 && ok(line[s - 1]) {
        s -= 1;
    }
    while e < line.len() && ok(line[e]) {
        e += 1;
    }
    // a URL starts at its scheme, so "(https://x)" and "url=https://x" link the URL only
    let starts = |i: usize, p: &str| p.chars().enumerate().all(|(k, c)| line.get(i + k).is_some_and(|x| x.to_ascii_lowercase() == c));
    let url = (s..=col).rev().find(|&i| (i == s || !line[i - 1].is_ascii_alphanumeric()) && SCHEMES.iter().any(|p| starts(i, p)));
    match url {
        Some(u) => s = u,
        None => {
            while s < e && "([{".contains(line[s]) {
                s += 1;
            }
        }
    }
    // trailing punctuation and unbalanced closing brackets aren't part of it
    while e > s {
        let c = line[e - 1];
        let open = match c {
            ')' => '(',
            ']' => '[',
            '}' => '{',
            _ => ' ',
        };
        let count = |ch: char| line[s..e].iter().filter(|&&x| x == ch).count();
        if !(".,;:!?".contains(c) || (open != ' ' && count(open) < count(c))) {
            break;
        }
        e -= 1;
    }
    let path_like = || line[s..e].iter().any(|c| matches!(c, '/' | '\\' | '.')) && line[s..e].iter().any(|c| c.is_alphanumeric());
    (col >= s && col < e && (url.is_some() || path_like())).then_some((s, e, url.is_some()))
}

/// Splits a `(line)` / `(line,col)` suffix (MSBuild) or a `:line[:col]` one off a path.
fn split_pos(t: &str) -> (&str, Option<(usize, Option<usize>)>) {
    if let Some((p, inner)) = t.strip_suffix(')').and_then(|x| x.rsplit_once('(')) {
        let mut n = inner.split(',').map(|v| v.trim().parse::<usize>().ok());
        if let (Some(Some(l)), c, None) = (n.next(), n.next(), n.next()) {
            if c.is_none() || c.is_some_and(|c| c.is_some()) {
                return (p, Some((l, c.flatten())));
            }
        }
    }
    crate::quickopen::split_line(t)
}

/// What link text `t` opens: a URL, or an existing file or folder (relative paths are looked
/// up in `bases`, also without the `a/` / `b/` prefix of git's diff output).
pub fn target(t: &str, url: bool, bases: &[PathBuf]) -> Option<Target> {
    let low = t.to_ascii_lowercase();
    if low.starts_with("file://") {
        // file:///C:/x and file://server/share
        let p = t[7..].strip_prefix('/').filter(|p| p.as_bytes().get(1) == Some(&b':')).map(str::to_string).unwrap_or_else(|| format!("//{}", &t[7..]));
        return target(&p.replace("%20", " "), false, bases);
    }
    if url {
        return Some(Target::Url(if low.starts_with("www.") { format!("https://{t}") } else { t.to_string() }));
    }
    let (p, pos) = split_pos(t);
    let p = Path::new(p);
    let mut cands: Vec<PathBuf> = if p.is_absolute() { vec![p.to_path_buf()] } else { bases.iter().map(|b| b.join(p)).collect() };
    if let Ok(q) = p.strip_prefix("a").or_else(|_| p.strip_prefix("b")) {
        cands.extend(bases.iter().map(|b| b.join(q)));
    }
    cands.into_iter().find(|c| c.exists()).map(|c| Target::Path(c, pos))
}

impl App {
    /// The link at client point `x`, `y` (on `hit`), and what it opens.
    pub fn link_at(&self, hit: Hit, x: i32, y: i32) -> Option<(Link, Target)> {
        let folder = self.folder();
        let mw = self.fonts.mono_w;
        let (at, line, chars, col, mut bases): (At, usize, Vec<char>, usize, Vec<PathBuf>) = match hit {
            h @ (Hit::TermBody | Hit::EditorTerm) => {
                let ed = h == Hit::EditorTerm;
                let t = self.terms.get(if ed { self.editor_term()? } else { self.panel_term()? })?;
                let (r, (cw, ch), sc) = (self.term_rect_of(ed), self.term_cell(), &t.screen);
                let row = ((y - r.top) / ch.max(1)) as usize;
                if x < r.left || y < r.top || row >= sc.rows {
                    return None;
                }
                let li = sc.total_lines() - sc.rows - t.scroll.min(sc.scrollback.len()) + row;
                let chars = sc.line(li)?.iter().map(|c| c.ch).collect();
                (At::Term(t.id), li, chars, ((x - r.left) / cw.max(1)) as usize, t.cwd.iter().cloned().collect())
            }
            Hit::EditorText => {
                let MainView::File(f) = &self.view else { return None };
                // half a char to the left: the char under the pointer, not the nearest edge
                let (l, c) = self.editor_pos_at(x - mw / 2, y)?;
                let dir = folder.as_ref().and_then(|r| r.join(&f.path).parent().map(Path::to_path_buf));
                (At::Editor, l, f.lines[l].chars().collect(), c, dir.into_iter().collect())
            }
            Hit::DiffRow(_) => {
                let MainView::Diff(d) = &self.view else { return None };
                let (side, (row, c)) = self.diff_pos_at(x - mw / 2, y, None)?;
                (At::Diff(side), row, d.side_line(row, side)?.text.trim_end_matches('\r').chars().collect(), c, Vec::new())
            }
            _ if self.st.output_visible && !self.st.panel_terminal && contains(&self.l.output_body, x, y) => {
                let b = self.l.output_body;
                let li = ((y - b.top + self.scroll[Area::Output as usize]) / self.fonts.mono_small_h.max(1)) as usize;
                let x0 = b.left + self.s(20);
                if x < x0 {
                    return None;
                }
                (At::Output, li, crate::git::log_lines(li, 1).pop()?.chars().collect(), ((x - x0) / self.fonts.mono_small_w.max(1)) as usize, Vec::new())
            }
            _ => return None,
        };
        bases.extend(folder);
        let (s, e, url) = find(&chars, col)?;
        let text: String = chars[s..e].iter().filter(|&&c| c != '\0').collect();
        Some((Link { at, line, s, e }, target(&text, url, &bases)?))
    }

    /// Mouse move (after `set_hover`), or Ctrl pressed / released: underlines the link under
    /// the pointer while Ctrl is held, and shows the hand pointer over it.
    pub fn hover_link(&mut self, x: i32, y: i32) {
        let l = if crate::key_down(VK_CONTROL) && self.drag.is_none() && self.overlay.is_none() { self.link_at(self.hover, x, y).map(|l| l.0) } else { None };
        if l != self.link {
            self.link = l;
            self.invalidate();
            unsafe {
                SendMessageW(self.hwnd, WM_SETCURSOR, self.hwnd as usize, (HTCLIENT | (WM_MOUSEMOVE << 16)) as isize);
            }
        }
    }

    /// Ctrl pressed or released: the link under the pointer, without a mouse move.
    pub fn ctrl_changed(&mut self) {
        let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p);
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut p);
        }
        self.hover_link(p.x, p.y);
    }

    /// Ctrl+click: opens the link under the pointer, if there is one.
    pub fn follow_link(&mut self, hit: Hit, x: i32, y: i32) -> bool {
        let Some((_, t)) = self.link_at(hit, x, y) else { return false };
        self.link = None;
        match t {
            Target::Url(u) => win::shell_open(&u),
            Target::Path(p, pos) => {
                let canon = |p: &Path| std::fs::canonicalize(p).ok();
                let rel = self.folder().and_then(|r| Some(canon(&p)?.strip_prefix(canon(&r)?).ok()?.to_string_lossy().replace('\\', "/")));
                let ext = p.extension().map(|e| format!(" {} ", e.to_string_lossy().to_ascii_lowercase())).unwrap_or_default();
                match rel {
                    Some(rel) if p.is_file() => self.open_file_at(rel, pos),
                    Some(rel) if !rel.is_empty() => self.reveal_in_explorer(&rel),
                    _ if p.is_file() && RUNNABLE.contains(&ext) => win::reveal(&p.display().to_string()),
                    _ => win::shell_open(&p.display().to_string()),
                }
            }
        }
        self.invalidate();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str, col: usize) -> Option<String> {
        let c: Vec<char> = s.chars().collect();
        find(&c, col).map(|(s, e, _)| c[s..e].iter().collect())
    }

    #[test]
    fn finds_urls_and_paths() {
        assert_eq!(at("see https://x.dev/a_(b). ok", 6).as_deref(), Some("https://x.dev/a_(b)"));
        assert_eq!(at("(https://x.dev/a)", 3).as_deref(), Some("https://x.dev/a"));
        assert_eq!(at("url=https://x.dev", 6).as_deref(), Some("https://x.dev"));
        // left of the scheme it's only a path candidate, which doesn't exist
        assert!(!find(&"url=https://x.dev".chars().collect::<Vec<_>>(), 1).is_some_and(|l| l.2));
        assert_eq!(at("--> src/app.rs:12:5: error", 6).as_deref(), Some("src/app.rs:12:5"));
        assert_eq!(at("at a.cs(3,4): x", 3).as_deref(), Some("a.cs(3,4)"));
        assert_eq!(at("'C:\\x\\y.txt'", 3).as_deref(), Some("C:\\x\\y.txt"));
        assert_eq!(at("plain words", 2), None);
        assert_eq!(at("a ... b", 3), None);
    }

    #[test]
    fn splits_positions() {
        assert_eq!(split_pos("a.rs:12:5"), ("a.rs", Some((12, Some(5)))));
        assert_eq!(split_pos("a.cs(3,4)"), ("a.cs", Some((3, Some(4)))));
        assert_eq!(split_pos("a.cs(3)"), ("a.cs", Some((3, None))));
        assert_eq!(split_pos("C:\\a.rs"), ("C:\\a.rs", None));
        assert_eq!(split_pos("f(x)"), ("f(x)", None));
    }
}
