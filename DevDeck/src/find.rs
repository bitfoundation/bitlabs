//! Find / Replace in file views, like VS Code's find widget: match case, whole word, regular
//! expressions, preserve case, find in selection, find all (every match highlighted, with marks
//! on the scroll bar and an "N of M" count), replace and replace all (one undo step).
//!
//! The search runs on the view's lines without copying the text. Plain text uses the standard
//! substring search; regular expressions use a small Pike VM (Thompson NFA with captures) that
//! runs in linear time on any pattern, so no regex crate adds to the binary. The syntax is
//! the JavaScript one VS Code uses, without backreferences and lookaround.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::*;
use crate::editor::{bidx, char_len, vcol, Pos};
use crate::theme::blend_colorref;
use crate::win::{self, rect, rh, rw, xywh, Gfx, DT_LINE};

/// A match: start and end positions.
pub type Range = (Pos, Pos);
/// Matches kept for highlighting and counting, like VS Code ("19999+").
pub const MATCH_LIMIT: usize = 19999;
/// Size limits of a compiled regular expression.
const MAX_INSTS: usize = 5000;
const MAX_REPEAT: u32 = 1000;
const MAX_DEPTH: usize = 200;
const HISTORY: usize = 20;

// ------------------------------------------------------------------ text helpers

/// Simple case folding (one char to one char, so columns don't change).
pub fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

fn upper1(c: char) -> char {
    let mut u = c.to_uppercase();
    match (u.next(), u.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `\w` of JavaScript regular expressions (ASCII).
fn is_re_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A position with its byte offset in the line, which the search works with.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct P {
    pub l: usize,
    pub c: usize,
    pub b: usize,
}

impl P {
    pub fn pos(self) -> Pos {
        (self.l, self.c)
    }
}

fn at(lines: &[String], p: Pos) -> P {
    P { l: p.0, c: p.1, b: bidx(&lines[p.0], p.1) }
}

/// The char at `p` and the position after it; with `single` the text ends at the line end,
/// otherwise a line break ('\n') comes between lines.
fn char_at(lines: &[String], p: P, single: bool) -> Option<(char, P)> {
    let line = &lines[p.l];
    if let Some(ch) = line[p.b..].chars().next() {
        return Some((ch, P { l: p.l, c: p.c + 1, b: p.b + ch.len_utf8() }));
    }
    if single || p.l + 1 >= lines.len() {
        return None;
    }
    Some(('\n', P { l: p.l + 1, c: 0, b: 0 }))
}

fn char_before(lines: &[String], p: P, single: bool) -> Option<char> {
    match lines[p.l][..p.b].chars().next_back() {
        Some(c) => Some(c),
        None if !single && p.l > 0 => Some('\n'),
        None => None,
    }
}

/// Appends the text from `a` to `b` (ordered), lines joined with '\n'.
fn push_text(out: &mut String, lines: &[String], a: P, b: P) {
    if a.l == b.l {
        out.push_str(&lines[a.l][a.b..b.b]);
        return;
    }
    out.push_str(&lines[a.l][a.b..]);
    for l in &lines[a.l + 1..b.l] {
        out.push('\n');
        out.push_str(l);
    }
    out.push('\n');
    out.push_str(&lines[b.l][..b.b]);
}

fn text_of(lines: &[String], a: P, b: P) -> String {
    let mut s = String::new();
    push_text(&mut s, lines, a, b);
    s
}

/// VS Code's whole word rule: each end of the match is at a word boundary, or the match itself
/// starts / ends with a separator there.
fn word_ok(lines: &[String], s: P, e: P) -> bool {
    let before = lines[s.l][..s.b].chars().next_back();
    let after = lines[e.l][e.b..].chars().next();
    let first = char_at(lines, s, false).map(|c| c.0).filter(|_| s != e);
    let last = if s == e { None } else { char_before(lines, e, false) };
    let left = !before.is_some_and(is_word) || !first.is_some_and(is_word);
    let right = !after.is_some_and(is_word) || !last.is_some_and(is_word);
    left && right
}

/// Converts byte offsets to char columns (and back) along one string, moving forward only.
struct Cols<'a> {
    s: &'a str,
    b: usize,
    c: usize,
    ascii: bool,
}

impl<'a> Cols<'a> {
    fn new(s: &'a str, b: usize, c: usize) -> Cols<'a> {
        Cols { s, b, c, ascii: s.is_ascii() }
    }
    fn col(&mut self, b: usize) -> usize {
        if self.ascii {
            return b;
        }
        self.c += self.s[self.b..b].chars().count();
        self.b = b;
        self.c
    }
    fn byte(&mut self, c: usize) -> usize {
        if self.ascii {
            return c;
        }
        while self.c < c {
            let Some(ch) = self.s[self.b..].chars().next() else { break };
            self.b += ch.len_utf8();
            self.c += 1;
        }
        self.b
    }
}

/// Byte where `p` (folded when `!case`) ends `line`.
fn suffix_at(line: &str, p: &str, case: bool) -> Option<usize> {
    if case {
        return line.ends_with(p).then(|| line.len() - p.len());
    }
    let mut it = line.char_indices().rev();
    let mut b = line.len();
    for pc in p.chars().rev() {
        let (i, lc) = it.next()?;
        if fold(lc) != pc {
            return None;
        }
        b = i;
    }
    Some(b)
}

/// Byte where `p` (folded when `!case`) ends when it starts `line`.
fn prefix_at(line: &str, p: &str, case: bool) -> Option<usize> {
    if case {
        return line.starts_with(p).then_some(p.len());
    }
    let mut it = line.char_indices();
    for pc in p.chars() {
        let (_, lc) = it.next()?;
        if fold(lc) != pc {
            return None;
        }
    }
    Some(it.next().map(|(i, _)| i).unwrap_or(line.len()))
}

/// Escapes regular expression syntax (a selection seeds the query with regex on).
pub fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.*+?()[]{}|/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A regular expression that can match line breaks, by VS Code's rule: it contains a line
/// break, `\n`, `\r` or `\W`; others are matched line by line.
fn multiline_source(q: &str) -> bool {
    let b = q.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\n' => return true,
            b'\\' if i + 1 < b.len() => {
                if matches!(b[i + 1], b'n' | b'r' | b'W') {
                    return true;
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

// ------------------------------------------------------------------ regular expressions

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Look {
    Bol,
    Eol,
    Word,
    NotWord,
}

#[derive(Clone, Copy, Debug)]
enum Item {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

impl Item {
    fn hit(self, c: char) -> bool {
        match self {
            Item::Range(a, b) => a <= c && c <= b,
            Item::Digit(neg) => c.is_ascii_digit() != neg,
            Item::Word(neg) => is_re_word(c) != neg,
            Item::Space(neg) => c.is_whitespace() != neg,
        }
    }
}

#[derive(Debug)]
struct Class {
    neg: bool,
    items: Vec<Item>,
}

impl Class {
    fn hit(&self, c: char, ci: bool) -> bool {
        let any = |c: char| self.items.iter().any(|i| i.hit(c));
        let m = any(c) || ci && (any(fold(c)) || any(upper1(c)));
        m != self.neg
    }
}

#[derive(Debug)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class(usize),
    Look(Look),
    Group(Box<Node>, Option<usize>),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Rep(Box<Node>, u32, Option<u32>, bool),
}

struct Parser {
    s: Vec<char>,
    i: usize,
    groups: usize,
    classes: Vec<Class>,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }
    fn peek_at(&self, k: usize) -> Option<char> {
        self.s.get(self.i + k).copied()
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn starts(&self, p: &str) -> bool {
        p.chars().enumerate().all(|(k, c)| self.peek_at(k) == Some(c))
    }
    fn class(&mut self, neg: bool, items: Vec<Item>) -> Node {
        self.classes.push(Class { neg, items });
        Node::Class(self.classes.len() - 1)
    }

    fn alt(&mut self) -> Result<Node, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("Regular expression is too deeply nested".into());
        }
        let mut v = vec![self.cat()?];
        while self.eat('|') {
            v.push(self.cat()?);
        }
        self.depth -= 1;
        Ok(if v.len() == 1 { v.pop().unwrap() } else { Node::Alt(v) })
    }

    fn cat(&mut self) -> Result<Node, String> {
        let mut v = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let a = self.atom()?;
            v.push(self.quant(a)?);
        }
        Ok(match v.len() {
            0 => Node::Empty,
            1 => v.pop().unwrap(),
            _ => Node::Cat(v),
        })
    }

    /// A `{n}`, `{n,}` or `{n,m}` quantifier at the current position: (min, max, length).
    fn braces(&self) -> Option<(u32, Option<u32>, usize)> {
        let mut k = 1;
        let num = |k: &mut usize| -> Option<u32> {
            let start = *k;
            let mut v: u32 = 0;
            while let Some(d) = self.peek_at(*k).and_then(|c| c.to_digit(10)) {
                v = v.saturating_mul(10).saturating_add(d);
                *k += 1;
            }
            (*k > start).then_some(v)
        };
        if self.peek() != Some('{') {
            return None;
        }
        let min = num(&mut k)?;
        let max = if self.peek_at(k) == Some(',') {
            k += 1;
            num(&mut k)
        } else {
            Some(min)
        };
        (self.peek_at(k) == Some('}')).then_some((min, max, k + 1))
    }

    fn quant(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, None)
            }
            Some('+') => {
                self.i += 1;
                (1, None)
            }
            Some('?') => {
                self.i += 1;
                (0, Some(1))
            }
            Some('{') => match self.braces() {
                Some((min, max, len)) => {
                    self.i += len;
                    (min, max)
                }
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Look(_)) {
            return Err("Nothing to repeat".into());
        }
        if max.is_some_and(|m| m < min) {
            return Err("Numbers out of order in {} quantifier".into());
        }
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT) {
            return Err("Repetition count is too large".into());
        }
        let greedy = !self.eat('?');
        if matches!(self.peek(), Some('*' | '+' | '?')) || self.braces().is_some() {
            return Err("Nothing to repeat".into());
        }
        // `\r?` (an empty node: lines have no carriage returns) stays empty
        if matches!(atom, Node::Empty) {
            return Ok(Node::Empty);
        }
        Ok(Node::Rep(Box::new(atom), min, max, greedy))
    }

    fn atom(&mut self) -> Result<Node, String> {
        let c = self.peek().unwrap();
        self.i += 1;
        Ok(match c {
            '(' => {
                let cap = if self.starts("?:") {
                    self.i += 2;
                    None
                } else if self.starts("?=") || self.starts("?!") || self.starts("?<=") || self.starts("?<!") {
                    return Err("Lookaround assertions are not supported".into());
                } else if self.starts("?<") {
                    // a named group is captured like any other
                    while let Some(c) = self.peek() {
                        self.i += 1;
                        if c == '>' {
                            break;
                        }
                    }
                    self.groups += 1;
                    Some(self.groups)
                } else if self.peek() == Some('?') {
                    return Err("Invalid group".into());
                } else {
                    self.groups += 1;
                    Some(self.groups)
                };
                let inner = self.alt()?;
                if !self.eat(')') {
                    return Err("Unterminated group".into());
                }
                Node::Group(Box::new(inner), cap)
            }
            '[' => self.bracket()?,
            '.' => Node::Any,
            '^' => Node::Look(Look::Bol),
            '$' => Node::Look(Look::Eol),
            '\\' => self.escape()?,
            '*' | '+' | '?' => return Err("Nothing to repeat".into()),
            '{' => {
                self.i -= 1;
                if self.braces().is_some() {
                    return Err("Nothing to repeat".into());
                }
                self.i += 1;
                Node::Char('{')
            }
            c => Node::Char(c),
        })
    }

    /// A char escape shared by classes and the rest: \n \t \xHH \uHHHH \u{H...} and so on.
    fn char_escape(&mut self, c: char) -> Option<char> {
        let hex = |p: &mut Parser, n: usize| -> Option<char> {
            let s: String = (0..n).map(|k| p.peek_at(k)).collect::<Option<String>>()?;
            let v = u32::from_str_radix(&s, 16).ok()?;
            p.i += n;
            char::from_u32(v)
        };
        Some(match c {
            'n' => '\n',
            't' => '\t',
            'f' => '\x0C',
            'v' => '\x0B',
            '0' => '\0',
            'x' => hex(self, 2).unwrap_or('x'),
            'u' if self.peek() == Some('{') => {
                let end = (self.i..self.s.len()).find(|&k| self.s[k] == '}')?;
                let v: String = self.s[self.i + 1..end].iter().collect();
                let ch = char::from_u32(u32::from_str_radix(&v, 16).ok()?)?;
                self.i = end + 1;
                ch
            }
            'u' => hex(self, 4).unwrap_or('u'),
            _ => return None,
        })
    }

    fn escape(&mut self) -> Result<Node, String> {
        let Some(c) = self.peek() else { return Err("\\ at end of pattern".into()) };
        self.i += 1;
        Ok(match c {
            'd' | 'D' => self.class(false, vec![Item::Digit(c == 'D')]),
            'w' | 'W' => self.class(false, vec![Item::Word(c == 'W')]),
            's' | 'S' => self.class(false, vec![Item::Space(c == 'S')]),
            'b' => Node::Look(Look::Word),
            'B' => Node::Look(Look::NotWord),
            // lines have no carriage returns: `\r\n` matches a line break of any file
            'r' => Node::Empty,
            '1'..='9' => return Err("Backreferences are not supported".into()),
            _ => Node::Char(self.char_escape(c).unwrap_or(c)),
        })
    }

    /// One member of a character class: a class escape or a char.
    fn class_atom(&mut self) -> Result<Result<char, Item>, String> {
        let Some(c) = self.peek() else { return Err("Unterminated character class".into()) };
        self.i += 1;
        if c != '\\' {
            return Ok(Ok(c));
        }
        let Some(e) = self.peek() else { return Err("Unterminated character class".into()) };
        self.i += 1;
        Ok(match e {
            'd' | 'D' => Err(Item::Digit(e == 'D')),
            'w' | 'W' => Err(Item::Word(e == 'W')),
            's' | 'S' => Err(Item::Space(e == 'S')),
            'b' => Ok('\x08'),
            'r' => Ok('\r'),
            _ => Ok(self.char_escape(e).unwrap_or(e)),
        })
    }

    fn bracket(&mut self) -> Result<Node, String> {
        let neg = self.eat('^');
        let mut items = Vec::new();
        loop {
            if self.eat(']') {
                break;
            }
            let lo = self.class_atom()?;
            let lo = match lo {
                Err(item) => {
                    items.push(item);
                    continue;
                }
                Ok(c) => c,
            };
            if self.peek() == Some('-') && self.peek_at(1).is_some_and(|c| c != ']') {
                self.i += 1;
                match self.class_atom()? {
                    Ok(hi) => {
                        if hi < lo {
                            return Err("Range out of order in character class".into());
                        }
                        items.push(Item::Range(lo, hi));
                    }
                    Err(item) => {
                        items.extend([Item::Range(lo, lo), Item::Range('-', '-'), item]);
                    }
                }
            } else {
                items.push(Item::Range(lo, lo));
            }
        }
        Ok(self.class(neg, items))
    }
}

