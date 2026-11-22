//! Text editing for files opened from the Explorer: caret, selection, undo / redo,
//! clipboard and saving. A file keeps its encoding (UTF-8 with or without BOM, UTF-16
//! LE / BE) and its line endings (CRLF or LF) when saved.

use std::path::Path;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, GetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::GetMessageTime;

use crate::app::*;
use crate::theme::blend_colorref;
use crate::app::Cmd;
use crate::ui::Menu;
use crate::win::{self, rect, rh, rw, Gfx, MenuItem, DT_LINE};

use windows_sys::Win32::Graphics::Gdi::DT_RIGHT;

/// (line, column) where the column counts chars (not bytes) of the line.
pub type Pos = (usize, usize);

/// Tab stops in the editor, like the viewer's tab expansion.
pub const TAB: usize = 4;
/// Undo steps kept per file.
const MAX_UNDO: usize = 5000;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Enc {
    #[default]
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl Enc {
    pub fn label(self) -> &'static str {
        match self {
            Enc::Utf8 => "UTF-8",
            Enc::Utf8Bom => "UTF-8 with BOM",
            Enc::Utf16Le => "UTF-16 LE",
            Enc::Utf16Be => "UTF-16 BE",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditKind {
    Other,
    Type,
    Back,
    Del,
}

/// One undo step: `old` at `at` was replaced with `new`.
#[derive(Clone, Debug)]
struct Edit {
    at: Pos,
    old: String,
    new: String,
    /// (cursor, anchor) before the edit.
    before: (Pos, Pos),
    kind: EditKind,
}

/// Identifies a file view's text for as long as the view lives (the find widget's matches
/// belong to one); every view, editable or not, gets a new one.
#[derive(Debug)]
pub struct Uid(pub u64);

impl Default for Uid {
    fn default() -> Uid {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Uid(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// Editing state of a text file view. `editable` is false for conflict views, files
/// opened from a change, and files that would not survive a save (invalid UTF-8 / UTF-16).
#[derive(Default)]
pub struct EditState {
    pub editable: bool,
    pub enc: Enc,
    pub crlf: bool,
    /// Indentation unit inserted by Tab: "\t" or spaces.
    pub indent: String,
    pub cur: Pos,
    /// Selection anchor; equal to `cur` when nothing is selected.
    pub anchor: Pos,
    /// Visual column kept while moving up / down.
    pub want_vcol: Option<usize>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Undo depth that matches the file on disk (None: no state matches).
    saved_at: Option<usize>,
    /// The next edit starts a new undo step instead of extending the last one.
    seal: bool,
    /// High surrogate of a character typed as two WM_CHAR messages.
    pub surrogate: u16,
    /// (message time, line) of the last double click: a third click on that line soon after
    /// selects the line.
    dbl_at: Option<(u32, usize)>,
    pub uid: Uid,
    /// Changes with every edit, undo and redo.
    pub version: u64,
}

impl EditState {
    pub fn new(lines: &[String], enc: Enc, crlf: bool, editable: bool) -> EditState {
        EditState { editable, enc, crlf, indent: detect_indent(lines), saved_at: Some(0), seal: true, ..Default::default() }
    }

    pub fn dirty(&self) -> bool {
        self.editable && self.saved_at != Some(self.undo.len())
    }
}

// ------------------------------------------------------------------ text helpers

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Byte offset of char column `col` (the end of the string past the last char).
pub fn bidx(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

/// Visual column of char column `col` (tabs expanded).
pub fn vcol(s: &str, col: usize) -> usize {
    let mut v = 0;
    for ch in s.chars().take(col) {
        v += if ch == '\t' { TAB - v % TAB } else { 1 };
    }
    v
}

/// Char column closest to visual column `target` (rounding to the nearer edge).
pub fn col_at_vcol(s: &str, target2: usize) -> usize {
    // target2 is in half columns so a click on a char's right half lands after it
    let mut v = 0;
    for (i, ch) in s.chars().enumerate() {
        let w = if ch == '\t' { TAB - v % TAB } else { 1 };
        if target2 < v * 2 + w {
            return i;
        }
        v += w;
    }
    char_len(s)
}

/// Line text with tabs expanded, one char per cell, up to `max` cells (or a bit past it
/// after a tab): painting needs the visible part only, not all of a huge one-line file.
pub fn display_chars(s: &str, max: usize) -> Vec<char> {
    let mut out = Vec::with_capacity(s.len().min(max));
    for ch in s.chars() {
        if out.len() >= max {
            break;
        }
        if ch == '\t' {
            let n = TAB - out.len() % TAB;
            out.extend(std::iter::repeat(' ').take(n));
        } else {
            out.push(ch);
        }
    }
    out
}

fn detect_indent(lines: &[String]) -> String {
    let (mut tabs, mut spaces, mut two) = (0, 0, 0);
    for l in lines.iter().take(2000) {
        if l.starts_with('\t') {
            tabs += 1;
        } else if l.starts_with("  ") {
            let n = l.len() - l.trim_start_matches(' ').len();
            if n % 2 == 0 {
                spaces += 1;
                if n % 4 == 2 {
                    two += 1;
                }
            }
        }
    }
    if tabs > spaces {
        "\t".into()
    } else if spaces > 0 && two * 4 > spaces {
        "  ".into()
    } else {
        "    ".into()
    }
}

pub fn class(ch: char) -> u8 {
    if ch.is_alphanumeric() || ch == '_' {
        2
    } else if ch.is_whitespace() {
        0
    } else {
        1
    }
}

/// Position after `text` inserted at `start`.
pub fn end_of(start: Pos, text: &str) -> Pos {
    match text.rsplit_once('\n') {
        None => (start.0, start.1 + char_len(text)),
        Some((_, last)) => (start.0 + text.matches('\n').count(), char_len(last)),
    }
}

/// Number of chars between `a` and `b` (ordered), a line break counting as one.
pub fn range_len(lines: &[String], a: Pos, b: Pos) -> usize {
    if a.0 == b.0 {
        return b.1.saturating_sub(a.1);
    }
    let mut n = char_len(&lines[a.0]).saturating_sub(a.1) + 1 + b.1;
    for l in &lines[a.0 + 1..b.0] {
        n += char_len(l) + 1;
    }
    n
}

/// Text between `a` and `b` (ordered), lines joined with '\n'.
pub fn text_range(lines: &[String], a: Pos, b: Pos) -> String {
    if a.0 == b.0 {
        let l = &lines[a.0];
        return l[bidx(l, a.1)..bidx(l, b.1)].to_string();
    }
    let first = &lines[a.0];
    let mut out = first[bidx(first, a.1)..].to_string();
    for l in &lines[a.0 + 1..b.0] {
        out.push('\n');
        out.push_str(l);
    }
    out.push('\n');
    let last = &lines[b.0];
    out.push_str(&last[..bidx(last, b.1)]);
    out
}

/// Replaces the text between `a` and `b` (ordered) with `text`; returns the end of the
/// inserted text.
pub fn replace(lines: &mut Vec<String>, a: Pos, b: Pos, text: &str) -> Pos {
    let head = lines[a.0][..bidx(&lines[a.0], a.1)].to_string();
    let tail = lines[b.0][bidx(&lines[b.0], b.1)..].to_string();
    let parts: Vec<&str> = text.split('\n').collect();
    let n = parts.len();
    let new_lines: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let mut s = String::with_capacity(p.len() + if k == 0 { head.len() } else { 0 } + if k == n - 1 { tail.len() } else { 0 });
            if k == 0 {
                s.push_str(&head);
            }
            s.push_str(p);
            if k == n - 1 {
                s.push_str(&tail);
            }
            s
        })
        .collect();
    lines.splice(a.0..=b.0, new_lines);
    end_of(a, text)
}

/// Decodes file bytes for the editor: (text, encoding, lossless). Returns None for binary
/// content. `lossless` is false when invalid sequences were replaced, so saving would
/// change bytes the user didn't touch.
pub fn decode(bytes: &[u8]) -> Option<(String, Enc, bool)> {
    let utf16 = |be: bool| {
        let body = &bytes[2..];
        let units: Vec<u16> = body.chunks_exact(2).map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
        let ok = body.len() % 2 == 0 && char::decode_utf16(units.iter().copied()).all(|r| r.is_ok());
        (String::from_utf16_lossy(&units), ok)
    };
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (s, ok) = utf16(false);
        return Some((s, Enc::Utf16Le, ok));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let (s, ok) = utf16(true);
        return Some((s, Enc::Utf16Be, ok));
    }
    let (body, enc) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(b) => (b, Enc::Utf8Bom),
        None => (bytes, Enc::Utf8),
    };
    if crate::git::is_binary(body) {
        return None;
    }
    match std::str::from_utf8(body) {
        Ok(s) => Some((s.to_string(), enc, true)),
        Err(_) => Some((String::from_utf8_lossy(body).into_owned(), enc, false)),
    }
}

/// The bytes written when saving `lines`.
pub fn encode(lines: &[String], enc: Enc, crlf: bool) -> Vec<u8> {
    let text = lines.join(if crlf { "\r\n" } else { "\n" });
    match enc {
        Enc::Utf8 => text.into_bytes(),
        Enc::Utf8Bom => {
            let mut v = vec![0xEF, 0xBB, 0xBF];
            v.extend_from_slice(text.as_bytes());
            v
        }
        Enc::Utf16Le | Enc::Utf16Be => {
            let be = enc == Enc::Utf16Be;
            let mut v = if be { vec![0xFE, 0xFF] } else { vec![0xFF, 0xFE] };
            for u in text.encode_utf16() {
                v.extend_from_slice(&if be { u.to_be_bytes() } else { u.to_le_bytes() });
            }
            v
        }
    }
}

// ------------------------------------------------------------------ FileView editing

impl FileView {
    fn clamp(&self, p: Pos) -> Pos {
        let line = p.0.min(self.lines.len().saturating_sub(1));
        (line, p.1.min(char_len(&self.lines[line])))
    }

    /// The selection, ordered (None when it is empty).
    pub fn sel(&self) -> Option<(Pos, Pos)> {
        let (a, b) = (self.ed.anchor, self.ed.cur);
        if a == b {
            None
        } else {
            Some((a.min(b), a.max(b)))
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        self.sel().map(|(a, b)| text_range(&self.lines, a, b))
    }

    fn apply(&mut self, a: Pos, b: Pos, text: &str, kind: EditKind) {
        let (a, b) = (self.clamp(a.min(b)), self.clamp(a.max(b)));
        let old = text_range(&self.lines, a, b);
        if old.is_empty() && text.is_empty() {
            return;
        }
        let before = (self.ed.cur, self.ed.anchor);
        let end = replace(&mut self.lines, a, b, text);
        let ed = &mut self.ed;
        ed.version += 1;
        if ed.saved_at.is_some_and(|s| s > ed.undo.len()) {
            ed.saved_at = None;
        }
        let merged = match ed.undo.last_mut() {
            Some(last) if !ed.seal && last.kind == kind => match kind {
                EditKind::Type if old.is_empty() && !text.contains('\n') && end_of(last.at, &last.new) == a => {
                    last.new.push_str(text);
                    true
                }
                EditKind::Back if text.is_empty() && last.new.is_empty() && b == last.at && !old.contains('\n') => {
                    last.at = a;
                    last.old.insert_str(0, &old);
                    true
                }
                EditKind::Del if text.is_empty() && last.new.is_empty() && a == last.at && !old.contains('\n') => {
                    last.old.push_str(&old);
                    true
                }
                _ => false,
            },
            _ => false,
        };
        if !merged {
            ed.undo.push(Edit { at: a, old, new: text.to_string(), before, kind });
            if ed.undo.len() > MAX_UNDO {
                ed.undo.remove(0);
                ed.saved_at = ed.saved_at.and_then(|s| s.checked_sub(1));
            }
        }
        ed.redo.clear();
        // a word and the space after it are one undo step, like VS Code
        ed.seal = kind == EditKind::Other || text.contains('\n') || (kind == EditKind::Type && text.ends_with([' ', '\t']));
        ed.cur = end;
        ed.anchor = end;
        ed.want_vcol = None;
    }

    pub fn undo(&mut self) -> bool {
        let Some(e) = self.ed.undo.pop() else { return false };
        let end = end_of(e.at, &e.new);
        replace(&mut self.lines, e.at, end, &e.old);
        self.ed.version += 1;
        self.ed.cur = e.before.0;
        self.ed.anchor = e.before.1;
        self.ed.cur = self.clamp(self.ed.cur);
        self.ed.anchor = self.clamp(self.ed.anchor);
        self.ed.redo.push(e);
        self.ed.seal = true;
        self.ed.want_vcol = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(e) = self.ed.redo.pop() else { return false };
        let end = end_of(e.at, &e.old);
        let to = replace(&mut self.lines, e.at, end, &e.new);
        self.ed.version += 1;
        self.ed.cur = to;
        self.ed.anchor = to;
        self.ed.undo.push(e);
        self.ed.seal = true;
        self.ed.want_vcol = None;
        true
    }

    /// Replaces the text between `a` and `b` with `text` as one undo step (Replace / Replace All).
    pub fn edit_range(&mut self, a: Pos, b: Pos, text: &str) {
        self.apply(a, b, text, EditKind::Other);
    }

    pub fn mark_clean(&mut self) {
        self.ed.saved_at = Some(self.ed.undo.len());
        self.ed.seal = true;
    }

    /// Moves the caret; `extend` keeps the anchor (Shift).
    pub fn move_to(&mut self, p: Pos, extend: bool) {
        let p = self.clamp(p);
        self.ed.cur = p;
        if !extend {
            self.ed.anchor = p;
        }
        self.ed.seal = true;
    }

    fn insert(&mut self, text: &str, kind: EditKind) {
        let (a, b) = self.sel().unwrap_or((self.ed.cur, self.ed.cur));
        let kind = if a != b { EditKind::Other } else { kind };
        self.apply(a, b, text, kind);
    }

    pub fn type_text(&mut self, text: &str) {
        let kind = if text.chars().count() == 1 && text != "\n" { EditKind::Type } else { EditKind::Other };
        self.insert(text, kind);
    }

    pub fn word_left(&self, p: Pos) -> Pos {
        if p.1 == 0 {
            return if p.0 == 0 { p } else { (p.0 - 1, char_len(&self.lines[p.0 - 1])) };
        }
        let chars: Vec<char> = self.lines[p.0].chars().collect();
        let mut i = p.1.min(chars.len());
        while i > 0 && class(chars[i - 1]) == 0 {
            i -= 1;
        }
        if i > 0 {
            let c = class(chars[i - 1]);
            while i > 0 && class(chars[i - 1]) == c {
                i -= 1;
            }
        }
        (p.0, i)
    }

    pub fn word_right(&self, p: Pos) -> Pos {
        let chars: Vec<char> = self.lines[p.0].chars().collect();
        if p.1 >= chars.len() {
            return if p.0 + 1 < self.lines.len() { (p.0 + 1, 0) } else { p };
        }
        let mut i = p.1;
        while i < chars.len() && class(chars[i]) == 0 {
            i += 1;
        }
        if i < chars.len() {
            let c = class(chars[i]);
            while i < chars.len() && class(chars[i]) == c {
                i += 1;
            }
        }
        (p.0, i)
    }

    /// Up / Down (`delta` rows, a page for Page Up / Down): the position `delta` visual rows
    /// away (lines without word wrap, `cols` with it), at the column the caret had when it
    /// started moving up or down. One row past the first / last line goes to its start / end.
    pub fn vmove(&mut self, cols: Option<usize>, delta: isize) -> Pos {
        let cur = self.ed.cur;
        let last = self.lines.len() - 1;
        let mut segs = crate::wrap::segments(&self.lines[cur.0], cols);
        let mut k = crate::wrap::seg_of(&segs, cur.1);
        let lines = &self.lines;
        let want = *self.ed.want_vcol.get_or_insert_with(|| vcol(&lines[cur.0], cur.1) - segs[k].vs);
        let mut line = cur.0;
        for _ in 0..delta.unsigned_abs() {
            if delta < 0 {
                if k > 0 {
                    k -= 1;
                } else if line > 0 {
                    line -= 1;
                    segs = crate::wrap::segments(&self.lines[line], cols);
                    k = segs.len() - 1;
                } else if delta == -1 {
                    return (0, 0);
                } else {
                    break;
                }
            } else if k + 1 < segs.len() {
                k += 1;
            } else if line < last {
                line += 1;
                segs = crate::wrap::segments(&self.lines[line], cols);
                k = 0;
            } else if delta == 1 {
                return (last, char_len(&self.lines[last]));
            } else {
                break;
            }
        }
        (line, crate::wrap::col_in_seg(&self.lines[line], &segs, k, want * 2))
    }

    /// Selects the word (or run of spaces / punctuation) at `p`.
    pub fn select_word(&mut self, p: Pos) {
        let p = self.clamp(p);
        let chars: Vec<char> = self.lines[p.0].chars().collect();
        if chars.is_empty() {
            self.move_to(p, false);
            return;
        }
        let at = if p.1 < chars.len() { p.1 } else { p.1 - 1 };
        let c = class(chars[at]);
        let (mut s, mut e) = (at, at + 1);
        while s > 0 && class(chars[s - 1]) == c {
            s -= 1;
        }
        while e < chars.len() && class(chars[e]) == c {
            e += 1;
        }
        self.ed.anchor = (p.0, s);
        self.ed.cur = (p.0, e);
        self.ed.seal = true;
    }

    /// Selects line `l` with its line break (triple click).
    pub fn select_line(&mut self, l: usize) {
        let l = l.min(self.lines.len() - 1);
        self.ed.anchor = (l, 0);
        self.ed.cur = if l + 1 < self.lines.len() { (l + 1, 0) } else { (l, char_len(&self.lines[l])) };
        self.ed.seal = true;
    }

    pub fn select_all(&mut self) {
        let last = self.lines.len() - 1;
        self.ed.anchor = (0, 0);
        self.ed.cur = (last, char_len(&self.lines[last]));
        self.ed.seal = true;
    }

    /// Backspace (`word`: Ctrl+Backspace).
    pub fn backspace(&mut self, word: bool) {
        if let Some((a, b)) = self.sel() {
            self.apply(a, b, "", EditKind::Other);
            return;
        }
        let cur = self.ed.cur;
        let from = if word {
            self.word_left(cur)
        } else if cur.1 > 0 {
            (cur.0, cur.1 - 1)
        } else if cur.0 > 0 {
            (cur.0 - 1, char_len(&self.lines[cur.0 - 1]))
        } else {
            return;
        };
        self.apply(from, cur, "", if word { EditKind::Other } else { EditKind::Back });
    }

    /// Delete (`word`: Ctrl+Delete).
    pub fn delete(&mut self, word: bool) {
        if let Some((a, b)) = self.sel() {
            self.apply(a, b, "", EditKind::Other);
            return;
        }
        let cur = self.ed.cur;
        let to = if word {
            self.word_right(cur)
        } else if cur.1 < char_len(&self.lines[cur.0]) {
            (cur.0, cur.1 + 1)
        } else if cur.0 + 1 < self.lines.len() {
            (cur.0 + 1, 0)
        } else {
            return;
        };
        self.apply(cur, to, "", if word { EditKind::Other } else { EditKind::Del });
    }

    /// Enter: a new line with the current line's indentation.
    pub fn newline(&mut self) {
        let start = self.sel().map(|s| s.0).unwrap_or(self.ed.cur);
        let line = &self.lines[start.0];
        let indent: String = line.chars().take(start.1).take_while(|c| *c == ' ' || *c == '\t').collect();
        self.insert(&format!("\n{indent}"), EditKind::Other);
    }

    /// Ctrl+Enter / Ctrl+Shift+Enter: a new line below / above the caret's line with its
    /// indentation, wherever the caret is in the line (one undo step).
    pub fn newline_beside(&mut self, above: bool) {
        let l = self.ed.cur.0;
        let indent: String = self.lines[l].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if above {
            self.apply((l, 0), (l, 0), &format!("{indent}\n"), EditKind::Other);
            let p = (l, char_len(&indent));
            self.ed.cur = p;
            self.ed.anchor = p;
        } else {
            let end = (l, char_len(&self.lines[l]));
            self.apply(end, end, &format!("\n{indent}"), EditKind::Other);
        }
    }

    /// Tab / Shift+Tab: indents or outdents the selected lines; Tab without a multi-line
    /// selection inserts one indentation unit.
    pub fn indent(&mut self, outdent: bool) {
        let sel = self.sel();
        // like VS Code, a selection of one whole line indents it too (only a part of a line is
        // replaced by the indentation)
        let multi = matches!(sel, Some((a, b)) if a.0 != b.0 || (a.1 == 0 && b.1 == char_len(&self.lines[b.0])));
        if !outdent && !multi {
            let unit = if self.ed.indent == "\t" {
                "\t".to_string()
            } else {
                // spaces up to the next indentation stop
                let n = self.ed.indent.len();
                let v = vcol(&self.lines[self.ed.cur.0], self.ed.cur.1);
                " ".repeat(n - v % n)
            };
            self.insert(&unit, EditKind::Other);
            return;
        }
        let (a, b) = sel.unwrap_or((self.ed.cur, self.ed.cur));
        // a selection ending at column 0 doesn't include that line
        let last = if b.0 > a.0 && b.1 == 0 { b.0 - 1 } else { b.0 };
        let unit = self.ed.indent.clone();
        let width = if unit == "\t" { TAB } else { unit.len() };
        let mut out: Vec<String> = Vec::with_capacity(last - a.0 + 1);
        let mut shift: Vec<isize> = Vec::with_capacity(last - a.0 + 1);
        for l in &self.lines[a.0..=last] {
            if outdent {
                let n = if l.starts_with('\t') { 1 } else { l.chars().take(width).take_while(|c| *c == ' ').count() };
                out.push(l[n..].to_string());
                shift.push(-(n as isize));
            } else if l.is_empty() {
                out.push(String::new());
                shift.push(0);
            } else {
                out.push(format!("{unit}{l}"));
                shift.push(char_len(&unit) as isize);
            }
        }
        if shift.iter().all(|s| *s == 0) {
            return;
        }
        let end_col = char_len(&self.lines[last]);
        let text = out.join("\n");
        let (cur, anchor) = (self.ed.cur, self.ed.anchor);
        self.apply((a.0, 0), (last, end_col), &text, EditKind::Other);
        // keep the selection on the same text
        let adj = |p: Pos| -> Pos {
            if p.0 < a.0 || p.0 > last || p.1 == 0 {
                return p;
            }
            (p.0, (p.1 as isize + shift[p.0 - a.0]).max(0) as usize)
        };
        self.ed.cur = self.clamp(adj(cur));
        self.ed.anchor = self.clamp(adj(anchor));
    }

    /// Text the clipboard gets for Copy / Cut: the selection, or the whole line.
    pub fn copy_text(&self) -> String {
        match self.selected_text() {
            Some(t) => t.replace('\n', "\r\n"),
            None => format!("{}\r\n", self.lines[self.ed.cur.0]),
        }
    }

    /// Pastes a whole line copied without a selection above the caret's line, like VS Code
    /// (the caret stays on its text).
    pub fn paste_line(&mut self, text: &str) {
        let cur = self.ed.cur;
        self.apply((cur.0, 0), (cur.0, 0), text, EditKind::Other);
        let p = (cur.0 + text.matches('\n').count(), cur.1);
        self.ed.cur = p;
        self.ed.anchor = p;
    }

    pub fn cut(&mut self) {
        if let Some((a, b)) = self.sel() {
            self.apply(a, b, "", EditKind::Other);
            return;
        }
        let l = self.ed.cur.0;
        if self.lines.len() == 1 {
            let end = char_len(&self.lines[0]);
            self.apply((0, 0), (0, end), "", EditKind::Other);
        } else if l + 1 < self.lines.len() {
            self.apply((l, 0), (l + 1, 0), "", EditKind::Other);
        } else {
            let prev = char_len(&self.lines[l - 1]);
            let end = char_len(&self.lines[l]);
            self.apply((l - 1, prev), (l, end), "", EditKind::Other);
        }
    }
}

/// Clipboard sequence number after the last Copy / Cut without a selection (a whole line),
/// so pasting exactly that clipboard goes above the caret's line instead of splitting it.
/// Any later copy, here or in another app, changes the number.
static LINE_CLIP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn clipboard_seq() -> u32 {
    unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

fn set_line_clip(whole_line: bool) {
    LINE_CLIP.store(if whole_line { clipboard_seq() } else { 0 }, std::sync::atomic::Ordering::Relaxed);
}

/// The file was changed on disk since the view loaded it (a deleted file is just saved again).
fn changed_on_disk(root: &Path, f: &FileView) -> bool {
    let disk = crate::explorer::mtime(root, &f.path);
    disk.is_some() && f.mtime.is_some() && disk != f.mtime
}

/// Writes the view's text to disk and marks it saved.
pub fn write_view(root: &Path, f: &mut FileView) -> Result<(), String> {
    let bytes = encode(&f.lines, f.ed.enc, f.ed.crlf);
    let abs = root.join(&f.path);
    // the folder may have been deleted while the file was open with unsaved changes
    if let Some(dir) = abs.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&abs, bytes).map_err(|e| format!("Failed to save '{}': {e}", f.title))?;
    f.mtime = crate::explorer::mtime(root, &f.path);
    f.mark_clean();
    Ok(())
}

// ------------------------------------------------------------------ App: editor input

impl App {
    /// The active view is a file that can be edited.
    pub fn editable_view(&self) -> bool {
        matches!(&self.view, MainView::File(f) if f.ed.editable)
    }

    /// Typing goes to the file editor.
    pub fn editor_focused(&self) -> bool {
        self.overlay.is_none() && self.focus == Focus::Main && self.editable_view() && unsafe { GetFocus() } == self.hwnd
    }

    fn fv(&mut self) -> Option<&mut FileView> {
        match &mut self.view {
            MainView::File(f) if f.ed.editable => Some(f),
            _ => None,
        }
    }

    /// Width of the line number gutter of a file view with `n` lines.
    pub fn gutter_w(&self, n: usize) -> i32 {
        (n.to_string().len() as i32).max(3) * self.fonts.mono_w + self.s(16)
    }

    /// Left edge of a file view's text (scrolled).
    pub fn text_x0(&self, n: usize) -> i32 {
        self.l.main_body.left + self.gutter_w(n) + self.s(4) - self.hscroll
    }

    /// Scrolls so the caret is visible.
    pub fn reveal_caret(&mut self) {
        let (line, x, n) = match &self.view {
            MainView::File(f) if f.ed.editable => (f.ed.cur.0, vcol(&f.lines[f.ed.cur.0], f.ed.cur.1) as i32 * self.fonts.mono_w, f.lines.len()),
            _ => return,
        };
        self.clamp_scroll(Area::Main);
        // wrapped: the caret's visual row, and nothing to scroll sideways
        if let Some(cols) = self.wrap_cols().filter(|_| self.wrap_map().is_some()) {
            let MainView::File(f) = &self.view else { return };
            let segs = crate::wrap::segments(&f.lines[line], Some(cols));
            let v = crate::wrap::vrow(self.wrap_map(), line) + crate::wrap::seg_of(&segs, f.ed.cur.1);
            self.reveal_vrow(v);
            self.hscroll = 0;
            return;
        }
        self.reveal_row(Area::Main, line);
        let body = self.l.main_body;
        let mw = self.fonts.mono_w;
        let view_w = (rw(&body) - self.gutter_w(n) - self.s(4) - self.s(12)).max(mw * 4);
        if x < self.hscroll {
            self.hscroll = (x - mw * 4).max(0);
        } else if x + mw * 2 > self.hscroll + view_w {
            self.hscroll = x + mw * 6 - view_w;
        }
    }

    /// Called after a change to the text: pins a preview tab (an edited file stays open,
    /// like VS Code) and keeps the caret visible.
    fn after_edit(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.tab_active) {
            t.preview = false;
        }
        self.after_move();
    }

    fn after_move(&mut self) {
        self.reveal_caret();
        self.invalidate();
    }

    /// Text position under the mouse.
    pub fn editor_pos_at(&self, x: i32, y: i32) -> Option<Pos> {
        let MainView::File(f) = &self.view else { return None };
        if !f.ed.editable {
            return None;
        }
        let body = self.l.main_body;
        let lh = self.line_h();
        let top = body.top + self.s(4);
        let v = ((y - top + self.scroll[Area::Main as usize]).max(0) / lh) as usize;
        let m = self.wrap_map();
        let (line, sub) = crate::wrap::row_at(m, v);
        let line = line.min(f.lines.len() - 1);
        let rel = x - self.text_x0(f.lines.len());
        let half = ((rel.max(0) * 2) / self.fonts.mono_w.max(1)) as usize;
        if let Some(cols) = self.wrap_cols().filter(|_| m.is_some()) {
            let segs = crate::wrap::segments(&f.lines[line], Some(cols));
            return Some((line, crate::wrap::col_in_seg(&f.lines[line], &segs, sub.min(segs.len() - 1), half)));
        }
        Some((line, col_at_vcol(&f.lines[line], half)))
    }

    pub fn editor_click(&mut self, x: i32, y: i32, shift: bool) {
        self.focus = Focus::Main;
        let Some(p) = self.editor_pos_at(x, y) else { return };
        let now = unsafe { GetMessageTime() } as u32;
        if let Some(f) = self.fv() {
            let triple = matches!(f.ed.dbl_at.take(), Some((t, l)) if l == p.0 && now.wrapping_sub(t) <= unsafe { GetDoubleClickTime() });
            if triple && !shift {
                // no drag: a mouse move would shrink the line selection again
                f.select_line(p.0);
                self.invalidate();
                return;
            }
            f.move_to(p, shift);
            f.ed.want_vcol = None;
        }
        self.drag = Some(Drag::EditSelect);
        self.invalidate();
    }

    pub fn editor_drag(&mut self, x: i32, y: i32) {
        let body = self.l.main_body;
        if y > body.bottom - self.s(4) {
            self.scroll_by(Area::Main, self.line_h());
        } else if y < body.top + self.s(4) {
            self.scroll_by(Area::Main, -self.line_h());
        }
        let Some(p) = self.editor_pos_at(x, y) else { return };
        if let Some(f) = self.fv() {
            f.move_to(p, true);
        }
        self.after_move();
    }

    pub fn editor_dblclick(&mut self, x: i32, y: i32) {
        let Some(p) = self.editor_pos_at(x, y) else { return };
        let now = unsafe { GetMessageTime() } as u32;
        if let Some(f) = self.fv() {
            f.select_word(p);
            f.ed.dbl_at = Some((now, p.0));
        }
        self.invalidate();
    }

    /// Typed character (WM_CHAR).
    pub fn editor_char(&mut self, unit: u16) {
        let Some(f) = self.fv() else { return };
        if (0xD800..0xDC00).contains(&unit) {
            f.ed.surrogate = unit;
            return;
        }
        let ch = if (0xDC00..0xE000).contains(&unit) {
            let hi = std::mem::take(&mut f.ed.surrogate);
            match char::decode_utf16([hi, unit]).next() {
                Some(Ok(c)) => c,
                _ => return,
            }
        } else {
            match char::from_u32(unit as u32) {
                Some(c) => c,
                None => return,
            }
        };
        // control characters (Enter, Tab, Backspace, Ctrl+letter) are handled as keys
        if (ch as u32) < 0x20 || ch == '\u{7F}' {
            return;
        }
        f.type_text(ch.encode_utf8(&mut [0u8; 4]));
        self.after_edit();
    }

    /// Key presses for the editor; returns true when handled. Ctrl shortcuts the editor
    /// doesn't use fall through to the global ones.
    pub fn editor_key(&mut self, vk: u16, ctrl: bool, shift: bool) -> bool {
        let page = (rh(&self.l.main_body) / self.line_h()).max(2) as usize - 1;
        let cols = self.wrap_cols().filter(|_| self.wrap_map().is_some());
        let Some(f) = self.fv() else { return false };
        let cur = f.ed.cur;
        let last = f.lines.len() - 1;
        let line_len = |f: &FileView, l: usize| char_len(&f.lines[l]);
        let mut edited = false;
        let mut page_scroll = 0;
        match (vk, ctrl) {
            // ---- movement
            (0x25, _) => {
                let p = match f.sel() {
                    Some((a, _)) if !shift && !ctrl => a,
                    _ if ctrl => f.word_left(cur),
                    _ if cur.1 > 0 => (cur.0, cur.1 - 1),
                    _ if cur.0 > 0 => (cur.0 - 1, line_len(f, cur.0 - 1)),
                    _ => cur,
                };
                f.move_to(p, shift);
                f.ed.want_vcol = None;
            }
            (0x27, _) => {
                let p = match f.sel() {
                    Some((_, b)) if !shift && !ctrl => b,
                    _ if ctrl => f.word_right(cur),
                    _ if cur.1 < line_len(f, cur.0) => (cur.0, cur.1 + 1),
                    _ if cur.0 < last => (cur.0 + 1, 0),
                    _ => cur,
                };
                f.move_to(p, shift);
                f.ed.want_vcol = None;
            }
            (0x26, false) => {
                let p = f.vmove(cols, -1);
                f.move_to(p, shift);
            }
            (0x28, false) => {
                let p = f.vmove(cols, 1);
                f.move_to(p, shift);
            }
            // Page Up / Down scroll the view by a page along with the caret
            (0x21, false) => {
                let p = f.vmove(cols, -(page as isize));
                f.move_to(p, shift);
                page_scroll = -1;
            }
            (0x22, false) => {
                let p = f.vmove(cols, page as isize);
                f.move_to(p, shift);
                page_scroll = 1;
            }
            (0x24, _) => {
                let p = if ctrl {
                    (0, 0)
                } else {
                    // smart Home: first non-blank char, then column 0
                    let first = f.lines[cur.0].chars().take_while(|c| *c == ' ' || *c == '\t').count();
                    (cur.0, if cur.1 == first { 0 } else { first })
                };
                f.move_to(p, shift);
                f.ed.want_vcol = None;
            }
            (0x23, _) => {
                let l = if ctrl { last } else { cur.0 };
                let p = (l, line_len(f, l));
                f.move_to(p, shift);
                f.ed.want_vcol = None;
            }
            // Ctrl+Up / Ctrl+Down scroll without moving the caret
            (0x26, true) | (0x28, true) => {
                let lh = self.line_h();
                self.scroll_by(Area::Main, if vk == 0x26 { -lh } else { lh });
                return true;
            }
            (0x1B, false) => {
                f.move_to(cur, false);
            }
            // ---- editing
            (0x08, _) => {
                f.backspace(ctrl);
                edited = true;
            }
            (0x2E, _) if !shift => {
                f.delete(ctrl);
                edited = true;
            }
            (0x0D, false) => {
                f.newline();
                edited = true;
            }
            // insert line below / above (instead of the global Commit)
            (0x0D, true) => {
                f.newline_beside(shift);
                edited = true;
            }
            (0x09, false) => {
                f.indent(shift);
                edited = true;
            }
            // ---- Ctrl shortcuts
            // the view stays where it is, like VS Code (revealing the caret would jump to the end)
            (0x41, true) if !shift => {
                f.select_all();
                self.invalidate();
                return true;
            }
            (0x5A, true) if !shift => edited = f.undo(),
            (0x59, true) | (0x5A, true) => edited = f.redo(),
            (0x43, true) | (0x2D, true) if !shift => {
                let (t, whole) = (f.copy_text(), f.sel().is_none());
                win::set_clipboard(self.hwnd, &t);
                set_line_clip(whole);
                return true;
            }
            (0x58, true) | (0x2E, false) if vk == 0x58 || shift => {
                let (t, whole) = (f.copy_text(), f.sel().is_none());
                f.cut();
                win::set_clipboard(self.hwnd, &t);
                set_line_clip(whole);
                edited = true;
            }
            (0x56, true) if !shift => return self.editor_paste(),
            (0x2D, false) if shift => return self.editor_paste(),
            _ => {
                // F-keys and Ctrl shortcuts (Ctrl+S, Ctrl+P, ...) go on to the global keys;
                // everything else stays in the editor
                return !(ctrl || (0x70..=0x87).contains(&vk));
            }
        }
        if page_scroll != 0 {
            let lh = self.line_h();
            self.scroll_by(Area::Main, page_scroll * page as i32 * lh);
        }
        if edited {
            self.after_edit();
        } else {
            self.after_move();
        }
        true
    }

    pub fn editor_paste(&mut self) -> bool {
        let Some(raw) = win::get_clipboard(self.hwnd) else { return true };
        let text = raw.replace("\r\n", "\n").replace('\r', "\n");
        let seq = LINE_CLIP.load(std::sync::atomic::Ordering::Relaxed);
        let whole_line = seq != 0 && seq == clipboard_seq();
        if let Some(f) = self.fv() {
            if whole_line && f.sel().is_none() {
                f.paste_line(&text);
                self.after_edit();
                return true;
            }
            f.type_text(&text);
            if text.chars().count() == 1 {
                f.ed.seal = true;
            }
        }
        self.after_edit();
        true
    }

    /// Context menu of the editor.
    pub fn editor_menu(&mut self, m: &mut Menu) -> Vec<MenuItem> {
        let MainView::File(f) = &self.view else { return Vec::new() };
        let wrap = m.check_cmd("Word Wrap", self.st.word_wrap, Cmd::ToggleWordWrap);
        // a read-only file: finding and wrapping only
        if !f.ed.editable {
            return vec![m.cmd("Find", Cmd::Find), MenuItem::Sep, wrap];
        }
        let (has_sel, can_undo, can_redo, dirty) = (f.sel().is_some(), !f.ed.undo.is_empty(), !f.ed.redo.is_empty(), f.ed.dirty());
        vec![
            m.it_if("Undo\tCtrl+Z", can_undo, |a| {
                a.editor_key(0x5A, true, false);
            }),
            m.it_if("Redo\tCtrl+Y", can_redo, |a| {
                a.editor_key(0x59, true, false);
            }),
            MenuItem::Sep,
            m.it("Cut\tCtrl+X", |a| {
                a.editor_key(0x58, true, false);
            }),
            m.it("Copy\tCtrl+C", |a| {
                a.editor_key(0x43, true, false);
            }),
            m.it("Paste\tCtrl+V", |a| {
                a.editor_paste();
            }),
            MenuItem::Sep,
            m.it_if("Delete\tDel", has_sel, |a| {
                a.editor_key(0x2E, false, false);
            }),
            m.it("Select All\tCtrl+A", |a| {
                a.editor_key(0x41, true, false);
            }),
            MenuItem::Sep,
            m.cmd("Find", Cmd::Find),
            m.cmd("Replace", Cmd::Replace),
            wrap,
            MenuItem::Sep,
            m.it_if("Save\tCtrl+S", dirty, |a| a.save_active()),
            m.it_if("Revert File", dirty, |a| a.revert_file()),
        ]
    }

    // -------------------------------------------------------------- saving

    pub fn tab_file(&self, i: usize) -> Option<&FileView> {
        match self.tabs.get(i).map(|_| self.tab_view(i)) {
            Some(MainView::File(f)) => Some(f),
            _ => None,
        }
    }

    pub fn tab_file_mut(&mut self, i: usize) -> Option<&mut FileView> {
        if i >= self.tabs.len() {
            return None;
        }
        let v = if i == self.tab_active { &mut self.view } else { &mut self.tabs[i].view };
        match v {
            MainView::File(f) => Some(f),
            _ => None,
        }
    }

    pub fn tab_dirty(&self, i: usize) -> bool {
        self.tab_file(i).map(|f| f.ed.dirty()).unwrap_or(false)
    }

    /// Keys of the tabs with unsaved changes.
    pub fn dirty_tab_keys(&self) -> Vec<String> {
        (0..self.tabs.len()).filter(|&i| self.tab_dirty(i)).map(|i| self.tabs[i].key.clone()).collect()
    }

    /// Saves tab `i`; false (with an error toast) when it failed. Unless `force`, a file
    /// changed on disk since it was loaded is not overwritten (Ctrl+S asks about it), so Save
    /// All and the unsaved-changes dialog never clobber outside edits.
    pub fn save_tab(&mut self, i: usize, force: bool) -> bool {
        let Some(root) = self.folder() else { return false };
        let Some(f) = self.tab_file_mut(i) else { return true };
        if !f.ed.editable {
            return true;
        }
        if !force && changed_on_disk(&root, f) {
            let name = f.title.clone();
            self.error(format!("'{name}' has been changed on disk since it was opened. Use Ctrl+S to overwrite it."));
            return false;
        }
        match write_view(&root, f) {
            Ok(()) => {
                self.refresh();
                self.invalidate();
                true
            }
            Err(e) => {
                self.error(e);
                false
            }
        }
    }

    /// Ctrl+S: saves the active file. When the file changed on disk since it was loaded,
    /// asks before overwriting it.
    pub fn save_active(&mut self) {
        let Some(root) = self.folder() else { return };
        let MainView::File(f) = &self.view else { return };
        // like VS Code, a file without changes isn't written again
        if !f.ed.dirty() {
            return;
        }
        if changed_on_disk(&root, f) {
            let name = f.title.clone();
            self.confirm(
                &format!("'{name}' has been changed on disk since it was opened."),
                "Saving overwrites the changes made outside DevDeck.",
                &["Overwrite"],
                |a, _| {
                    let i = a.tab_active;
                    a.save_tab(i, true);
                },
            );
            return;
        }
        let i = self.tab_active;
        self.save_tab(i, false);
    }

    pub fn save_all(&mut self) {
        for i in 0..self.tabs.len() {
            if self.tab_dirty(i) && !self.save_tab(i, false) {
                return;
            }
        }
    }

    /// Discards the active file's changes and reloads it from disk.
    pub fn revert_file(&mut self) {
        let Some(f) = self.fv() else { return };
        f.mark_clean();
        f.mtime = None;
        self.reload_text_view_if_changed();
        self.invalidate();
    }

    /// Runs `then` once the tabs in `keys` have no unsaved changes: asks to save them
    /// (Save / Save All, Don't Save, Cancel). Cancel or a failed save leaves things as they are.
    pub fn confirm_unsaved(&mut self, keys: Vec<String>, then: impl FnOnce(&mut App) + 'static) {
        if keys.is_empty() {
            then(self);
            return;
        }
        let (message, save) = if keys.len() == 1 {
            let i = self.tabs.iter().position(|t| t.key == keys[0]).unwrap_or(0);
            let name = self.tab_file(i).map(|f| f.title.clone()).unwrap_or_default();
            (format!("Do you want to save the changes you made to {name}?"), "Save")
        } else {
            (format!("Do you want to save the changes to the following {} files?", keys.len()), "Save All")
        };
        let detail = if keys.len() == 1 {
            "Your changes will be lost if you don't save them.".to_string()
        } else {
            let names: Vec<String> = keys.iter().filter_map(|k| k.strip_prefix("file:")).map(|p| short_name(p).to_string()).collect();
            format!("{}\n\nYour changes will be lost if you don't save them.", names.join("\n"))
        };
        self.confirm(&message, &detail, &[save, "Don't Save"], move |a, choice| {
            for k in &keys {
                let Some(i) = a.tabs.iter().position(|t| &t.key == k) else { continue };
                if choice == 0 {
                    if !a.save_tab(i, false) {
                        return;
                    }
                } else if let Some(f) = a.tab_file_mut(i) {
                    f.mark_clean();
                }
            }
            then(a);
        });
    }

    // -------------------------------------------------------------- painting

    /// Paints an editable file: line numbers, current line, selection, text and caret.
    pub fn paint_editor(&mut self, g: &Gfx) {
        let body = self.l.main_body;
        let lh = self.line_h();
        let scroll = self.scroll[Area::Main as usize];
        let mw = self.fonts.mono_w.max(1);
        let focused = self.editor_focused();
        let MainView::File(f) = &self.view else { return };
        let count = f.lines.len();
        let num_w = self.gutter_w(count);
        let x0 = self.text_x0(count);
        let top = body.top + self.s(4);
        let clip = rect(body.left + num_w, body.top, body.right - self.s(12), body.bottom);
        let sel = f.sel();
        let cur = f.ed.cur;
        let sel_bg = if focused { blend_colorref(self.t.editor, self.t.accent, 0.35) } else { blend_colorref(self.t.editor, self.t.fg, 0.12) };
        let line_bg = blend_colorref(self.t.editor, self.t.fg, 0.05);
        // chars scrolled out on the left are skipped
        let skip = (self.hscroll / mw).max(0) as usize;
        let visible = (rw(&clip) / mw + 2).max(0) as usize;
        // word wrap: each line paints as its visual rows, each row's text starting at the left
        let m = self.wrap_map();
        let cols = if m.is_some() { self.wrap_cols() } else { None };
        let text_left = x0 + self.hscroll;
        let first = crate::wrap::row_at(m, (scroll / lh).max(0) as usize).0;
        let mut caret = None;
        for li in first..count {
            let y0 = top + crate::wrap::vrow(m, li) as i32 * lh - scroll;
            if y0 >= body.bottom {
                break;
            }
            let line = &f.lines[li];
            let segs = crate::wrap::segments(line, cols);
            let nseg = segs.len();
            if sel.is_none() && li == cur.0 {
                g.fill(&rect(body.left, y0, body.right - self.s(12), y0 + nseg as i32 * lh), line_bg);
            }
            let no_color = if li == cur.0 { self.t.fg } else { self.t.line_no };
            g.text_in(&rect(body.left, y0, body.left + num_w - self.s(8), y0 + lh), &(li + 1).to_string(), self.fonts.mono, no_color, DT_LINE | DT_RIGHT);
            // the cells up to the last row in view
            let last_vis = (((body.bottom - y0) / lh).max(0) as usize).min(nseg - 1);
            let cells = display_chars(line, if cols.is_some() { segs[last_vis].ve } else { skip + visible });
            let caret_seg = (li == cur.0).then(|| crate::wrap::seg_of(&segs, cur.1));
            for (k, sg) in segs.iter().enumerate().take(last_vis + 1) {
                let y = y0 + k as i32 * lh;
                if y + lh <= body.top {
                    continue;
                }
                // a row's own x origin and clip, so ranges past its ends are cut off
                let (x0, clip) = if cols.is_some() {
                    let right = if k + 1 == nseg { clip.right } else { (text_left + (sg.ve - sg.vs) as i32 * mw).min(clip.right) };
                    (text_left - sg.vs as i32 * mw, rect(text_left, y, right, y + lh))
                } else {
                    (x0, rect(clip.left, y, clip.right, y + lh))
                };
                self.paint_find_line(g, li, line, y, x0, &clip, false);
                if let Some((a, b)) = sel.filter(|(a, b)| li >= a.0 && li <= b.0) {
                    let c0 = if li == a.0 { vcol(line, a.1) } else { 0 } as i32;
                    // the line break of a selected line shows as one extra cell
                    let c1 = if li == b.0 { vcol(line, b.1) as i32 } else { vcol(line, char_len(line)) as i32 + 1 };
                    let r = rect((x0 + c0 * mw).max(clip.left), y, (x0 + c1 * mw).min(clip.right), y + lh);
                    if r.right > r.left {
                        g.fill(&r, sel_bg);
                    }
                }
                self.paint_find_line(g, li, line, y, x0, &clip, true);
                let (from, to) = if cols.is_some() { (sg.vs, sg.ve) } else { (skip, skip + visible) };
                let to = to.min(cells.len());
                if from < to {
                    g.text_cells(x0 + from as i32 * mw, y + self.s(1), &cells[from..to], mw, self.fonts.mono, self.t.fg, &clip);
                }
                if let Some(l) = self.link.filter(|l| l.at == crate::link::At::Editor && l.line == li) {
                    g.hline((x0 + vcol(line, l.s) as i32 * mw).max(clip.left), (x0 + vcol(line, l.e) as i32 * mw).min(clip.right), y + lh - 2, self.t.fg);
                }
                if focused && caret_seg == Some(k) {
                    caret = Some((x0 + vcol(line, cur.1) as i32 * mw, y, clip));
                }
            }
        }
        if let Some((x, y, c)) = caret {
            if x >= c.left - 1 && x < c.right.max(c.left + 1) {
                g.fill(&rect(x, y, x + self.s(2).max(2), y + lh), self.t.fg_strong);
            }
        }
        self.hits.push((rect(body.left, body.top, body.right - self.s(12), body.bottom), Hit::EditorText));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn view(text: &str) -> FileView {
        let lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        let ed = EditState::new(&lines, Enc::Utf8, false, true);
        FileView { title: "t".into(), path: "t".into(), lines, conflicts: Vec::new(), change: None, mtime: None, ed, nav: None }
    }

    fn text(f: &FileView) -> String {
        f.lines.join("\n")
    }

    #[test]
    fn replace_and_range_across_lines() {
        let mut lines: Vec<String> = vec!["héllo".into(), "big".into(), "world".into()];
        assert_eq!(text_range(&lines, (0, 2), (2, 1)), "llo\nbig\nw");
        assert_eq!(range_len(&lines, (0, 2), (2, 1)), 9);
        let end = replace(&mut lines, (0, 2), (2, 1), "X\nY");
        assert_eq!(lines, ["héX", "Yorld"]);
        assert_eq!(end, (1, 1));
        let end = replace(&mut lines, (1, 0), (1, 0), "");
        assert_eq!(end, (1, 0));
    }

    #[test]
    fn typing_coalesces_into_one_undo_step() {
        let mut f = view("ab");
        f.move_to((0, 1), false);
        for c in ["x", "y", "z"] {
            f.type_text(c);
        }
        assert_eq!(text(&f), "axyzb");
        assert!(f.ed.dirty());
        f.undo();
        assert_eq!(text(&f), "ab");
        assert_eq!(f.ed.cur, (0, 1));
        assert!(!f.ed.dirty());
        f.redo();
        assert_eq!(text(&f), "axyzb");
        assert_eq!(f.ed.cur, (0, 4));
    }

    #[test]
    fn backspace_delete_and_join_lines() {
        let mut f = view("one\ntwo");
        f.move_to((1, 0), false);
        f.backspace(false);
        assert_eq!(text(&f), "onetwo");
        assert_eq!(f.ed.cur, (0, 3));
        f.delete(false);
        assert_eq!(text(&f), "onewo");
        f.move_to((0, 5), false);
        f.backspace(true);
        assert_eq!(text(&f), "");
        f.undo();
        f.undo();
        f.undo();
        assert_eq!(text(&f), "one\ntwo");
    }

    #[test]
    fn newline_keeps_indentation_and_selection_is_replaced() {
        let mut f = view("    let x = 1;");
        f.move_to((0, 14), false);
        f.newline();
        assert_eq!(text(&f), "    let x = 1;\n    ");
        assert_eq!(f.ed.cur, (1, 4));
        f.move_to((0, 8), false);
        f.move_to((0, 9), true);
        f.type_text("y");
        assert_eq!(f.lines[0], "    let y = 1;");
    }

    #[test]
    fn ctrl_enter_inserts_line_below_or_above() {
        let mut f = view("  foo(bar)\nx");
        f.move_to((0, 6), false);
        f.newline_beside(false);
        assert_eq!(text(&f), "  foo(bar)\n  \nx");
        assert_eq!(f.ed.cur, (1, 2));
        f.undo();
        assert_eq!(text(&f), "  foo(bar)\nx");
        assert_eq!(f.ed.cur, (0, 6));
        f.newline_beside(true);
        assert_eq!(text(&f), "  \n  foo(bar)\nx");
        assert_eq!(f.ed.cur, (0, 2));
        f.undo();
        assert_eq!(text(&f), "  foo(bar)\nx");
    }

    #[test]
    fn whole_line_paste_and_line_select() {
        let mut f = view("a\nbc");
        f.move_to((1, 1), false);
        f.paste_line("a\n");
        assert_eq!(text(&f), "a\na\nbc");
        assert_eq!(f.ed.cur, (2, 1));
        f.undo();
        assert_eq!(text(&f), "a\nbc");
        f.select_line(0);
        assert_eq!(f.selected_text().as_deref(), Some("a\n"));
        f.select_line(1);
        assert_eq!(f.selected_text().as_deref(), Some("bc"));
    }

    #[test]
    fn indent_and_outdent_lines() {
        let mut f = view("a\n  b\nc");
        f.move_to((0, 0), false);
        f.move_to((1, 1), true);
        f.indent(false);
        assert_eq!(text(&f), "  a\n    b\nc");
        f.indent(true);
        assert_eq!(text(&f), "a\n  b\nc");
        // single caret: spaces up to the next stop
        f.move_to((2, 1), false);
        f.indent(false);
        assert_eq!(f.lines[2], "c ");
    }

    #[test]
    fn word_moves_and_cut_line() {
        let f = view("foo.bar  baz");
        assert_eq!(f.word_right((0, 0)), (0, 3));
        assert_eq!(f.word_right((0, 3)), (0, 4));
        assert_eq!(f.word_right((0, 7)), (0, 12));
        assert_eq!(f.word_left((0, 12)), (0, 9));
        let mut f = view("a\nb\nc");
        f.move_to((1, 0), false);
        assert_eq!(f.copy_text(), "b\r\n");
        f.cut();
        assert_eq!(text(&f), "a\nc");
    }

    #[test]
    fn columns_with_tabs() {
        assert_eq!(vcol("\tab", 1), 4);
        assert_eq!(vcol("a\tb", 2), 4);
        assert_eq!(col_at_vcol("\tab", 2), 0); // nearer the tab's start
        assert_eq!(col_at_vcol("\tab", 3 * 2), 1); // nearer its end
        assert_eq!(col_at_vcol("\tab", 4 * 2), 1);
        assert_eq!(col_at_vcol("ab", 3), 2); // right half of 'b'
        assert_eq!(col_at_vcol("ab", 99), 2);
        assert_eq!(display_chars("a\tb", 99), ['a', ' ', ' ', ' ', 'b']);
        // cut off at the visible cells, tabs still expanded from the line start
        assert_eq!(display_chars("a\tbcd", 5), ['a', ' ', ' ', ' ', 'b']);
        assert_eq!(display_chars("\t\tx", 2), [' ', ' ', ' ', ' ']);
        assert_eq!(display_chars(&"x".repeat(1 << 20), 10).len(), 10);
    }

    #[test]
    fn encodings_round_trip() {
        for (bytes, enc, crlf) in [
            (b"a\r\nb\r\n".to_vec(), Enc::Utf8, true),
            ([&[0xEF, 0xBB, 0xBF][..], "x\ny".as_bytes()].concat(), Enc::Utf8Bom, false),
            (vec![0xFF, 0xFE, b'h', 0, b'\n', 0, b'i', 0], Enc::Utf16Le, false),
            (vec![0xFE, 0xFF, 0, b'h', 0, b'i'], Enc::Utf16Be, false),
        ] {
            let (t, e, ok) = decode(&bytes).unwrap();
            assert!(ok);
            assert_eq!(e, enc);
            let lines: Vec<String> = t.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
            assert_eq!(encode(&lines, e, t.contains("\r\n")), bytes);
            assert_eq!(t.contains("\r\n"), crlf);
        }
        assert!(!decode(b"caf\xE9 au lait").unwrap().2);
        assert!(decode(&[b'a', 0, b'b']).is_none());
    }
}
