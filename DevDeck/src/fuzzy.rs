//! Fuzzy matching for Quick Open ("Go to File...", Ctrl+P), scored like fzf / VS Code: the
//! query's characters must appear in order (case-insensitive), and a match scores higher when
//! its characters are consecutive or start words (after '/', '_', '-', '.', a space, at a
//! lowercase-to-uppercase change or where digits start). Positions are char indices into the
//! text, for highlighting.

const SCORE_MATCH: i32 = 16;
const GAP_START: i32 = -3;
const GAP_EXT: i32 = -1;
/// A word start after '_', '-', '.' ...
const BONUS_BOUNDARY: i32 = 8;
/// A word start after a path separator (and the text's first character).
const BONUS_DELIM: i32 = 9;
const BONUS_WHITE: i32 = 10;
/// The separators themselves ("." in "a.rs").
const BONUS_NON_WORD: i32 = 8;
/// "B" in "aB", "1" in "a1".
const BONUS_CAMEL: i32 = 7;
/// Minimum bonus of a character that follows the previous match.
const BONUS_CONSECUTIVE: i32 = 4;
/// The query's first character counts its bonus this many times.
const FIRST_MULT: i32 = 2;
/// Larger queries / texts are scored greedily instead of optimally (the table would be big).
const MAX_PAT: usize = 64;
const MAX_TEXT: usize = 1024;

const NONE: i32 = i32::MIN / 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    White,
    NonWord,
    Delim,
    Lower,
    Upper,
    Letter,
    Number,
}

fn class(c: char) -> Class {
    if c.is_ascii_lowercase() {
        Class::Lower
    } else if c.is_ascii_uppercase() {
        Class::Upper
    } else if c.is_ascii_digit() {
        Class::Number
    } else if c == '/' || c == '\\' {
        Class::Delim
    } else if c.is_whitespace() {
        Class::White
    } else if c.is_ascii() {
        Class::NonWord
    } else if c.is_uppercase() {
        Class::Upper
    } else if c.is_lowercase() {
        Class::Lower
    } else if c.is_numeric() {
        Class::Number
    } else if c.is_alphabetic() {
        Class::Letter
    } else {
        Class::NonWord
    }
}

fn bonus(prev: Class, cur: Class) -> i32 {
    match cur {
        Class::White => BONUS_WHITE,
        Class::NonWord | Class::Delim => BONUS_NON_WORD,
        _ => match prev {
            Class::White => BONUS_WHITE,
            Class::Delim => BONUS_DELIM,
            Class::NonWord => BONUS_BOUNDARY,
            Class::Lower if cur == Class::Upper => BONUS_CAMEL,
            p if cur == Class::Number && p != Class::Number => BONUS_CAMEL,
            _ => 0,
        },
    }
}

/// Case folding that keeps one char per char, so positions in the folded text are positions
/// in the original. '\' matches '/' (a query typed with Windows separators).
pub fn fold(c: char) -> char {
    if c.is_ascii() {
        if c == '\\' {
            '/'
        } else {
            c.to_ascii_lowercase()
        }
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// The folded chars of a query word.
pub fn pattern(word: &str) -> Vec<char> {
    word.chars().map(fold).collect()
}

/// True when the chars of `pat` (folded) appear in `text` in order.
pub fn is_match(pat: &[char], text: &str) -> bool {
    let mut j = 0;
    if pat.is_empty() {
        return true;
    }
    for c in text.chars() {
        if fold(c) == pat[j] {
            j += 1;
            if j == pat.len() {
                return true;
            }
        }
    }
    false
}

/// Buffers reused by `score`, which runs for every file on every key typed in Quick Open.
#[derive(Default)]
struct Scratch {
    t: Vec<char>,
    cls: Vec<Class>,
    f: Vec<char>,
    b: Vec<i32>,
    sc: Vec<i32>,
    run_bonus: Vec<i32>,
    from: Vec<u32>,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::default());
}

/// Scores `pat` (folded, see `pattern`) against `text`; None when it doesn't match. With
/// `positions`, the char indices of the best match are added to it.
pub fn score(pat: &[char], text: &str, positions: Option<&mut Vec<usize>>) -> Option<i32> {
    if pat.is_empty() {
        return Some(0);
    }
    // the cheap test first: most texts don't match at all
    if !is_match(pat, text) {
        return None;
    }
    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        let r = score_in(&mut s, pat, text, positions);
        // a huge query or path doesn't keep its tables around
        if s.sc.capacity() > 1 << 16 {
            *s = Scratch::default();
        }
        r
    })
}

