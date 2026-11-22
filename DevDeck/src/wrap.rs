//! Word wrap of file views and diffs: a line longer than the view continues on the next
//! visual rows, cut after spaces and punctuation like VS Code's default wrapping.
//!
//! Views keep working in their own rows (lines, diff rows); `WrapMap` gives the first visual
//! row of each, so painting, hit testing and scrolling convert with a lookup or a binary
//! search. The map is built once per text version and width, with a fast path for lines that
//! fit; the rows of a line are only split when painted.

use crate::app::*;
use crate::editor::{col_at_vcol, TAB};

/// A row may end after these chars, or before the ones in BREAK_BEFORE (VS Code's ASCII set).
const BREAK_AFTER: &str = " \t})]?|/&.,;";
const BREAK_BEFORE: &str = "([{+";
/// Narrowest wrap width, in columns.
const MIN_COLS: usize = 8;

/// One visual row of a line: chars `s..e`, visual columns `vs..ve` (tabs expanded from the
/// line start).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Seg {
    pub s: usize,
    pub e: usize,
    pub vs: usize,
    pub ve: usize,
}

/// The line fits on one row (no tab, no more bytes than columns).
fn fits(line: &str, cols: usize) -> bool {
    line.len() <= cols && !line.as_bytes().contains(&b'\t')
}

/// The visual rows of `line` wrapped at `cols` columns (one row when `cols` is None).
pub fn segments(line: &str, cols: Option<usize>) -> Vec<Seg> {
    let Some(cols) = cols.map(|c| c.max(MIN_COLS)) else {
        return vec![Seg { s: 0, e: usize::MAX >> 1, vs: 0, ve: usize::MAX >> 1 }];
    };
    if fits(line, cols) {
        let n = line.chars().count();
        return vec![Seg { s: 0, e: n, vs: 0, ve: n }];
    }
    let mut out = Vec::new();
    let (mut start, mut start_v) = (0usize, 0usize);
    // where the next row may start: (char column, visual column)
    let mut cand: Option<(usize, usize)> = None;
    let (mut i, mut v) = (0usize, 0usize);
    for ch in line.chars() {
        let w = if ch == '\t' { TAB - v % TAB } else { 1 };
        if i > start && BREAK_BEFORE.contains(ch) {
            cand = Some((i, v));
        }
        while i > start && v + w - start_v > cols {
            let (bc, bv) = match cand {
                Some(c) if c.0 > start => c,
                // a word longer than the row is cut
                _ => (i, v),
            };
            out.push(Seg { s: start, e: bc, vs: start_v, ve: bv });
            (start, start_v) = (bc, bv);
            cand = None;
        }
        v += w;
        i += 1;
        if BREAK_AFTER.contains(ch) {
            cand = Some((i, v));
        }
    }
    out.push(Seg { s: start, e: i, vs: start_v, ve: v });
    out
}

/// Number of visual rows of `line`.
pub fn row_count(line: &str, cols: usize) -> usize {
    if fits(line, cols.max(MIN_COLS)) {
        1
    } else {
        segments(line, Some(cols)).len()
    }
}

/// The row of `segs` that shows char column `c` (a column where a row starts belongs to it).
pub fn seg_of(segs: &[Seg], c: usize) -> usize {
    segs.partition_point(|g| g.s <= c).saturating_sub(1)
}

/// Char column in row `k` of `line` at visual column `want2` (in half columns) from the
/// row's start; a row other than the last ends before its last char (its end is the next
/// row's start).
pub fn col_in_seg(line: &str, segs: &[Seg], k: usize, want2: usize) -> usize {
    let g = segs[k];
    let c = col_at_vcol(line, g.vs * 2 + want2);
    let last = k + 1 == segs.len();
    c.clamp(g.s, if last || g.e == g.s { g.e } else { g.e - 1 })
}

/// First visual row of each row of the active view, for one text version and width.
pub struct WrapMap {
    key: (u64, u64, usize, usize),
    /// First visual row of row `r`; one more entry holds the total.
    pub starts: Vec<u32>,
}

/// First visual row of row `r`.
pub fn vrow(m: Option<&WrapMap>, r: usize) -> usize {
    match m {
        Some(m) => m.starts[r.min(m.starts.len() - 1)] as usize,
        None => r,
    }
}