#[derive(Clone, Copy, Debug)]
enum Inst {
    /// A char (folded when the program ignores case).
    Char(char),
    Any,
    Class(usize),
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Assert(Look),
    Match,
}

pub struct Prog {
    insts: Vec<Inst>,
    classes: Vec<Class>,
    ncap: usize,
    ci: bool,
    multi: bool,
    /// The chars a match can start with (folded when ignoring case), to skip ahead.
    first: Option<Vec<char>>,
    /// The text every match starts with when the pattern begins with plain chars (case
    /// sensitive): candidates are found with the substring search.
    prefix: String,
}

struct Compiler {
    insts: Vec<Inst>,
    ci: bool,
}

impl Compiler {
    fn push(&mut self, i: Inst) -> usize {
        self.insts.push(i);
        self.insts.len() - 1
    }

    fn emit(&mut self, n: &Node) -> Result<(), String> {
        if self.insts.len() > MAX_INSTS {
            return Err("Regular expression is too large".into());
        }
        match n {
            Node::Empty => {}
            Node::Char(c) => {
                let c = if self.ci { fold(*c) } else { *c };
                self.push(Inst::Char(c));
            }
            Node::Any => {
                self.push(Inst::Any);
            }
            Node::Class(k) => {
                self.push(Inst::Class(*k));
            }
            Node::Look(l) => {
                self.push(Inst::Assert(*l));
            }
            Node::Group(inner, cap) => match cap {
                Some(k) => {
                    self.push(Inst::Save(2 * k));
                    self.emit(inner)?;
                    self.push(Inst::Save(2 * k + 1));
                }
                None => self.emit(inner)?,
            },
            Node::Cat(v) => {
                for x in v {
                    self.emit(x)?;
                }
            }
            Node::Alt(v) => {
                let mut jumps = Vec::new();
                for (k, x) in v.iter().enumerate() {
                    if k + 1 < v.len() {
                        let split = self.push(Inst::Split(0, 0));
                        self.emit(x)?;
                        jumps.push(self.push(Inst::Jmp(0)));
                        self.insts[split] = Inst::Split(split + 1, self.insts.len());
                    } else {
                        self.emit(x)?;
                    }
                }
                let end = self.insts.len();
                for j in jumps {
                    self.insts[j] = Inst::Jmp(end);
                }
            }
            Node::Rep(x, min, max, greedy) => {
                for _ in 0..*min {
                    self.emit(x)?;
                }
                let order = |a: usize, b: usize| if *greedy { Inst::Split(a, b) } else { Inst::Split(b, a) };
                match max {
                    None => {
                        let split = self.push(Inst::Split(0, 0));
                        self.emit(x)?;
                        self.push(Inst::Jmp(split));
                        self.insts[split] = order(split + 1, self.insts.len());
                    }
                    Some(m) => {
                        let mut splits = Vec::new();
                        for _ in *min..*m {
                            splits.push(self.push(Inst::Split(0, 0)));
                            self.emit(x)?;
                        }
                        let end = self.insts.len();
                        for s in splits {
                            self.insts[s] = order(s + 1, end);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn compile(src: &str, ci: bool) -> Result<Prog, String> {
    let mut p = Parser { s: src.chars().collect(), i: 0, groups: 0, classes: Vec::new(), depth: 0 };
    let node = p.alt()?;
    if p.i < p.s.len() {
        return Err("Unmatched ')'".into());
    }
    let mut c = Compiler { insts: vec![Inst::Save(0)], ci };
    c.emit(&node)?;
    c.push(Inst::Save(1));
    c.push(Inst::Match);
    let insts = c.insts;
    let first = first_chars(&insts);
    // nothing jumps over the leading chars, so every match starts with them
    let prefix = if ci { String::new() } else { insts[1..].iter().map_while(|i| if let Inst::Char(c) = i { Some(*c) } else { None }).take_while(|c| *c != '\n').collect() };
    Ok(Prog { insts, classes: p.classes, ncap: 2 * (p.groups + 1), ci, multi: multiline_source(src), first, prefix })
}

/// The chars every match starts with, when there are only a few (else None).
fn first_chars(insts: &[Inst]) -> Option<Vec<char>> {
    let mut set = Vec::new();
    let mut seen = vec![false; insts.len()];
    let mut stack = vec![0];
    while let Some(pc) = stack.pop() {
        if std::mem::replace(&mut seen[pc], true) {
            continue;
        }
        match insts[pc] {
            Inst::Char(c) => {
                if !set.contains(&c) {
                    set.push(c);
                }
                if set.len() > 16 {
                    return None;
                }
            }
            Inst::Split(a, b) => stack.extend([b, a]),
            Inst::Jmp(a) => stack.push(a),
            // assertions only restrict where a match starts
            Inst::Save(_) | Inst::Assert(_) => stack.push(pc + 1),
            Inst::Any | Inst::Class(_) | Inst::Match => return None,
        }
    }
    Some(set)
}

/// Thread list of the VM: a sparse set of instructions, in priority order, with the
/// captures of each.
#[derive(Default)]
struct Threads {
    sparse: Vec<usize>,
    dense: Vec<usize>,
    caps: Vec<Option<P>>,
    ncap: usize,
}

impl Threads {
    fn reset(&mut self, n: usize, ncap: usize) {
        if self.sparse.len() != n || self.ncap != ncap {
            self.sparse = vec![0; n];
            self.caps = vec![None; n * ncap];
            self.ncap = ncap;
        }
        self.dense.clear();
    }
    fn has(&self, pc: usize) -> bool {
        let i = self.sparse[pc];
        i < self.dense.len() && self.dense[i] == pc
    }
    fn insert(&mut self, pc: usize) {
        self.sparse[pc] = self.dense.len();
        self.dense.push(pc);
    }
    fn caps(&self, pc: usize) -> &[Option<P>] {
        &self.caps[pc * self.ncap..(pc + 1) * self.ncap]
    }
}

enum Job {
    Pc(usize),
    Restore(usize, Option<P>),
}

/// What assertions see at a position.
struct Ctx {
    prev: Option<char>,
    next: Option<char>,
    bol: bool,
    eol: bool,
}

impl Ctx {
    fn new(lines: &[String], p: P, prev: Option<char>, next: Option<char>) -> Ctx {
        Ctx { prev, next, bol: p.b == 0, eol: p.b == lines[p.l].len() }
    }
    fn check(&self, l: Look) -> bool {
        match l {
            Look::Bol => self.bol,
            Look::Eol => self.eol,
            Look::Word | Look::NotWord => {
                let w = self.prev.is_some_and(is_re_word) != self.next.is_some_and(is_re_word);
                w == (l == Look::Word)
            }
        }
    }
}

#[derive(Default)]
struct Vm {
    clist: Threads,
    nlist: Threads,
    stack: Vec<Job>,
    scratch: Vec<Option<P>>,
    /// Captures of the last match: slots 0 and 1 are the whole match.
    found: Vec<Option<P>>,
}

/// Follows the empty transitions from `pc` and adds the threads they reach to `list`.
fn add(prog: &Prog, list: &mut Threads, stack: &mut Vec<Job>, caps: &mut [Option<P>], pc: usize, cx: &Ctx, at: P) {
    stack.push(Job::Pc(pc));
    while let Some(j) = stack.pop() {
        match j {
            Job::Restore(slot, v) => caps[slot] = v,
            Job::Pc(mut pc) => loop {
                if list.has(pc) {
                    break;
                }
                list.insert(pc);
                match prog.insts[pc] {
                    Inst::Split(a, b) => {
                        stack.push(Job::Pc(b));
                        pc = a;
                    }
                    Inst::Jmp(a) => pc = a,
                    Inst::Save(s) => {
                        stack.push(Job::Restore(s, caps[s]));
                        caps[s] = Some(at);
                        pc += 1;
                    }
                    Inst::Assert(l) => {
                        if !cx.check(l) {
                            break;
                        }
                        pc += 1;
                    }
                    _ => {
                        let n = list.ncap;
                        list.caps[pc * n..(pc + 1) * n].copy_from_slice(caps);
                        break;
                    }
                }
            },
        }
    }
}

/// Next position at or after `p` where a match can start, given the text or the chars it
/// starts with.
fn skip(lines: &[String], mut p: P, single: bool, set: &[char], prog: &Prog) -> Option<P> {
    loop {
        let line = &lines[p.l];
        let rest = &line[p.b..];
        let found = if prog.prefix.chars().nth(1).is_some() { rest.find(prog.prefix.as_str()) } else { rest.char_indices().find(|&(_, ch)| set.contains(&if prog.ci { fold(ch) } else { ch })).map(|(off, _)| off) };
        if let Some(off) = found {
            return Some(P { l: p.l, c: p.c + rest[..off].chars().count(), b: p.b + off });
        }
        if single || p.l + 1 >= lines.len() {
            return None;
        }
        if set.contains(&'\n') {
            return Some(P { l: p.l, c: p.c + rest.chars().count(), b: line.len() });
        }
        p = P { l: p.l + 1, c: 0, b: 0 };
    }
}

impl Vm {
    /// Finds the leftmost match at or after `start` (preferring alternatives in order, like
    /// a backtracking engine); its captures are left in `found`.
    fn exec(&mut self, prog: &Prog, lines: &[String], start: P, single: bool) -> bool {
        let n = prog.insts.len();
        let Vm { clist, nlist, stack, scratch, found } = self;
        clist.reset(n, prog.ncap);
        nlist.reset(n, prog.ncap);
        scratch.clear();
        scratch.resize(prog.ncap, None);
        let mut p = start;
        let mut prev = char_before(lines, p, single);
        let mut cur = char_at(lines, p, single);
        let mut matched = false;
        loop {
            if !matched {
                if clist.dense.is_empty() {
                    if let Some(set) = &prog.first {
                        let np = skip(lines, p, single, set, prog);
                        match np {
                            Some(np) if np != p => {
                                p = np;
                                prev = char_before(lines, p, single);
                                cur = char_at(lines, p, single);
                            }
                            Some(_) => {}
                            None => return false,
                        }
                    }
                }
                // a new thread starts here, with the lowest priority
                scratch.iter_mut().for_each(|c| *c = None);
                let cx = Ctx::new(lines, p, prev, cur.map(|c| c.0));
                add(prog, clist, stack, scratch, 0, &cx, p);
            }
            if clist.dense.is_empty() {
                break;
            }
            let x = cur.map(|c| c.0);
            let xf = x.map(|c| if prog.ci { fold(c) } else { c });
            let next = cur.and_then(|(_, np)| char_at(lines, np, single));
            let ncx = cur.map(|(c, np)| (np, Ctx::new(lines, np, Some(c), next.map(|n| n.0))));
            for k in 0..clist.dense.len() {
                let pc = clist.dense[k];
                let ok = match prog.insts[pc] {
                    Inst::Match => {
                        found.clear();
                        found.extend_from_slice(clist.caps(pc));
                        matched = true;
                        // threads of lower priority are cut off
                        break;
                    }
                    Inst::Char(c) => xf == Some(c),
                    Inst::Any => x.is_some_and(|c| c != '\n'),
                    Inst::Class(i) => x.is_some_and(|c| (c != '\n' || prog.multi) && prog.classes[i].hit(c, prog.ci)),
                    _ => false,
                };
                if ok {
                    if let Some((np, cx)) = &ncx {
                        scratch.copy_from_slice(clist.caps(pc));
                        add(prog, nlist, stack, scratch, pc + 1, cx, *np);
                    }
                }
            }
            let Some((_, np)) = cur else { break };
            std::mem::swap(clist, nlist);
            nlist.dense.clear();
            prev = x;
            p = np;
            cur = next;
        }
        matched
    }
}

// ------------------------------------------------------------------ search

enum Kind {
    Lit { needle: String, nchars: usize },
    /// A plain text query with line breaks: the parts between them.
    Multi(Vec<String>),
    Re(Prog),
}

/// A compiled query. Callbacks get each match's start, end and captures (for regular
/// expressions, slots 2k / 2k+1 are group k) and return false to stop.
pub struct Search {
    kind: Kind,
    case: bool,
    word: bool,
    buf: String,
    vm: Vm,
}

type OnMatch<'a> = &'a mut dyn FnMut(P, P, &[Option<P>]) -> bool;

/// The matches of a regular expression from `p` on (in its line with `single`); false when
/// `f` stopped.
fn re_scan(vm: &mut Vm, prog: &Prog, word: bool, lines: &[String], mut p: P, single: bool, f: OnMatch) -> bool {
    loop {
        if !vm.exec(prog, lines, p, single) {
            return true;
        }
        let (s, e) = (vm.found[0].unwrap(), vm.found[1].unwrap());
        // not a whole word: the search goes on from the next char (an empty match also moves on)
        let skip = word && !word_ok(lines, s, e);
        if !skip && !f(s, e, &vm.found) {
            return false;
        }
        p = if skip || e == s {
            match char_at(lines, s, single) {
                Some((_, np)) => np,
                None => return true,
            }
        } else {
            e
        };
    }
}

impl Search {
    /// None for an empty query, Err for an invalid regular expression.
    pub fn new(q: &str, case: bool, word: bool, regex: bool) -> Result<Option<Search>, String> {
        if q.is_empty() {
            return Ok(None);
        }
        let kind = if regex {
            Kind::Re(compile(q, !case)?)
        } else {
            let q = q.replace("\r\n", "\n");
            let q: String = if case { q } else { q.chars().map(fold).collect() };
            if q.contains('\n') {
                Kind::Multi(q.split('\n').map(String::from).collect())
            } else {
                Kind::Lit { nchars: char_len(&q), needle: q }
            }
        };
        Ok(Some(Search { kind, case, word, buf: String::new(), vm: Vm::default() }))
    }

    pub fn is_regex(&self) -> bool {
        matches!(self.kind, Kind::Re(_))
    }

    /// Matches are found line by line (no match spans lines).
    fn by_line(&self) -> bool {
        match &self.kind {
            Kind::Lit { .. } => true,
            Kind::Multi(_) => false,
            Kind::Re(p) => !p.multi,
        }
    }

    /// The matches of line `l` that start at or after column `from`.
    fn scan_line(&mut self, lines: &[String], l: usize, from: usize, f: OnMatch) -> bool {
        let line = lines[l].as_str();
        match &self.kind {
            Kind::Lit { needle, nchars } => {
                let hay: &str = if self.case {
                    line
                } else {
                    self.buf.clear();
                    if line.is_ascii() {
                        self.buf.push_str(line);
                        self.buf.make_ascii_lowercase();
                    } else {
                        self.buf.extend(line.chars().map(fold));
                    }
                    &self.buf
                };
                let start = bidx(hay, from);
                let mut hc = Cols::new(hay, start, from);
                let mut lc = Cols::new(line, 0, 0);
                for (hb, _) in hay[start..].match_indices(needle.as_str()) {
                    let hb = hb + start;
                    let c = hc.col(hb);
                    let (b0, b1) = if self.case { (hb, hb + needle.len()) } else { (lc.byte(c), lc.byte(c + nchars)) };
                    let (s, e) = (P { l, c, b: b0 }, P { l, c: c + nchars, b: b1 });
                    if self.word && !word_ok(lines, s, e) {
                        continue;
                    }
                    if !f(s, e, &[]) {
                        return false;
                    }
                }
                true
            }
            Kind::Re(prog) => re_scan(&mut self.vm, prog, self.word, lines, P { l, c: from, b: bidx(line, from) }, true, f),
            Kind::Multi(_) => true,
        }
    }

    /// Every match starting at or after `from`, in order.
    pub fn scan(&mut self, lines: &[String], from: Pos, f: OnMatch) {
        if from.0 >= lines.len() {
            return;
        }
        if self.by_line() {
            for l in from.0..lines.len() {
                if !self.scan_line(lines, l, if l == from.0 { from.1 } else { 0 }, f) {
                    return;
                }
            }
            return;
        }
        match &self.kind {
            Kind::Multi(parts) => {
                let n = parts.len();
                'lines: for l in from.0..lines.len() {
                    if l + n > lines.len() {
                        return;
                    }
                    let line = &lines[l];
                    let Some(sb) = suffix_at(line, &parts[0], self.case) else { continue };
                    let sc = line[..sb].chars().count();
                    if l == from.0 && sc < from.1 {
                        continue;
                    }
                    for k in 1..n - 1 {
                        let (t, p) = (&lines[l + k], &parts[k]);
                        let same = if self.case { t == p } else { t.chars().count() == p.chars().count() && t.chars().zip(p.chars()).all(|(a, b)| fold(a) == b) };
                        if !same {
                            continue 'lines;
                        }
                    }
                    let Some(eb) = prefix_at(&lines[l + n - 1], &parts[n - 1], self.case) else { continue };
                    let s = P { l, c: sc, b: sb };
                    let e = P { l: l + n - 1, c: char_len(&parts[n - 1]), b: eb };
                    if self.word && !word_ok(lines, s, e) {
                        continue;
                    }
                    if !f(s, e, &[]) {
                        return;
                    }
                }
            }
            Kind::Re(prog) => {
                re_scan(&mut self.vm, prog, self.word, lines, at(lines, from), false, f);
            }
            Kind::Lit { .. } => {}
        }
    }

    /// The matches inside `scope` (the whole text when None), at most `limit`; the flag tells
    /// whether there were more.
    pub fn all(&mut self, lines: &[String], scope: Option<Range>, limit: usize) -> (Vec<Range>, bool) {
        let (from, end) = (scope.map_or((0, 0), |s| s.0), scope.map(|s| s.1));
        let mut v = Vec::new();
        let mut capped = false;
        self.scan(lines, from, &mut |s, e, _| {
            if let Some(end) = end {
                if s.pos() >= end && !(s == e && s.pos() == end) {
                    return false;
                }
                if e.pos() > end {
                    return true;
                }
            }
            if v.len() == limit {
                capped = true;
                return false;
            }
            v.push((s.pos(), e.pos()));
            true
        });
        (v, capped)
    }

    /// The first match that starts at or after `from` (inside `scope`).
    pub fn next(&mut self, lines: &[String], from: Pos, scope: Option<Range>) -> Option<Range> {
        let from = scope.map_or(from, |s| from.max(s.0));
        let end = scope.map(|s| s.1);
        let mut r = None;
        self.scan(lines, from, &mut |s, e, _| {
            if end.is_some_and(|end| s.pos() > end || e.pos() > end) {
                return false;
            }
            r = Some((s.pos(), e.pos()));
            false
        });
        r
    }

    /// The last match that starts before `before` (inside `scope`).
    pub fn prev(&mut self, lines: &[String], before: Pos, scope: Option<Range>) -> Option<Range> {
        let start = scope.map_or((0, 0), |s| s.0);
        let before = scope.map_or(before, |s| before.min(s.1));
        let mut r = None;
        let keep = |s: P, e: P, r: &mut Option<Range>| {
            if s.pos() >= before {
                return false;
            }
            if s.pos() >= start && scope.is_none_or(|sc| e.pos() <= sc.1) {
                *r = Some((s.pos(), e.pos()));
            }
            true
        };
        if self.by_line() {
            // line by line backwards: the previous match is usually close
            let last = before.0.min(lines.len().saturating_sub(1));
            for l in (start.0..=last).rev() {
                let from = if l == start.0 { start.1 } else { 0 };
                self.scan_line(lines, l, from, &mut |s, e, _| keep(s, e, &mut r));
                if r.is_some() {
                    break;
                }
            }
            return r;
        }
        self.scan(lines, start, &mut |s, e, _| keep(s, e, &mut r));
        r
    }
}

/// Replaces the matches of `search` in `lines` that `keep` accepts (by start and end), like
/// Replace All: returns (first changed line, end of the last match's line, new text, count),
/// the new text taking the place of everything from the start of that first line to that end.
/// None when nothing was replaced.
pub fn replace_lines(search: &mut Search, lines: &[String], rep: &str, preserve: bool, keep: &mut dyn FnMut(Pos, Pos) -> bool) -> Option<(usize, Pos, String, usize)> {
    let regex = search.is_regex();
    let mut out = String::new();
    let mut first: Option<usize> = None;
    let mut last = P::default();
    let mut count = 0usize;
    search.scan(lines, (0, 0), &mut |s, e, caps| {
        if !keep(s.pos(), e.pos()) {
            return true;
        }
        let copy_from = match first {
            None => {
                first = Some(s.l);
                P { l: s.l, c: 0, b: 0 }
            }
            Some(_) => last,
        };
        push_text(&mut out, lines, copy_from, s);
        out.push_str(&replacement(rep, regex, lines, s, e, caps, preserve));
        last = e;
        count += 1;
        true
    });
    let first = first?;
    let line_end = P { l: last.l, c: char_len(&lines[last.l]), b: lines[last.l].len() };
    push_text(&mut out, lines, last, line_end);
    Some((first, line_end.pos(), out, count))
}

/// The replacement for a match: `rep` with `$1`, `$&`, `\n`, `\u`... expanded for a regular
/// expression, adapted to the matched text's case with `preserve`.
fn replacement(rep: &str, regex: bool, lines: &[String], s: P, e: P, caps: &[Option<P>], preserve: bool) -> String {
    let out = if regex { expand(rep, lines, caps) } else { rep.to_string() };
    if preserve {
        preserve_case(&text_of(lines, s, e), &out)
    } else {
        out
    }
}

fn expand(rep: &str, lines: &[String], caps: &[Option<P>]) -> String {
    let ch: Vec<char> = rep.chars().collect();
    let groups = caps.len() / 2;
    let group = |k: usize| -> Option<String> {
        match (caps.get(2 * k).copied().flatten(), caps.get(2 * k + 1).copied().flatten()) {
            (Some(a), Some(b)) => Some(text_of(lines, a, b)),
            _ => None,
        }
    };
    let mut out = String::with_capacity(rep.len());
    let mut modifier: Option<char> = None;
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        let next = ch.get(i + 1).copied();
        let mut piece = None;
        match (c, next) {
            ('\\', Some(n)) => {
                match n {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    'u' | 'l' | 'U' | 'L' => modifier = Some(n),
                    _ => {
                        out.push('\\');
                        out.push(n);
                    }
                }
                i += 2;
            }
            ('$', Some('$')) => {
                out.push('$');
                i += 2;
            }
            ('$', Some('&')) => {
                piece = Some(group(0).unwrap_or_default());
                i += 2;
            }
            ('$', Some(d)) if d.is_ascii_digit() => {
                let d1 = d.to_digit(10).unwrap() as usize;
                let two = ch.get(i + 2).and_then(|c| c.to_digit(10)).map(|d2| d1 * 10 + d2 as usize).filter(|&k| k < groups);
                let (k, len) = match two {
                    Some(k) => (k, 3),
                    None => (d1, 2),
                };
                if k < groups {
                    piece = Some(group(k).unwrap_or_default());
                } else {
                    out.push('$');
                    out.push(d);
                }
                i += len;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
        if let Some(p) = piece {
            let p = match modifier.take() {
                Some('U') => p.to_uppercase(),
                Some('L') => p.to_lowercase(),
                Some(m) => {
                    let mut it = p.chars();
                    match it.next() {
                        Some(f) => {
                            let f: String = if m == 'u' { f.to_uppercase().collect() } else { f.to_lowercase().collect() };
                            f + it.as_str()
                        }
                        None => p,
                    }
                }
                None => p,
            };
            out.push_str(&p);
        }
    }
    out
}

/// VS Code's Preserve Case: the replacement takes the case of the matched text (all upper,
/// all lower, capitalized), part by part when both are joined with '-' or '_'.
pub fn preserve_case(matched: &str, rep: &str) -> String {
    if matched.is_empty() {
        return rep.to_string();
    }
    for sep in ['-', '_'] {
        let other = if sep == '-' { '_' } else { '-' };
        if matched.contains(sep) && rep.contains(sep) && !(matched.contains(other) && rep.contains(other)) {
            let (m, r): (Vec<&str>, Vec<&str>) = (matched.split(sep).collect(), rep.split(sep).collect());
            if m.len() == r.len() {
                return m.iter().zip(&r).map(|(m, r)| preserve_case(m, r)).collect::<Vec<_>>().join(&sep.to_string());
            }
        }
    }
    let first = matched.chars().next().unwrap();
    let cap_first = |up: bool| {
        let mut it = rep.chars();
        match it.next() {
            Some(f) => {
                let f: String = if up { f.to_uppercase().collect() } else { f.to_lowercase().collect() };
                f + it.as_str()
            }
            None => String::new(),
        }
    };
    if matched.to_uppercase() == matched {
        rep.to_uppercase()
    } else if matched.to_lowercase() == matched {
        rep.to_lowercase()
    } else if first.is_uppercase() {
        cap_first(true)
    } else if first.is_lowercase() {
        cap_first(false)
    } else {
        rep.to_string()
    }
}

/// Where position `p` moves when the text from somewhere before `old_end` up to `old_end` is
/// replaced by text ending at `new_end`.
fn shift(p: Pos, old_end: Pos, new_end: Pos) -> Pos {
    if p < old_end {
        p
    } else if p.0 == old_end.0 {
        (new_end.0, p.1 - old_end.1 + new_end.1)
    } else {
        (p.0 + new_end.0 - old_end.0, p.1)
    }
}

// ------------------------------------------------------------------ widget state

/// Find widget buttons (`Hit::Find`).
pub const F_TOGGLE: u8 = 0;
pub const F_CASE: u8 = 1;
pub const F_WORD: u8 = 2;
pub const F_REGEX: u8 = 3;
pub const F_PRESERVE: u8 = 4;
pub const F_PREV: u8 = 5;
pub const F_NEXT: u8 = 6;
pub const F_IN_SEL: u8 = 7;
pub const F_CLOSE: u8 = 8;
pub const F_REPLACE: u8 = 9;
pub const F_REPLACE_ALL: u8 = 10;
/// The find / replace input boxes (around their edit controls).
pub const F_FIND_INPUT: u8 = 11;
pub const F_REPLACE_INPUT: u8 = 12;

pub struct Find {
    pub open: bool,
    /// The replace row is shown.
    pub replace: bool,
    pub case: bool,
    pub word: bool,
    pub regex: bool,
    pub preserve: bool,
    pub query: String,
    /// Find in Selection: the document's id and the range searched.
    pub scope: Option<(u64, Range)>,
    /// Why the regular expression is invalid.
    pub error: Option<String>,
    search: Option<Search>,
    /// `search` and `error` follow the query and the options.
    compiled: bool,
    /// Matches in the active file (at most MATCH_LIMIT), in order.
    pub matches: Vec<Range>,
    /// There are more matches than `matches` holds.
    pub capped: bool,
    /// (document id, version, scope) `matches` were found for.
    key: Option<(u64, u64, Option<Range>)>,
    /// The current match of a read-only view (an editable one selects it instead).
    pub cur: Option<(u64, Range)>,
    pub find_edit: HWND,
    pub repl_edit: HWND,
    /// Where the edit controls are (x, y, width, height), None while hidden.
    placed: [Option<[i32; 4]>; 2],
    history: Vec<String>,
    /// Position in `history` while browsing it with Up / Down.
    hist_pos: Option<usize>,
}

impl Find {
    pub fn new() -> Find {
        Find {
            open: false,
            replace: false,
            case: false,
            word: false,
            regex: false,
            preserve: false,
            query: String::new(),
            scope: None,
            error: None,
            search: None,
            compiled: false,
            matches: Vec::new(),
            capped: false,
            key: None,
            cur: None,
            find_edit: null_mut(),
            repl_edit: null_mut(),
            placed: [None; 2],
            history: Vec::new(),
            hist_pos: None,
        }
    }

    fn remember(&mut self) {
        let q = self.query.clone();
        if q.is_empty() {
            return;
        }
        self.history.retain(|h| *h != q);
        self.history.push(q);
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
        self.hist_pos = None;
    }
}

/// Subclass of the find and replace boxes (single-line EDIT controls); `id` 4 is the replace box.
unsafe extern "system" fn find_edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, main: usize) -> LRESULT {
    let main = main as HWND;
    let repl = id == 4;
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wp as u16;
            if crate::altgr_char(vk) {
                return DefSubclassProc(hwnd, msg, wp, lp);
            }
            let (ctrl, shift, alt) = (crate::key_down(VK_CONTROL), crate::key_down(VK_SHIFT), crate::key_down(VK_MENU));
            if crate::with_app(main, |a| a.find_edit_key(vk, ctrl, shift, alt, repl)).unwrap_or(false) {
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

// ------------------------------------------------------------------ App: find widget

/// Widget geometry (client coordinates).
struct Geo {
    bx: RECT,
    toggle: RECT,
    find_in: RECT,
    repl_in: RECT,
    count: RECT,
    /// Left edge of the buttons after the count: prev, next, in selection, close.
    btn_x: i32,
    bw: i32,
    opt_w: i32,
}

impl App {
    /// The widget is shown: open, the active view is a file and the editor area is shown (not
    /// covered by a maximized panel).
    pub fn find_visible(&self) -> bool {
        self.find.open && matches!(self.view, MainView::File(_)) && rh(&self.l.main_body) > self.s(60)
    }

    fn find_editable(&self) -> bool {
        matches!(&self.view, MainView::File(f) if f.ed.editable)
    }

    fn find_geo(&self) -> Geo {
        let b = self.l.main_body;
        let right = b.right - self.s(14);
        let w = self.s(420).min(rw(&b) - self.s(20)).max(self.s(260));
        let replace = self.find.replace && self.find_editable();
        let h = if replace { self.s(62) } else { self.s(33) };
        let bx = rect(right - w, b.top, right, b.top + h);
        let bw = self.s(22);
        let count_w = self.s(72);
        let opt_w = self.s(22);
        let left = bx.left + self.s(22);
        let in_right = (bx.right - self.s(6) - bw * 4 - count_w).max(left + opt_w * 3 + self.s(40));
        let ih = self.s(25);
        let find_in = rect(left, bx.top + self.s(4), in_right, bx.top + self.s(4) + ih);
        let repl_in = rect(left, find_in.bottom + self.s(4), in_right, find_in.bottom + self.s(4) + ih);
        let count = rect(in_right + self.s(6), find_in.top, in_right + self.s(4) + count_w, find_in.bottom);
        Geo { bx, toggle: rect(bx.left + self.s(3), bx.top + self.s(4), bx.left + self.s(19), bx.bottom - self.s(4)), find_in, repl_in, count, btn_x: count.right, bw, opt_w }
    }

    fn find_edits(&mut self) {
        if !self.find.find_edit.is_null() {
            return;
        }
        let class = win::wz("EDIT");
        let empty = win::wz("");
        unsafe {
            let hinst = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            for (k, cue) in [(0usize, "Find"), (1, "Replace")] {
                let h = CreateWindowExW(0, class.as_ptr(), empty.as_ptr(), WS_CHILD | ES_AUTOHSCROLL as u32, 0, 0, 10, 10, self.hwnd, (102 + k) as HMENU, hinst, std::ptr::null());
                SetWindowSubclass(h, Some(find_edit_proc), 3 + k, self.hwnd as usize);
                SendMessageW(h, WM_SETFONT, self.fonts.ui as WPARAM, 1);
                let c = win::wz(cue);
                SendMessageW(h, 0x1501 /* EM_SETCUEBANNER */, 1, c.as_ptr() as isize);
                win::set_control_theme(h, self.t.dark);
                if k == 0 {
                    self.find.find_edit = h;
                } else {
                    self.find.repl_edit = h;
                }
            }
        }
    }

    fn find_focus_edit(&mut self, repl: bool) {
        let h = if repl { self.find.repl_edit } else { self.find.find_edit };
        if h.is_null() {
            return;
        }
        self.find_place();
        unsafe {
            if IsWindowVisible(h) != 0 {
                SetFocus(h);
                SendMessageW(h, 0x00B1 /* EM_SETSEL */, 0, -1);
            }
        }
    }

    /// Document id and version of the active file.
    fn find_doc(&self) -> Option<(u64, u64)> {
        match &self.view {
            MainView::File(f) => Some((f.ed.uid.0, f.ed.version)),
            _ => None,
        }
    }

    fn find_scope(&self) -> Option<Range> {
        let (uid, _) = self.find_doc()?;
        let MainView::File(f) = &self.view else { return None };
        let (u, (a, b)) = self.find.scope?;
        if u != uid {
            return None;
        }
        let clamp = |p: Pos| {
            let l = p.0.min(f.lines.len() - 1);
            (l, p.1.min(char_len(&f.lines[l])))
        };
        Some((clamp(a), clamp(b)))
    }

    fn find_compile(&mut self) {
        self.find.compiled = true;
        self.find.key = None;
        match Search::new(&self.find.query, self.find.case, self.find.word, self.find.regex) {
            Ok(s) => {
                self.find.search = s;
                self.find.error = None;
            }
            Err(e) => {
                self.find.search = None;
                self.find.error = Some(e);
            }
        }
    }

    /// Brings the matches up to date with the query, the options and the active file.
    pub fn find_update(&mut self) {
        if !self.find.compiled {
            self.find_compile();
        }
        let scope = self.find_scope();
        let Some((uid, version)) = self.find_doc() else { return };
        let key = (uid, version, scope);
        if self.find.key == Some(key) {
            return;
        }
        self.find.key = Some(key);
        let MainView::File(f) = &self.view else { return };
        let find = &mut self.find;
        match &mut find.search {
            Some(s) => (find.matches, find.capped) = s.all(&f.lines, scope, MATCH_LIMIT),
            None => {
                find.matches = Vec::new();
                find.capped = false;
            }
        }
    }

    /// The current match: the selection of an editable view, the found range of a read-only one.
    fn find_current(&self) -> Option<Range> {
        match &self.view {
            MainView::File(f) if f.ed.editable => f.sel(),
            MainView::File(f) => self.find.cur.filter(|c| c.0 == f.ed.uid.0).map(|c| c.1),
            _ => None,
        }
    }

    /// "3 of 17", "? of 17", "No results" (empty with no query).
    fn find_count_label(&self) -> String {
        if self.find.query.is_empty() || self.find.error.is_some() {
            return String::new();
        }
        if self.find.matches.is_empty() {
            return "No results".into();
        }
        let total = if self.find.capped { format!("{MATCH_LIMIT}+") } else { self.find.matches.len().to_string() };
        match self.find_current().and_then(|c| self.find.matches.binary_search(&c).ok()) {
            Some(i) => format!("{} of {total}", i + 1),
            None => format!("? of {total}"),
        }
    }

    /// Ctrl+F / Ctrl+H: opens the widget, seeded with the selection (one line) or the word at
    /// the caret, and puts the keyboard in it.
    pub fn find_open(&mut self, replace: bool) {
        let MainView::File(f) = &self.view else { return };
        let editable = f.ed.editable;
        let seed = if editable {
            match f.sel() {
                Some((a, b)) if a.0 == b.0 => f.selected_text(),
                Some(_) => None,
                None => word_at(&f.lines[f.ed.cur.0], f.ed.cur.1),
            }
        } else {
            None
        };
        if let Some(s) = seed.filter(|s| !s.is_empty()) {
            self.find.query = if self.find.regex { escape_regex(&s) } else { s };
            self.find.compiled = false;
        }
        self.find.open = true;
        if replace && editable {
            self.find.replace = true;
        }
        self.find_edits();
        win::set_text(self.find.find_edit, &self.find.query.clone());
        self.find_update();
        let to_replace = replace && editable && !self.find.query.is_empty();
        self.find_focus_edit(to_replace);
        self.invalidate();
    }

    /// Escape: closes the widget, the keyboard goes back to the editor.
    pub fn find_close(&mut self) {
        if !self.find.open {
            return;
        }
        self.find.open = false;
        self.find.scope = None;
        self.find.cur = None;
        self.find_place();
        self.focus = Focus::Main;
        unsafe {
            SetFocus(self.hwnd);
        }
        self.invalidate();
    }

    /// Typing in the find box: finds as you type from where the search started.
    pub fn find_query_changed(&mut self) {
        let t = win::get_text(self.find.find_edit);
        if t == self.find.query {
            return;
        }
        self.find.query = t;
        self.find.compiled = false;
        self.find.hist_pos = None;
        self.find_research();
    }

    /// After the query or an option changed: selects the first match from the start of the
    /// selection (the caret, the top of a read-only view), like VS Code's find as you type;
    /// a selection that still matches stays.
    fn find_research(&mut self) {
        self.find_update();
        let cur = self.find_current();
        if cur.is_some_and(|c| self.find.matches.binary_search(&c).is_ok()) {
            self.invalidate();
            return;
        }
        let start = match (&self.view, cur) {
            (_, Some(c)) => c.0,
            (MainView::File(f), None) if f.ed.editable => f.ed.cur,
            _ => self.find_top_pos(),
        };
        if let Some(r) = self.find_from(start, false, true) {
            self.find_select(r);
        }
        self.invalidate();
    }

    /// Start of the first line in view (a read-only view has no caret).
    fn find_top_pos(&self) -> Pos {
        let lh = self.line_h().max(1);
        let MainView::File(f) = &self.view else { return (0, 0) };
        let row = crate::wrap::row_at(self.wrap_map(), (self.scroll[Area::Main as usize] / lh).max(0) as usize).0;
        // rows include the conflict action rows
        let mut line = row;
        for (j, c) in f.conflicts.iter().enumerate() {
            if c.start + j <= row {
                line = line.saturating_sub(1);
            }
        }
        (line.min(f.lines.len() - 1), 0)
    }

    /// The match at or after `from` (`back`: the last one before it), wrapping around.
    /// `inclusive` accepts a match starting exactly at `from` going forward.
    fn find_from(&mut self, from: Pos, back: bool, inclusive: bool) -> Option<Range> {
        self.find_update();
        let scope = self.find_scope();
        // going forward, the current match itself is passed over (it starts at `from` when empty)
        let cur = if inclusive { None } else { self.find_current() };
        if !self.find.capped {
            let m = &self.find.matches;
            if m.is_empty() {
                return None;
            }
            return Some(if back {
                let i = m.partition_point(|r| r.0 < from);
                if i == 0 {
                    *m.last().unwrap()
                } else {
                    m[i - 1]
                }
            } else {
                let i = m.partition_point(|r| r.0 < from || r.0 == from && Some(*r) == cur);
                *m.get(i).unwrap_or(&m[0])
            });
        }
        let MainView::File(f) = &self.view else { return None };
        let s = self.find.search.as_mut()?;
        if back {
            s.prev(&f.lines, from, scope).or_else(|| s.prev(&f.lines, (usize::MAX, 0), scope))
        } else {
            let mut r = s.next(&f.lines, from, scope);
            if r.is_some() && r == cur {
                let c = r.unwrap();
                r = s.next(&f.lines, if c.1 > from { c.1 } else { next_pos(&f.lines, from) }, scope);
            }
            r.or_else(|| s.next(&f.lines, scope.map_or((0, 0), |sc| sc.0), scope))
        }
    }

    /// Selects match `r` (an editable view) or makes it current (read-only), and scrolls to it.
    pub fn find_select(&mut self, r: Range) {
        let MainView::File(f) = &mut self.view else { return };
        if f.ed.editable {
            f.move_to(r.0, false);
            f.move_to(r.1, true);
            f.ed.want_vcol = None;
        } else {
            self.find.cur = Some((f.ed.uid.0, r));
        }
        self.find_reveal(r);
        self.invalidate();
    }

    /// Scrolls a match into view, centering it when it is outside (or under the widget).
    fn find_reveal(&mut self, r: Range) {
        self.wrap_update();
        let lh = self.line_h();
        let mw = self.fonts.mono_w.max(1);
        let body = self.l.main_body;
        let hidden_top = if self.find_visible() { rh(&self.find_geo().bx) } else { 0 };
        let MainView::File(f) = &self.view else { return };
        let row = r.0 .0 + f.conflicts.iter().filter(|c| c.start <= r.0 .0).count();
        // with word wrap: the visual row of the match's start, and no sideways scrolling
        let cols = self.wrap_cols().filter(|_| self.wrap_map().is_some());
        let sub = cols.map_or(0, |c| {
            let segs = crate::wrap::segments(&f.lines[r.0 .0], Some(c));
            crate::wrap::seg_of(&segs, r.0 .1)
        });
        let row = crate::wrap::vrow(self.wrap_map(), row) + sub;
        let (x0, x1) = (vcol(&f.lines[r.0 .0], r.0 .1) as i32 * mw, if r.1 .0 == r.0 .0 { vcol(&f.lines[r.1 .0], r.1 .1) as i32 * mw } else { 0 });
        let n = f.lines.len();
        let view_h = rh(&body) - self.s(4);
        let top = row as i32 * lh;
        let s = self.scroll[Area::Main as usize];
        if top < s + hidden_top || top + lh > s + view_h {
            self.scroll[Area::Main as usize] = (top - (view_h - lh) / 2).max(0);
            self.clamp_scroll(Area::Main);
        }
        if cols.is_some() {
            self.hscroll = 0;
            return;
        }
        let view_w = (rw(&body) - self.gutter_w(n) - self.s(4) - self.s(16)).max(mw * 8);
        let x1 = x1.max(x0);
        if x0 < self.hscroll {
            self.hscroll = (x0 - mw * 4).max(0);
        } else if x1 + mw > self.hscroll + view_w {
            self.hscroll = if x1 - x0 + mw * 8 > view_w { (x0 - mw * 4).max(0) } else { x1 + mw * 4 - view_w };
        }
    }

    /// F3 / Shift+F3, Enter / Shift+Enter: the next / previous match.
    pub fn find_move(&mut self, back: bool) {
        if !matches!(self.view, MainView::File(_)) {
            return;
        }
        if !self.find.open {
            if self.find.query.is_empty() {
                self.find_open(false);
                return;
            }
            self.find.open = true;
            self.find_edits();
            win::set_text(self.find.find_edit, &self.find.query.clone());
        }
        self.find.remember();
        let cur = self.find_current();
        let from = match (&self.view, cur) {
            (_, Some(c)) => {
                if back {
                    c.0
                } else {
                    c.1
                }
            }
            (MainView::File(f), None) if f.ed.editable => f.ed.cur,
            _ => self.find_top_pos(),
        };
        // a read-only view without a current match starts at the top of the view
        let inclusive = cur.is_none();
        if let Some(r) = self.find_from(from, back, inclusive) {
            self.find_select(r);
        }
        self.invalidate();
    }

    /// Ctrl+F3 / Ctrl+Shift+F3: searches for the selection (or the word at the caret).
    pub fn find_selection(&mut self, back: bool) {
        let MainView::File(f) = &self.view else { return };
        let seed = match f.sel() {
            Some((a, b)) if a.0 == b.0 => f.selected_text(),
            Some(_) => None,
            None if f.ed.editable => word_at(&f.lines[f.ed.cur.0], f.ed.cur.1),
            None => None,
        };
        let Some(s) = seed.filter(|s| !s.is_empty()) else { return };
        self.find.query = if self.find.regex { escape_regex(&s) } else { s };
        self.find.compiled = false;
        if !self.find.find_edit.is_null() {
            win::set_text(self.find.find_edit, &self.find.query.clone());
        }
        let was_open = self.find.open;
        self.find.open = true;
        self.find_move(back);
        self.find.open = was_open;
        self.invalidate();
    }

    fn find_toggle(&mut self, what: u8) {
        match what {
            F_CASE => self.find.case = !self.find.case,
            F_WORD => self.find.word = !self.find.word,
            F_REGEX => self.find.regex = !self.find.regex,
            F_PRESERVE => {
                self.find.preserve = !self.find.preserve;
                self.invalidate();
                return;
            }
            _ => return,
        }
        self.find.compiled = false;
        self.find_research();
    }

    /// Alt+L / the button: searches only the selection (or the caret's line), or everywhere again.
    fn find_toggle_scope(&mut self) {
        if self.find_scope().is_some() {
            self.find.scope = None;
        } else {
            let MainView::File(f) = &self.view else { return };
            if !f.ed.editable {
                return;
            }
            let (mut a, mut b) = f.sel().unwrap_or(((f.ed.cur.0, 0), (f.ed.cur.0, char_len(&f.lines[f.ed.cur.0]))));
            // a selection of whole lines ends at the start of the next one
            if b.1 == 0 && b.0 > a.0 {
                b = (b.0 - 1, char_len(&f.lines[b.0 - 1]));
            }
            if a > b {
                a = b;
            }
            self.find.scope = Some((f.ed.uid.0, (a, b)));
            // the selection becomes the scope: the caret goes to its start
            if let MainView::File(f) = &mut self.view {
                f.move_to(a, false);
            }
        }
        self.find.key = None;
        self.find_update();
        self.invalidate();
    }

    fn find_toggle_replace(&mut self) {
        if !self.find_editable() {
            return;
        }
        self.find.replace = !self.find.replace;
        let had = unsafe { GetFocus() } == self.find.repl_edit;
        if self.find.replace {
            self.find_focus_edit(true);
        } else if had {
            self.find_focus_edit(false);
        }
        self.invalidate();
    }

    fn find_rep_text(&self) -> String {
        if self.find.repl_edit.is_null() {
            String::new()
        } else {
            win::get_text(self.find.repl_edit).replace("\r\n", "\n")
        }
    }

    /// Replace (Enter in the replace box): replaces the selected match and selects the next
    /// one; when the selection isn't a match, it selects the next match first.
    pub fn find_replace_one(&mut self) {
        if !self.find_editable() {
            return;
        }
        self.find_update();
        self.find.remember();
        let rep = self.find_rep_text();
        let preserve = self.find.preserve;
        let (uid, _) = self.find_doc().unwrap_or((0, 0));
        let MainView::File(f) = &mut self.view else { return };
        let Some(search) = self.find.search.as_mut() else { return };
        let Some((a, b)) = f.sel() else {
            self.find_move(false);
            return;
        };
        let regex = search.is_regex();
        let mut text = None;
        search.scan(&f.lines, a, &mut |s, e, caps| {
            if s.pos() == a && e.pos() == b {
                text = Some(replacement(&rep, regex, &f.lines, s, e, caps, preserve));
            }
            false
        });
        let Some(text) = text else {
            self.find_move(false);
            return;
        };
        let new_end = crate::editor::end_of(a, &text);
        f.edit_range(a, b, &text);
        if let Some((u, sc)) = self.find.scope.as_mut() {
            if *u == uid {
                sc.1 = shift(sc.1, b, new_end);
            }
        }
        self.after_find_edit();
        // the next match after the replaced text
        if let Some(r) = self.find_from(new_end, false, true) {
            self.find_select(r);
        }
        self.invalidate();
    }

    /// Replace All (Ctrl+Alt+Enter): one edit, so one undo step brings everything back.
    pub fn find_replace_all(&mut self) {
        if !self.find_editable() {
            return;
        }
        self.find_update();
        self.find.remember();
        let rep = self.find_rep_text();
        let preserve = self.find.preserve;
        let scope = self.find_scope();
        let (uid, _) = self.find_doc().unwrap_or((0, 0));
        let MainView::File(f) = &mut self.view else { return };
        let Some(search) = self.find.search.as_mut() else { return };
        let regex = search.is_regex();
        let (from, end) = (scope.map_or((0, 0), |s| s.0), scope.map(|s| s.1));
        let mut out = String::new();
        let mut first: Option<usize> = None;
        let mut last = P::default();
        let mut count = 0usize;
        let lines = &f.lines;
        search.scan(lines, from, &mut |s, e, caps| {
            if let Some(end) = end {
                if s.pos() > end || s.pos() == end && s != e {
                    return false;
                }
                if e.pos() > end {
                    return true;
                }
            }
            let copy_from = match first {
                None => {
                    first = Some(s.l);
                    P { l: s.l, c: 0, b: 0 }
                }
                Some(_) => last,
            };
            push_text(&mut out, lines, copy_from, s);
            out.push_str(&replacement(&rep, regex, lines, s, e, caps, preserve));
            last = e;
            count += 1;
            true
        });
        let Some(first) = first else {
            self.info("No results to replace.");
            return;
        };
        let line_end = P { l: last.l, c: char_len(&f.lines[last.l]), b: f.lines[last.l].len() };
        push_text(&mut out, &f.lines, last, line_end);
        let (cur, anchor) = (f.ed.cur, f.ed.anchor);
        let old_end = line_end.pos();
        let new_end = crate::editor::end_of((first, 0), &out);
        f.edit_range((first, 0), old_end, &out);
        // the caret stays where it was (moved along when it was after the replaced text)
        let clamp = |f: &crate::app::FileView, p: Pos| {
            let p = shift(p, old_end, new_end);
            let l = p.0.min(f.lines.len() - 1);
            (l, p.1.min(char_len(&f.lines[l])))
        };
        let (c, a) = (clamp(f, cur), clamp(f, anchor));
        f.move_to(a, false);
        f.move_to(c, true);
        if let Some((u, sc)) = self.find.scope.as_mut() {
            if *u == uid {
                sc.1 = shift(sc.1, old_end, new_end);
            }
        }
        self.after_find_edit();
        self.info(format!("Replaced {count} occurrence{}.", if count == 1 { "" } else { "s" }));
        self.invalidate();
    }

    /// A replace changed the text: the tab is kept open (like any edit) and the matches are
    /// found again.
    fn after_find_edit(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.tab_active) {
            t.preview = false;
        }
        self.find.key = None;
        self.find_update();
    }

    /// Keys in the find / replace boxes; true when handled.
    pub fn find_edit_key(&mut self, vk: u16, ctrl: bool, shift: bool, alt: bool, repl: bool) -> bool {
        match vk {
            // Enter / Shift+Enter: next / previous match; in the replace box: replace;
            // Ctrl+Alt+Enter: replace all
            0x0D => {
                if ctrl && alt {
                    self.find_replace_all();
                } else if ctrl || alt {
                } else if repl {
                    self.find_replace_one();
                } else {
                    self.find_move(shift);
                }
                true
            }
            0x1B => {
                self.find_close();
                true
            }
            0x72 if !ctrl && !alt => {
                self.find_move(shift);
                true
            }
            // Tab: between the find and replace boxes
            0x09 if !ctrl && !alt => {
                if self.find.replace && self.find_editable() {
                    self.find_focus_edit(!repl);
                }
                true
            }
            // Up / Down: earlier queries
            0x26 | 0x28 if !repl && !ctrl && !alt && !shift => {
                let n = self.find.history.len();
                if n > 0 {
                    let pos = match (self.find.hist_pos, vk) {
                        (None, 0x26) => Some(n - 1),
                        (None, _) => None,
                        (Some(p), 0x26) => Some(p.saturating_sub(1)),
                        (Some(p), _) if p + 1 < n => Some(p + 1),
                        (Some(_), _) => None,
                    };
                    if let Some(p) = pos {
                        let q = self.find.history[p].clone();
                        win::set_text(self.find.find_edit, &q);
                        unsafe {
                            SendMessageW(self.find.find_edit, 0x00B1, q.encode_utf16().count(), -1isize as LPARAM);
                        }
                        self.find_query_changed();
                        self.find.hist_pos = Some(p);
                    }
                }
                true
            }
            0x43 | 0x57 | 0x52 | 0x50 | 0x4C | 0x5A if alt && !ctrl => self.alt_key(vk),
            // Ctrl+Shift+1: replace
            0x31 if ctrl && shift && !alt => {
                self.find_replace_one();
                true
            }
            // Ctrl+F / Ctrl+H in the widget: its find / replace box (not seeded again)
            0x46 if ctrl && !shift && !alt => {
                self.find_focus_edit(false);
                true
            }
            0x48 if ctrl && !shift && !alt => {
                if self.find_editable() {
                    self.find.replace = true;
                    self.invalidate();
                    self.find_focus_edit(true);
                }
                true
            }
            0x41 if ctrl && !shift && !alt => {
                let h = if repl { self.find.repl_edit } else { self.find.find_edit };
                unsafe {
                    SendMessageW(h, 0x00B1, 0, -1);
                }
                true
            }
            // the other shortcuts work as anywhere else (Ctrl+S, Ctrl+P, Ctrl+W, F5...);
            // Ctrl+C / V / X / Z go to the box
            _ if (ctrl && !alt) || (0x70..=0x87).contains(&vk) => self.global_key(vk, ctrl, shift),
            _ => false,
        }
    }

    /// Alt shortcuts of the editor area: Alt+Z (word wrap), Alt+Shift+R (reveal the active
    /// file in the Explorer), and the find widget's options while the editor or the widget
    /// has the keyboard.
    pub fn alt_key(&mut self, vk: u16) -> bool {
        if vk == 0x52 && crate::key_down(VK_SHIFT) && !crate::key_down(VK_CONTROL) {
            self.exec(Cmd::RevealInExplorer);
            return true;
        }
        if vk == 0x5A && matches!(self.view, MainView::File(_) | MainView::Diff(_)) {
            self.toggle_word_wrap();
            return true;
        }
        self.focus == Focus::Main && self.find_alt_key(vk)
    }

    /// Alt+C / W / R / P / L while the widget is shown (also from the editor).
    pub fn find_alt_key(&mut self, vk: u16) -> bool {
        if !self.find_visible() {
            return false;
        }
        match vk {
            0x43 => self.find_toggle(F_CASE),
            0x57 => self.find_toggle(F_WORD),
            0x52 => self.find_toggle(F_REGEX),
            0x50 if self.find.replace && self.find_editable() => self.find_toggle(F_PRESERVE),
            0x4C => self.find_toggle_scope(),
            _ => return false,
        }
        true
    }

    /// Keys of the editor / file view while the widget is shown; true when handled.
    pub fn find_view_key(&mut self, vk: u16, ctrl: bool, shift: bool, alt: bool) -> bool {
        if !self.find_visible() || self.focus != Focus::Main {
            return false;
        }
        match (vk, ctrl, shift, alt) {
            (0x1B, false, false, false) => self.find_close(),
            (0x0D, true, _, true) => self.find_replace_all(),
            (0x31, true, true, false) => self.find_replace_one(),
            _ => return false,
        }
        true
    }

    pub fn find_click(&mut self, what: u8) {
        match what {
            F_TOGGLE => self.find_toggle_replace(),
            F_CASE | F_WORD | F_REGEX | F_PRESERVE => self.find_toggle(what),
            F_PREV => self.find_move(true),
            F_NEXT => self.find_move(false),
            F_IN_SEL => self.find_toggle_scope(),
            F_CLOSE => self.find_close(),
            F_REPLACE => self.find_replace_one(),
            F_REPLACE_ALL => self.find_replace_all(),
            F_FIND_INPUT => self.find_focus_edit(false),
            F_REPLACE_INPUT => self.find_focus_edit(true),
            _ => {}
        }
        self.invalidate();
    }

    pub fn find_tooltip(&self, what: u8) -> Option<String> {
        Some(
            match what {
                F_TOGGLE => "Toggle Replace",
                F_CASE => "Match Case (Alt+C)",
                F_WORD => "Match Whole Word (Alt+W)",
                F_REGEX => "Use Regular Expression (Alt+R)",
                F_PRESERVE => "Preserve Case (Alt+P)",
                F_PREV => "Previous Match (Shift+Enter)",
                F_NEXT => "Next Match (Enter)",
                F_IN_SEL => "Find in Selection (Alt+L)",
                F_CLOSE => "Close (Escape)",
                F_REPLACE => "Replace (Enter)",
                F_REPLACE_ALL => "Replace All (Ctrl+Alt+Enter)",
                _ => return None,
            }
            .into(),
        )
    }

    // -------------------------------------------------------------- painting

    /// Paints the widget over the top right of the file view.
    pub fn paint_find(&mut self, g: &Gfx) {
        if !self.find_visible() {
            return;
        }
        let geo = self.find_geo();
        let editable = self.find_editable();
        let replace = self.find.replace && editable;
        let t = &self.t;
        let bx = geo.bx;
        // shadow below, like VS Code's widget
        g.fill(&rect(bx.left + self.s(2), bx.bottom, bx.right, bx.bottom + self.s(2)), blend_colorref(t.editor, 0, if t.dark { 0.6 } else { 0.15 }));
        g.fill(&bx, t.widget_bg);
        g.fill(&rect(bx.left, bx.top, bx.left + 1, bx.bottom), t.widget_border);
        g.fill(&rect(bx.right - 1, bx.top, bx.right, bx.bottom), t.widget_border);
        g.hline(bx.left, bx.right, bx.bottom - 1, t.widget_border);
        let mut hits = vec![(bx, Hit::FindBox)];
        let hover = self.hover;
        let fg = t.fg;
        let dim = blend_colorref(t.widget_bg, t.fg, 0.4);
        let s2 = self.s(2);
        let focus = unsafe { GetFocus() };
        // a toolbar button: hover background, toggled look, disabled color
        let button = |g: &Gfx, hits: &mut Vec<(RECT, Hit)>, r: RECT, what: u8, glyph: char, on: bool, enabled: bool| {
            let hit = Hit::Find(what);
            if on {
                g.round_rect(&r, s2 + 1, blend_colorref(t.widget_bg, t.accent, 0.3), Some(t.accent));
            } else if enabled && hover == hit {
                g.round_rect(&r, s2 + 1, t.toolbar_hover, None);
            }
            g.icon(&r, glyph, self.fonts.icon_small, if enabled { fg } else { dim });
            hits.push((r, hit));
        };
        if editable {
            let r = geo.toggle;
            if hover == Hit::Find(F_TOGGLE) {
                g.round_rect(&r, s2 + 1, t.toolbar_hover, None);
            }
            g.icon(&r, if replace { '\u{E70D}' } else { '\u{E76C}' }, self.fonts.icon_small, fg);
            hits.push((r, Hit::Find(F_TOGGLE)));
        }
        // the input boxes, with their option toggles inside on the right
        let opt = |g: &Gfx, hits: &mut Vec<(RECT, Hit)>, r: RECT, what: u8, label: &str, on: bool| {
            let hit = Hit::Find(what);
            if on {
                g.round_rect(&r, s2 + 1, blend_colorref(t.input_bg, t.accent, 0.3), Some(t.accent));
            } else if hover == hit {
                g.round_rect(&r, s2 + 1, t.toolbar_hover, None);
            }
            g.text_in(&r, label, self.fonts.small, if on { t.fg_strong } else { fg }, DT_LINE | windows_sys::Win32::Graphics::Gdi::DT_CENTER);
            if what == F_WORD {
                // the whole word icon: a bracket under the letters
                let (x0, x1, y) = (r.left + self.s(5), r.right - self.s(5), r.bottom - self.s(5));
                g.hline(x0, x1, y, fg);
                g.vline(x0, y - self.s(2), y + 1, fg);
                g.vline(x1 - 1, y - self.s(2), y + 1, fg);
            }
            hits.push((r, hit));
        };
        let error = self.find.error.is_some();
        let rows: &[(RECT, usize, u8)] = if replace { &[(geo.find_in, 0, F_FIND_INPUT), (geo.repl_in, 1, F_REPLACE_INPUT)] } else { &[(geo.find_in, 0, F_FIND_INPUT)] };
        for &(r, k, what) in rows {
            let edit = if k == 0 { self.find.find_edit } else { self.find.repl_edit };
            g.fill(&r, t.input_bg);
            let border = if k == 0 && error {
                t.error
            } else if focus == edit && !edit.is_null() {
                t.focus
            } else {
                t.input_border
            };
            g.frame(&r, border);
            hits.push((r, Hit::Find(what)));
            let n = if k == 0 { 3 } else { 1 };
            let oy = r.top + (rh(&r) - self.s(20)) / 2;
            let mut ox = r.right - self.s(3) - geo.opt_w * n;
            let opts: &[(u8, &str, bool)] = if k == 0 { &[(F_CASE, "Aa", self.find.case), (F_WORD, "ab", self.find.word), (F_REGEX, ".*", self.find.regex)] } else { &[(F_PRESERVE, "AB", self.find.preserve)] };
            for &(w, label, on) in opts {
                opt(g, &mut hits, xywh(ox, oy, geo.opt_w - self.s(2), self.s(20)), w, label, on);
                ox += geo.opt_w;
            }
        }
        let label = self.find_count_label();
        let no_results = label == "No results";
        g.text_in(&geo.count, &label, self.fonts.small, if no_results { t.error } else { fg }, DT_LINE);
        let has = !self.find.matches.is_empty();
        let by = geo.find_in.top + (rh(&geo.find_in) - self.s(22)) / 2;
        let mut x = geo.btn_x;
        let scoped = self.find_scope().is_some();
        for (what, glyph, on, enabled) in [(F_PREV, '\u{E74A}', false, has), (F_NEXT, '\u{E74B}', false, has), (F_IN_SEL, '\u{E8B3}', scoped, editable), (F_CLOSE, '\u{E711}', false, true)] {
            button(g, &mut hits, xywh(x, by, geo.bw - self.s(2), self.s(22)), what, glyph, on, enabled);
            x += geo.bw;
        }
        if replace {
            let by = geo.repl_in.top + (rh(&geo.repl_in) - self.s(22)) / 2;
            let x = geo.repl_in.right + self.s(6);
            button(g, &mut hits, xywh(x, by, geo.bw - self.s(2), self.s(22)), F_REPLACE, '\u{E8AB}', false, has);
            button(g, &mut hits, xywh(x + geo.bw, by, geo.bw - self.s(2), self.s(22)), F_REPLACE_ALL, '\u{E8EE}', false, has);
        }
        // an invalid regular expression: the reason under the find box
        if let Some(e) = &self.find.error {
            let w = g.text_w(e, self.fonts.small) + self.s(16);
            let er = xywh(geo.find_in.left, bx.bottom + self.s(2), w.min(rw(&self.l.main_body) - self.s(20)).max(rw(&geo.find_in)), self.s(24));
            g.fill(&er, t.widget_bg);
            g.frame(&er, t.error);
            g.text_in(&rect(er.left + self.s(8), er.top, er.right - self.s(4), er.bottom), e, self.fonts.small, fg, DT_LINE);
            hits.push((er, Hit::FindBox));
        }
        self.hits.extend(hits);
    }

    /// Where the edit controls go (x, y, width, height): the find box's and the replace box's
    /// space left of their option toggles.
    fn find_edit_rects(&self) -> [Option<[i32; 4]>; 2] {
        if !self.find_visible() {
            return [None; 2];
        }
        let geo = self.find_geo();
        let eh = self.s(18);
        let one = |r: RECT, n: i32| {
            let x = r.left + self.s(5);
            Some([x, r.top + (rh(&r) - eh) / 2, r.right - self.s(5) - geo.opt_w * n - x, eh])
        };
        [one(geo.find_in, 3), if self.find.replace && self.find_editable() { one(geo.repl_in, 1) } else { None }]
    }

    /// Moves / shows / hides the edit controls to follow the widget.
    pub fn find_place(&mut self) {
        if self.find.find_edit.is_null() {
            return;
        }
        let want = self.find_edit_rects();
        for k in 0..2 {
            let h = if k == 0 { self.find.find_edit } else { self.find.repl_edit };
            let want = want[k];
            if want == self.find.placed[k] {
                continue;
            }
            unsafe {
                match want {
                    Some([x, y, w, hh]) => {
                        MoveWindow(h, x, y, w.max(10), hh, 1);
                        if self.find.placed[k].is_none() {
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
            self.find.placed[k] = want;
        }
    }

    /// Match highlights of one line of a file view: the find-in-selection range, the matches,
    /// and (`current`) the current match.
    pub fn paint_find_line(&self, g: &Gfx, li: usize, line: &str, y: i32, x0: i32, clip: &RECT, current: bool) {
        if !self.find_visible() {
            return;
        }
        let mw = self.fonts.mono_w.max(1);
        let lh = self.line_h();
        // a column is never left of its char column (tabs only widen): matches starting past
        // the view are skipped, and the visual columns are worked out in one pass over the line
        let max_col = ((clip.right - x0) / mw).max(0) as usize + 1;
        let len = char_len(line);
        let (mut col, mut vis, mut chars) = (0usize, 0usize, line.chars());
        let mut vcol_at = |target: usize| -> usize {
            while col < target {
                let Some(ch) = chars.next() else { break };
                vis += if ch == '\t' { crate::editor::TAB - vis % crate::editor::TAB } else { 1 };
                col += 1;
            }
            vis + (target - col)
        };
        let mut fill = |g: &Gfx, a: Pos, b: Pos, color: u32| {
            if li < a.0 || li > b.0 {
                return;
            }
            let c0 = if li == a.0 { vcol_at(a.1) } else { 0 } as i32;
            // the line break of a line the range continues past shows as one cell
            let c1 = if li == b.0 { vcol_at(b.1) } else { vcol_at(len) + 1 } as i32;
            let r = rect((x0 + c0 * mw).max(clip.left), y, (x0 + c1 * mw).min(clip.right), y + lh);
            if r.right > r.left {
                g.fill(&r, color);
            }
        };
        let cur = self.find_current();
        if !current {
            if let Some((a, b)) = self.find_scope() {
                fill(g, a, b, self.t.find_range);
            }
            self.paint_find_matches(g, li, line, y, x0, clip, cur, max_col);
        } else if let Some((a, b)) = cur.filter(|c| self.find.matches.binary_search(c).is_ok()) {
            fill(g, a, b, self.t.find_current);
        }
    }

    fn paint_find_matches(&self, g: &Gfx, li: usize, line: &str, y: i32, x0: i32, clip: &RECT, cur: Option<Range>, max_col: usize) {
        let mw = self.fonts.mono_w.max(1);
        let lh = self.line_h();
        let (mut col, mut vis, mut chars) = (0usize, 0usize, line.chars());
        let mut vcol_at = |target: usize| -> usize {
            while col < target {
                let Some(ch) = chars.next() else { break };
                vis += if ch == '\t' { crate::editor::TAB - vis % crate::editor::TAB } else { 1 };
                col += 1;
            }
            vis + (target - col)
        };
        let len = char_len(line);
        let m = &self.find.matches;
        let mut i = m.partition_point(|r| r.1 < (li, 0));
        while i < m.len() && m[i].0 .0 <= li {
            let (a, b) = m[i];
            i += 1;
            if a.0 == li && a.1 > max_col {
                break;
            }
            if Some((a, b)) == cur {
                continue;
            }
            let c0 = if a.0 == li { vcol_at(a.1) } else { 0 } as i32;
            let c1 = if b.0 == li { vcol_at(b.1) } else { vcol_at(len) + 1 } as i32;
            let r = rect((x0 + c0 * mw).max(clip.left), y, (x0 + c1 * mw).min(clip.right), y + lh);
            if r.right > r.left {
                g.fill(&r, self.t.find_match);
            }
        }
    }

    /// Marks of the matches on the editor's scroll bar.
    pub fn paint_find_marks(&self, g: &Gfx, track: RECT, content: i32) {
        if !self.find_visible() || self.find.matches.is_empty() || content <= 0 {
            return;
        }
        let lh = self.line_h();
        let view = rh(&track);
        let h = self.s(2).max(2);
        let mut last = i32::MIN;
        let x0 = track.right - self.s(6);
        let wm = self.wrap_map();
        let MainView::File(f) = &self.view else { return };
        let mut c = 0;
        for m in &self.find.matches {
            // the row of the match's line (after the conflict action rows before it)
            while c < f.conflicts.len() && f.conflicts[c].start <= m.0 .0 {
                c += 1;
            }
            let v = crate::wrap::vrow(wm, m.0 .0 + c);
            let y = track.top + ((v as i64 * lh as i64 + lh as i64 / 2) * view as i64 / content as i64) as i32;
            if y == last {
                continue;
            }
            last = y;
            g.fill(&rect(x0, y - h / 2, track.right, y - h / 2 + h), self.t.find_mark);
        }
    }
}

/// The word at char column `col` of `line` (or just before it).
pub fn word_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let at = if col < chars.len() && is_word(chars[col]) {
        col
    } else if col > 0 && col <= chars.len() && is_word(chars[col - 1]) {
        col - 1
    } else {
        return None;
    };
    let (mut s, mut e) = (at, at + 1);
    while s > 0 && is_word(chars[s - 1]) {
        s -= 1;
    }
    while e < chars.len() && is_word(chars[e]) {
        e += 1;
    }
    Some(chars[s..e].iter().collect())
}

/// The position one char after `p` (the next line's start at a line end).
fn next_pos(lines: &[String], p: Pos) -> Pos {
    if p.1 < char_len(&lines[p.0]) {
        (p.0, p.1 + 1)
    } else if p.0 + 1 < lines.len() {
        (p.0 + 1, 0)
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(t: &str) -> Vec<String> {
        t.split('\n').map(String::from).collect()
    }

    fn all(q: &str, text: &str, case: bool, word: bool, regex: bool) -> Vec<Range> {
        let l = lines(text);
        Search::new(q, case, word, regex).unwrap().map(|mut s| s.all(&l, None, MATCH_LIMIT).0).unwrap_or_default()
    }

    fn re(q: &str, text: &str) -> Vec<String> {
        let l = lines(text);
        let mut s = Search::new(q, true, false, true).unwrap().unwrap();
        s.all(&l, None, MATCH_LIMIT).0.iter().map(|r| crate::editor::text_range(&l, r.0, r.1)).collect()
    }

    #[test]
    fn plain_text_case_and_whole_word() {
        assert_eq!(all("ab", "ab Ab aB\nxab", false, false, false), [((0, 0), (0, 2)), ((0, 3), (0, 5)), ((0, 6), (0, 8)), ((1, 1), (1, 3))]);
        assert_eq!(all("ab", "ab Ab aB\nxab", true, false, false), [((0, 0), (0, 2)), ((1, 1), (1, 3))]);
        assert_eq!(all("ab", "ab Ab aB\nxab", false, true, false).len(), 3);
        // a separator at the end of the query is its own boundary, like VS Code
        assert_eq!(all("a.", "xa.y a.", true, true, false), [((0, 5), (0, 7))]);
        // columns are chars, not bytes
        assert_eq!(all("é", "café É", false, false, false), [((0, 3), (0, 4)), ((0, 5), (0, 6))]);
        assert_eq!(all("x", "ÉÉx", false, false, false), [((0, 2), (0, 3))]);
        assert!(all("", "abc", false, false, false).is_empty());
    }

    #[test]
    fn multi_line_plain_text() {
        let text = "one\ntwo\nthree\ntwo\nthr";
        assert_eq!(all("e\ntw", text, true, false, false), [((0, 2), (1, 2)), ((2, 4), (3, 2))]);
        assert!(all("o\ntw", text, true, false, false).is_empty());
        assert_eq!(all("TWO\nTHR", text, false, false, false), [((1, 0), (2, 3)), ((3, 0), (4, 3))]);
        assert_eq!(all("e\ntwo\nt", text, true, false, false), [((0, 2), (2, 1)), ((2, 4), (4, 1))]);
        assert_eq!(all("\n", "a\nb\nc", true, false, false).len(), 2);
    }

    #[test]
    fn regex_basics() {
        assert_eq!(re(r"a+", "caaab a"), ["aaa", "a"]);
        assert_eq!(re(r"\d{2,3}", "1 22 4444 55555"), ["22", "444", "555", "55"]);
        assert_eq!(re(r"colou?r", "color colour colouur"), ["color", "colour"]);
        assert_eq!(re(r"\bis\b", "this is island is"), ["is", "is"]);
        assert_eq!(re(r"^\w+", "foo bar\n  baz\nqux"), ["foo", "qux"]);
        assert_eq!(re(r"\w+$", "foo bar\nbaz "), ["bar"]);
        assert_eq!(re(r"[a-c]+|x", "abcdxcab"), ["abc", "x", "cab"]);
        assert_eq!(re(r"[^a-c\s]+", "abc def\tgh"), ["def", "gh"]);
        assert_eq!(re(r"a.*?b", "aXbYb"), ["aXb"]);
        assert_eq!(re(r"a.*b", "aXbYb"), ["aXbYb"]);
        assert_eq!(re(r"(cat|category)", "category"), ["cat"]);
        assert_eq!(re(r"\.", "a.b"), ["."]);
        assert_eq!(re(r"\u0041|\x42", "AB"), ["A", "B"]);
        // empty matches advance
        assert_eq!(re(r"x*", "ab").len(), 3);
        // ignoring case
        let l = lines("Hello HELLO hello");
        let mut s = Search::new("h[aeiou]llo", false, false, true).unwrap().unwrap();
        assert_eq!(s.all(&l, None, 99).0.len(), 3);
    }

    #[test]
    fn regex_lines() {
        // `.` and classes stay in a line unless the pattern has \n
        assert_eq!(re(r"a[^x]b", "a\nb"), Vec::<String>::new());
        assert_eq!(re(r"a\nb", "a\nb\na\nc"), ["a\nb"]);
        assert_eq!(re(r"\r\n", "a\nb"), ["\n"]);
        // \s alone doesn't make a pattern multi-line (VS Code's rule), \W does
        assert!(re(r"b\s+c", "ab\n  c").is_empty());
        assert_eq!(re(r"b\W+c", "ab\n  c"), ["b\n  c"]);
    }

    #[test]
    fn regex_errors() {
        for bad in ["(a", "a)", "*a", "a**", "[a", "(?=a)", r"(a)\1", "a{3,1}", "\\"] {
            assert!(Search::new(bad, true, false, true).is_err(), "{bad}");
        }
        // a brace that isn't a quantifier is a char, like JavaScript
        assert_eq!(re(r"a{x", "a{x"), ["a{x"]);
        // huge repetitions are refused instead of exhausting memory
        assert!(Search::new("(a{1000}){1000}", true, false, true).is_err());
    }

    #[test]
    fn pathological_patterns_stay_linear() {
        let text = "a".repeat(5000);
        let l = lines(&text);
        let mut s = Search::new("(a*)*b", true, false, true).unwrap().unwrap();
        let t = std::time::Instant::now();
        assert!(s.all(&l, None, 10).0.is_empty());
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn replacement_expansion() {
        let l = lines("John Smith");
        let mut s = Search::new(r"(\w+) (\w+)", true, false, true).unwrap().unwrap();
        let mut out = Vec::new();
        s.scan(&l, (0, 0), &mut |a, b, caps| {
            out.push(replacement(r"$2, $1 ($&) $$ $3 \U$1 \l$2\n", true, &l, a, b, caps, false));
            true
        });
        assert_eq!(out, ["Smith, John (John Smith) $ $3 JOHN smith\n"]);
    }

    #[test]
    fn preserve_case_rules() {
        assert_eq!(preserve_case("FOO", "bar"), "BAR");
        assert_eq!(preserve_case("foo", "Bar"), "bar");
        assert_eq!(preserve_case("Foo", "bar"), "Bar");
        assert_eq!(preserve_case("fOO", "Bar"), "bar");
        assert_eq!(preserve_case("Foo-Bar", "baz-qux"), "Baz-Qux");
        assert_eq!(preserve_case("foo_BAR", "baz_qux"), "baz_QUX");
    }

    #[test]
    fn scope_next_and_prev() {
        let l = lines("a a a\na a");
        let mut s = Search::new("a", true, false, false).unwrap().unwrap();
        let scope = Some(((0, 2), (1, 1)));
        assert_eq!(s.all(&l, scope, 99).0, [((0, 2), (0, 3)), ((0, 4), (0, 5)), ((1, 0), (1, 1))]);
        assert_eq!(s.next(&l, (0, 3), None), Some(((0, 4), (0, 5))));
        assert_eq!(s.next(&l, (1, 3), None), None);
        assert_eq!(s.prev(&l, (1, 0), None), Some(((0, 4), (0, 5))));
        assert_eq!(s.prev(&l, (0, 0), None), None);
        assert_eq!(s.prev(&l, (usize::MAX, 0), None), Some(((1, 2), (1, 3))));
        let (v, capped) = s.all(&l, None, 3);
        assert!(capped && v.len() == 3);
    }

    #[test]
    fn replace_all_in_editor() {
        let mut f = crate::editor::tests::view("foo bar foo\nfoo");
        let mut s = Search::new("foo", true, false, false).unwrap().unwrap();
        let (m, _) = s.all(&f.lines, None, 99);
        assert_eq!(m.len(), 3);
        // what replace_all builds, applied as one edit
        let mut out = String::new();
        let mut last = P::default();
        let mut first = None;
        let lines = f.lines.clone();
        s.scan(&lines, (0, 0), &mut |a, b, caps| {
            let from = if first.is_none() {
                first = Some(a.l);
                P { l: a.l, c: 0, b: 0 }
            } else {
                last
            };
            push_text(&mut out, &lines, from, a);
            out.push_str(&replacement("x", false, &lines, a, b, caps, false));
            last = b;
            true
        });
        let end = P { l: last.l, c: char_len(&lines[last.l]), b: lines[last.l].len() };
        push_text(&mut out, &lines, last, end);
        f.edit_range((0, 0), end.pos(), &out);
        assert_eq!(f.lines.join("\n"), "x bar x\nx");
        f.undo();
        assert_eq!(f.lines.join("\n"), "foo bar foo\nfoo");
    }

    #[test]
    fn word_at_caret() {
        assert_eq!(word_at("foo.bar", 2).as_deref(), Some("foo"));
        assert_eq!(word_at("foo.bar", 3).as_deref(), Some("foo"));
        assert_eq!(word_at("foo.bar", 4).as_deref(), Some("bar"));
        assert_eq!(word_at("a  b", 2), None);
        assert_eq!(escape_regex("a.b*"), r"a\.b\*");
        assert_eq!(shift((0, 9), (0, 5), (0, 2)), (0, 6));
        assert_eq!(shift((2, 1), (0, 5), (1, 0)), (3, 1));
    }
}