fn score_in(s: &mut Scratch, pat: &[char], text: &str, positions: Option<&mut Vec<usize>>) -> Option<i32> {
    let m = pat.len();
    let Scratch { t, cls, f, b, sc, run_bonus, from } = s;
    t.clear();
    t.extend(text.chars());
    // the window an alignment can use: from the first occurrence of the first char to the
    // last occurrence of the last char
    let lo = t.iter().position(|&c| fold(c) == pat[0])?;
    let hi = t.iter().rposition(|&c| fold(c) == pat[m - 1])?;
    cls.clear();
    cls.extend(t.iter().map(|&c| class(c)));
    let bonus_at = |j: usize| bonus(if j == 0 { Class::Delim } else { cls[j - 1] }, cls[j]);
    if m > MAX_PAT || hi + 1 - lo > MAX_TEXT {
        return Some(greedy(pat, t, lo, &bonus_at, positions));
    }
    let w = hi + 1 - lo;
    f.clear();
    f.extend(t[lo..=hi].iter().map(|&c| fold(c)));
    b.clear();
    b.extend((lo..=hi).map(bonus_at));
    // best score with pat[i] matched at window index j, the bonus of the run of consecutive
    // matches it ends, and where pat[i - 1] was matched
    sc.clear();
    sc.resize(m * w, NONE);
    run_bonus.clear();
    run_bonus.resize(m * w, 0);
    from.clear();
    from.resize(m * w, u32::MAX);
    for i in 0..m {
        // best "pat[i - 1] at k, then a gap" for k <= j - 2, and its k
        let (mut gap, mut gap_from) = (NONE, 0usize);
        for j in 0..w {
            if i > 0 && j >= 2 {
                let k = j - 2;
                let prev = sc[(i - 1) * w + k];
                if gap > NONE {
                    gap += GAP_EXT;
                }
                if prev > NONE && prev + GAP_START >= gap {
                    gap = prev + GAP_START;
                    gap_from = k;
                }
            }
            if f[j] != pat[i] {
                continue;
            }
            let at = i * w + j;
            if i == 0 {
                sc[at] = SCORE_MATCH + b[j] * FIRST_MULT;
                run_bonus[at] = b[j];
                continue;
            }
            if j == 0 {
                continue;
            }
            // right after the previous match: the run keeps its (word start) bonus
            let prev = sc[(i - 1) * w + j - 1];
            if prev > NONE {
                let rb = run_bonus[(i - 1) * w + j - 1].max(b[j]).max(BONUS_CONSECUTIVE);
                sc[at] = prev + SCORE_MATCH + rb;
                run_bonus[at] = rb;
                from[at] = (j - 1) as u32;
            }
            if gap > NONE && gap + SCORE_MATCH + b[j] > sc[at] {
                sc[at] = gap + SCORE_MATCH + b[j];
                run_bonus[at] = b[j];
                from[at] = gap_from as u32;
            }
        }
    }
    let last = (m - 1) * w;
    let (mut j, best) = (0..w).map(|j| (j, sc[last + j])).filter(|&(_, s)| s > NONE).max_by_key(|&(j, s)| (s, std::cmp::Reverse(j)))?;
    if let Some(out) = positions {
        let start = out.len();
        for i in (0..m).rev() {
            out.push(lo + j);
            if i > 0 {
                j = from[i * w + j] as usize;
            }
        }
        out[start..].reverse();
    }
    Some(best)
}

/// Leftmost match, for inputs too large for the table.
fn greedy(pat: &[char], t: &[char], lo: usize, bonus_at: &dyn Fn(usize) -> i32, positions: Option<&mut Vec<usize>>) -> i32 {
    let (mut s, mut i, mut last) = (0, 0, None::<usize>);
    let mut pos = Vec::with_capacity(pat.len());
    for (j, &c) in t.iter().enumerate().skip(lo) {
        if i < pat.len() && fold(c) == pat[i] {
            s += SCORE_MATCH + bonus_at(j) * if i == 0 { FIRST_MULT } else { 1 };
            if let Some(l) = last {
                if j > l + 1 {
                    s += GAP_START + (j - l - 2) as i32 * GAP_EXT;
                }
            }
            last = Some(j);
            pos.push(j);
            i += 1;
        }
    }
    if let Some(out) = positions {
        out.extend(pos);
    }
    s
}

/// Char indices in `text` covered by the first occurrence of each word (folded), for
/// highlighting plain "contains" matches (the Command Palette and other pickers).
pub fn substring_positions(words: &[Vec<char>], text: &str) -> Vec<usize> {
    let t: Vec<char> = text.chars().map(fold).collect();
    let mut out = Vec::new();
    for w in words {
        if w.is_empty() || w.len() > t.len() {
            continue;
        }
        if let Some(at) = (0..=t.len() - w.len()).find(|&i| t[i..i + w.len()] == w[..]) {
            out.extend(at..at + w.len());
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}