/// The row at visual row `v` and the visual row within it.
pub fn row_at(m: Option<&WrapMap>, v: usize) -> (usize, usize) {
    match m {
        Some(m) => {
            let r = m.starts.partition_point(|&s| s as usize <= v).saturating_sub(1).min(m.starts.len().saturating_sub(2));
            (r, v - m.starts[r] as usize)
        }
        None => (v, 0),
    }
}

/// Visual rows of the view with `rows` rows.
pub fn vtotal(m: Option<&WrapMap>, rows: usize) -> usize {
    m.map_or(rows, |m| *m.starts.last().unwrap() as usize)
}

impl App {
    /// Word wrap is on and the active view is a file or a diff.
    pub fn wrapping(&self) -> bool {
        self.st.word_wrap && matches!(self.view, MainView::File(_) | MainView::Diff(_))
    }

    /// Width of the line number column of a diff.
    pub fn diff_num_w(&self, d: &DiffView) -> i32 {
        let digits = d.fd.hunks.iter().map(|h| (h.old_start + h.old_len).max(h.new_start + h.new_len)).max().unwrap_or(1).to_string().len() as i32;
        digits.max(3) * self.fonts.mono_w + self.s(14)
    }

    /// Columns of a visual row of the active view (one side of a side-by-side diff); None
    /// without word wrap.
    pub fn wrap_cols(&self) -> Option<usize> {
        if !self.wrapping() {
            return None;
        }
        let body = self.l.main_body;
        let mw = self.fonts.mono_w.max(1);
        let px = match &self.view {
            MainView::File(f) => rw_(&body) - self.gutter_w(f.lines.len()) - self.s(4) - self.s(14),
            MainView::Diff(d) => {
                let width = rw_(&body) - self.s(12);
                let num_w = self.diff_num_w(d);
                (if self.st.inline_diff { width - num_w * 2 } else { width / 2 - num_w }) - mw - self.s(10)
            }
            _ => return None,
        };
        Some(((px / mw).max(0) as usize).max(MIN_COLS))
    }

    fn wrap_key(&self) -> Option<(u64, u64, usize, usize)> {
        let cols = self.wrap_cols()?;
        Some(match &self.view {
            MainView::File(f) => (f.ed.uid.0, f.ed.version, cols, f.lines.len() + f.conflicts.len()),
            MainView::Diff(d) => (d.uid.0, self.st.inline_diff as u64, cols, d.rows.len()),
            _ => return None,
        })
    }

    /// The visual rows of the active view, when word wrap applies and they are up to date.
    pub fn wrap_map(&self) -> Option<&WrapMap> {
        let key = self.wrap_key()?;
        self.wrap.as_ref().filter(|w| w.key == key)
    }

    /// Builds the visual rows of the active view when its text or width changed.
    pub fn wrap_update(&mut self) {
        let Some(key) = self.wrap_key() else { return };
        if self.wrap.as_ref().is_some_and(|w| w.key == key) {
            return;
        }
        let cols = key.2;
        let mut starts: Vec<u32> = Vec::with_capacity(key.3 + 1);
        let mut v = 0u32;
        let mut push = |n: usize| {
            starts.push(v);
            v = v.saturating_add(n as u32);
        };
        match &self.view {
            MainView::File(f) => {
                // a conflict's action row comes before its first line
                let mut c = 0;
                for (li, l) in f.lines.iter().enumerate() {
                    while c < f.conflicts.len() && f.conflicts[c].start == li {
                        push(1);
                        c += 1;
                    }
                    push(row_count(l, cols));
                }
            }
            MainView::Diff(d) => {
                let inline = self.st.inline_diff;
                let n = |id: Option<crate::diff::LineId>| id.map_or(1, |(hi, li)| row_count(d.fd.hunks[hi].lines[li].text.trim_end_matches('\r'), cols));
                for r in &d.rows {
                    push(if r.sep.is_some() {
                        1
                    } else if inline {
                        n(r.left)
                    } else {
                        n(r.left).max(n(r.right))
                    });
                }
            }
            _ => return,
        }
        starts.push(v);
        self.wrap = Some(WrapMap { key, starts });
    }

    /// View: Toggle Word Wrap (Alt+Z, the context menus): the row at the top stays there.
    pub fn toggle_word_wrap(&mut self) {
        self.wrap_update();
        let lh = self.line_h().max(1);
        let (row, _) = row_at(self.wrap_map(), (self.scroll[Area::Main as usize] / lh).max(0) as usize);
        self.st.word_wrap = !self.st.word_wrap;
        self.st.save();
        self.hscroll = 0;
        self.wrap_update();
        self.scroll[Area::Main as usize] = vrow(self.wrap_map(), row) as i32 * lh;
        self.clamp_scroll(Area::Main);
        if self.editable_view() {
            self.reveal_caret();
        }
        self.invalidate();
    }

    /// Scrolls the main view so visual row `v` is in view.
    pub fn reveal_vrow(&mut self, v: usize) {
        let lh = self.line_h();
        let top = v as i32 * lh;
        let view = crate::win::rh(&self.l.main_body) - self.s(4);
        let s = self.scroll[Area::Main as usize];
        if top < s {
            self.scroll[Area::Main as usize] = top;
        } else if top + lh > s + view {
            self.scroll[Area::Main as usize] = top + lh - view;
        }
        self.clamp_scroll(Area::Main);
    }
}

fn rw_(r: &windows_sys::Win32::Foundation::RECT) -> i32 {
    crate::win::rw(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(line: &str, cols: usize) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        segments(line, Some(cols)).iter().map(|g| chars[g.s..g.e].iter().collect()).collect()
    }

    #[test]
    fn wraps_after_spaces_and_cuts_long_words() {
        assert_eq!(rows("short", 10), ["short"]);
        assert_eq!(rows("the quick brown fox jumps", 10), ["the quick ", "brown fox ", "jumps"]);
        assert_eq!(rows("abcdefghijklmnopqrst", 8), ["abcdefgh", "ijklmnop", "qrst"]);
        assert_eq!(rows("call(a, b) + other(c)", 12), ["call(a, b) ", "+ other(c)"]);
        assert_eq!(rows("foo.bar.baz.qux", 9), ["foo.bar.", "baz.qux"]);
        // tabs count to the next stop from the line start
        let s = segments("\t\tabcdefgh", Some(10));
        assert_eq!(s[0], Seg { s: 0, e: 2, vs: 0, ve: 8 });
        assert_eq!(s.len(), 2);
        assert_eq!(row_count("x".repeat(100).as_str(), 10), 10);
        assert_eq!(row_count("", 10), 1);
        // no column is lost or repeated
        let line = "let value = compute(alpha, beta) + another_long_identifier_name * 3; // done";
        for cols in 8..40 {
            let s = segments(line, Some(cols));
            assert_eq!(s[0].s, 0);
            assert_eq!(s.last().unwrap().e, line.chars().count());
            assert!(s.windows(2).all(|w| w[0].e == w[1].s && w[0].e > w[0].s));
            assert!(s.iter().all(|g| g.ve - g.vs <= cols));
        }
    }

    #[test]
    fn rows_map_and_caret_columns() {
        let m = WrapMap { key: (0, 0, 0, 3), starts: vec![0, 1, 4, 5] };
        assert_eq!(vrow(Some(&m), 2), 4);
        assert_eq!(row_at(Some(&m), 0), (0, 0));
        assert_eq!(row_at(Some(&m), 3), (1, 2));
        assert_eq!(row_at(Some(&m), 4), (2, 0));
        assert_eq!(row_at(Some(&m), 9), (2, 5));
        assert_eq!(vtotal(Some(&m), 3), 5);
        assert_eq!(row_at(None, 7), (7, 0));
        let line = "aaaa bbbb cccc";
        let segs = segments(line, Some(8));
        assert_eq!(segs.len(), 3);
        assert_eq!(seg_of(&segs, 4), 0);
        assert_eq!(seg_of(&segs, 5), 1);
        assert_eq!(seg_of(&segs, 14), 2);
        // past a row's end: before its last char (the next row starts at its end)
        assert_eq!(col_in_seg(line, &segs, 0, 40), 4);
        assert_eq!(col_in_seg(line, &segs, 2, 40), 14);
        assert_eq!(col_in_seg(line, &segs, 1, 2), 6);
    }
}
