//! Vim motions and their resolution to a target position.

use regex::Regex;
use ropey::Rope;

use crate::buffer::Buffer;
use crate::search::{SearchError, SearchQuery, find_next};
use crate::text_nav;

/// A vim cursor motion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Motion {
    /// `h`: one char left, stops at the line start.
    Left,
    /// `l`: one char right, stops at the line end.
    Right,
    /// `k`: one line up (linewise).
    Up,
    /// `j`: one line down (linewise).
    Down,
    /// `w` / `W`: start of the next word.
    WordStart {
        /// WORD (whitespace separated) instead of word.
        big: bool,
    },
    /// `b` / `B`: start of the previous word.
    WordBack {
        /// WORD (whitespace separated) instead of word.
        big: bool,
    },
    /// `e` / `E`: end of the next word (inclusive).
    WordEnd {
        /// WORD (whitespace separated) instead of word.
        big: bool,
    },
    /// `Enter` (vim `+`): first non-blank `count` lines down (linewise).
    NextLineFirstNonBlank,
    /// `0`: first char of the line.
    LineStart,
    /// `^`: first non-blank char of the line.
    FirstNonBlank,
    /// `$`: end of the line (count moves down count-1 lines).
    LineEnd,
    /// `gg`: first line (linewise).
    DocStart,
    /// `{n}G` / `{n}gg`: line `n`, 1-based (linewise).
    GotoLine(usize),
    /// `G`: last line (linewise).
    DocEnd,
    /// `f F t T`: find a char in the current line.
    FindChar {
        /// Char to look for.
        c: char,
        /// Search to the right.
        forward: bool,
        /// Stop just before (or after, backwards) the char.
        till: bool,
    },
    /// `;` / `,`: repeat the last find, `reverse` for `,`.
    RepeatFind {
        /// Search in the opposite direction.
        reverse: bool,
    },
    /// `%`: matching bracket.
    MatchBracket,
    /// `}`: next empty line.
    ParagraphNext,
    /// `{`: previous empty line.
    ParagraphPrev,
    /// `'x`: first non-blank of the line of mark `x` (linewise).
    MarkLine(char),
    /// `` `x ``: exact position of mark `x`.
    MarkExact(char),
    /// `n` / `N`: next match of the last search, `reverse` for `N`.
    SearchNext {
        /// Search in the opposite direction.
        reverse: bool,
    },
}

/// The outcome of resolving a motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MotionResult {
    /// Target char index.
    pub pos: usize,
    /// Operators act on whole lines.
    pub linewise: bool,
    /// For charwise operators the target char is included.
    pub inclusive: bool,
}

/// The last `f F t T` search, repeated by `;` and `,`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastFind {
    /// Char searched for.
    pub c: char,
    /// Direction of the original search.
    pub forward: bool,
    /// Whether it was a `t`/`T` search.
    pub till: bool,
}

/// The last `/ ? * #` search, repeated by `n` and `N`.
#[derive(Clone, Debug)]
pub struct LastSearch {
    /// The query (always regex, case sensitive).
    pub query: SearchQuery,
    /// `query` compiled once, so `n` and `N` do not recompile it on every press.
    pub regex: Regex,
    /// Direction of the original search.
    pub forward: bool,
}

impl LastSearch {
    /// Compiles `query` and remembers it with its direction.
    pub fn new(query: SearchQuery, forward: bool) -> Result<LastSearch, SearchError> {
        let regex = query.compile()?;
        Ok(LastSearch {
            query,
            regex,
            forward,
        })
    }
}

/// State that motions depend on besides the buffer and the cursor.
#[derive(Clone, Debug, Default)]
pub struct MotionContext {
    /// Last char find, for `;` and `,`.
    pub last_find: Option<LastFind>,
    /// Last search, for `n` and `N`.
    pub last_search: Option<LastSearch>,
    /// Column kept across vertical moves (`usize::MAX` after `$`).
    pub preferred_col: Option<usize>,
}

/// Resolves motion `m` repeated `count` times from `pos`. `None` means the motion fails.
pub fn resolve(
    m: &Motion,
    count: usize,
    buf: &Buffer,
    pos: usize,
    state: &MotionContext,
) -> Option<MotionResult> {
    let rope = buf.rope();
    let count = count.max(1);
    let pos = pos.min(rope.len_chars());
    let exclusive = |pos| Some(charwise(pos, false));
    match m {
        // Within the line.
        Motion::Left => left(rope, pos, count),
        Motion::Right => right(rope, pos, count),
        Motion::Up | Motion::Down => vertical(rope, pos, count, *m == Motion::Down, state),
        Motion::WordStart { big } => {
            exclusive((0..count).fold(pos, |p, _| next_word_start(rope, p, *big)))
        }
        Motion::WordBack { big } => {
            exclusive((0..count).fold(pos, |p, _| prev_word_start(rope, p, *big)))
        }
        Motion::WordEnd { big } => {
            let end = (0..count).fold(pos, |p, _| text_nav::word_end(rope, p, *big));
            Some(charwise(end, true))
        }
        Motion::NextLineFirstNonBlank => next_line_first_non_blank(rope, pos, count),
        Motion::LineStart => exclusive(text_nav::line_start(rope, pos)),
        Motion::FirstNonBlank => exclusive(text_nav::first_non_blank(rope, pos)),
        Motion::LineEnd => exclusive(counted_line_end(rope, pos, count)),
        Motion::DocStart => Some(line_target(rope, 0)),
        Motion::GotoLine(n) => Some(line_target(rope, n.saturating_sub(1))),
        Motion::DocEnd => Some(line_target(rope, last_line(rope))),
        Motion::FindChar { c, forward, till } => {
            find_char(rope, pos, *c, *forward, *till, count, false)
        }
        Motion::RepeatFind { reverse } => repeat_find(rope, pos, state.last_find?, *reverse, count),
        Motion::MatchBracket => match_bracket(rope, pos),
        Motion::ParagraphNext => exclusive(paragraph_next(rope, pos, count)),
        Motion::ParagraphPrev => exclusive(paragraph_prev(rope, pos, count)),
        Motion::MarkLine(c) => {
            let mark = buf.mark(*c)?.min(rope.len_chars());
            Some(line_target(rope, rope.char_to_line(mark)))
        }
        Motion::MarkExact(c) => exclusive(buf.mark(*c)?.min(rope.len_chars())),
        Motion::SearchNext { reverse } => {
            search_next(rope, pos, state.last_search.as_ref()?, *reverse, count)
        }
    }
}

/// `h`: `count` chars left, stopping at the line start; fails at the line start.
fn left(rope: &Rope, pos: usize, count: usize) -> Option<MotionResult> {
    let start = text_nav::line_start(rope, pos);
    (pos != start).then(|| charwise(pos.saturating_sub(count).max(start), false))
}

/// `l`: `count` chars right, stopping at the line end; fails at the line end.
fn right(rope: &Rope, pos: usize, count: usize) -> Option<MotionResult> {
    let end = text_nav::line_end(rope, pos);
    (pos < end).then(|| charwise((pos + count).min(end), false))
}

/// `+` / Enter: first non-blank `count` lines down (linewise); fails on the last line.
fn next_line_first_non_blank(rope: &Rope, pos: usize, count: usize) -> Option<MotionResult> {
    let line = rope.char_to_line(pos);
    let target = (line + count).min(last_line(rope));
    (target > line).then(|| line_target(rope, target))
}

/// `$`: the end of the line `count - 1` lines down (clamped to the last line).
fn counted_line_end(rope: &Rope, pos: usize, count: usize) -> usize {
    let here = rope.char_to_line(pos);
    let line = (here + count - 1).min(last_line(rope)).max(here);
    line_end_of(rope, line)
}

/// `;` / `,`: repeats the char find `last`, in the opposite direction when `reverse`.
fn repeat_find(
    rope: &Rope,
    pos: usize,
    last: LastFind,
    reverse: bool,
    count: usize,
) -> Option<MotionResult> {
    let forward = last.forward != reverse;
    find_char(rope, pos, last.c, forward, last.till, count, true)
}

/// `n` / `N`: the `count`-th match of the last search, in the opposite direction when
/// `reverse`; fails when the pattern does not match.
fn search_next(
    rope: &Rope,
    pos: usize,
    last: &LastSearch,
    reverse: bool,
    count: usize,
) -> Option<MotionResult> {
    let forward = last.forward != reverse;
    let mut p = pos;
    for _ in 0..count {
        p = find_next(rope, &last.regex, p, forward)?.start;
    }
    Some(charwise(p, false))
}

/// Builds a charwise result.
fn charwise(pos: usize, inclusive: bool) -> MotionResult {
    MotionResult {
        pos,
        linewise: false,
        inclusive,
    }
}

/// Builds a linewise result on the first non-blank of `line` (clamped to the last line).
fn line_target(rope: &Rope, line: usize) -> MotionResult {
    let line = line.min(last_line(rope));
    MotionResult {
        pos: text_nav::first_non_blank(rope, rope.line_to_char(line)),
        linewise: true,
        inclusive: false,
    }
}

/// Returns the index of the last line motions may target. Like vim, the empty line after a
/// final `\n` does not count (an edit may still leave the caret there).
pub fn last_line(rope: &Rope) -> usize {
    let len = rope.len_chars();
    let lines = rope.len_lines();
    if len > 0 && rope.char(len - 1) == '\n' {
        return lines.saturating_sub(2);
    }
    lines.saturating_sub(1)
}

/// Returns the index of the `\n` ending `line` (or the document length).
pub fn line_end_of(rope: &Rope, line: usize) -> usize {
    text_nav::line_end(rope, rope.line_to_char(line))
}

/// Vim `w`: like `text_nav::next_word_start` but an empty line counts as a word.
pub fn next_word_start(rope: &Rope, pos: usize, big: bool) -> usize {
    text_nav::next_word_start_with(rope, pos, big, true)
}

/// Vim `b`: like `text_nav::prev_word_start` but an empty line counts as a word.
pub fn prev_word_start(rope: &Rope, pos: usize, big: bool) -> usize {
    text_nav::prev_word_start_with(rope, pos, big, true)
}

/// Moves `count` lines up or down keeping the preferred column.
fn vertical(
    rope: &Rope,
    pos: usize,
    count: usize,
    down: bool,
    state: &MotionContext,
) -> Option<MotionResult> {
    let (line, col) = text_nav::line_col(rope, pos);
    let target = if down {
        (line + count).min(last_line(rope))
    } else {
        line.saturating_sub(count)
    };
    if target == line || (down && target < line) {
        return None;
    }
    let want = state.preferred_col.unwrap_or(col);
    let start = rope.line_to_char(target);
    let end = text_nav::line_end(rope, start);
    Some(MotionResult {
        pos: start.saturating_add(want).min(end),
        linewise: true,
        inclusive: false,
    })
}

/// Finds `c` in the current line `count` times; `repeat` skips an adjacent till target.
fn find_char(
    rope: &Rope,
    pos: usize,
    c: char,
    forward: bool,
    till: bool,
    count: usize,
    repeat: bool,
) -> Option<MotionResult> {
    let start = text_nav::line_start(rope, pos);
    let end = text_nav::line_end(rope, pos);
    let mut p = pos;
    for n in 0..count {
        // A repeated till search would find the char right next to the cursor again.
        let skip = usize::from(till && repeat && n == 0);
        p = if forward {
            (p + 1 + skip..end).find(|&i| rope.char(i) == c)?
        } else {
            (start..p.saturating_sub(skip))
                .rev()
                .find(|&i| rope.char(i) == c)?
        };
    }
    if !till {
        return Some(charwise(p, forward));
    }
    let target = if forward { p - 1 } else { p + 1 };
    Some(charwise(target, forward))
}

/// `%`: the bracket under the cursor, or the first bracket after it on the line, and its match.
fn match_bracket(rope: &Rope, pos: usize) -> Option<MotionResult> {
    let end = text_nav::line_end(rope, pos);
    let at = (pos..end).find(|&i| "()[]{}".contains(rope.char(i)))?;
    text_nav::matching_bracket(rope, at).map(|p| charwise(p, true))
}

/// `}`: the next empty line after the current paragraph, or the end of the last line.
fn paragraph_next(rope: &Rope, pos: usize, count: usize) -> usize {
    let last = last_line(rope);
    let mut line = rope.char_to_line(pos);
    for _ in 0..count {
        while line < last && text_nav::is_empty_line(rope, line) {
            line += 1;
        }
        while line < last && !text_nav::is_empty_line(rope, line) {
            line += 1;
        }
    }
    if text_nav::is_empty_line(rope, line) {
        return rope.line_to_char(line);
    }
    line_end_of(rope, line)
}

/// `{`: the previous empty line before the current paragraph, or the document start.
fn paragraph_prev(rope: &Rope, pos: usize, count: usize) -> usize {
    let mut line = rope.char_to_line(pos);
    for _ in 0..count {
        while line > 0 && text_nav::is_empty_line(rope, line) {
            line -= 1;
        }
        while line > 0 && !text_nav::is_empty_line(rope, line) {
            line -= 1;
        }
    }
    rope.line_to_char(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves `m` once with an empty context.
    fn go(text: &str, pos: usize, m: Motion) -> Option<usize> {
        let buf = Buffer::from_text(text);
        resolve(&m, 1, &buf, pos, &MotionContext::default()).map(|r| r.pos)
    }

    #[test]
    fn counted_motions_stop_at_their_limits() {
        let rope = Rope::from_str("abc\nde\nf");
        assert_eq!(left(&rope, 0, 1), None, "h fails at the line start");
        assert_eq!(left(&rope, 2, 5).map(|r| r.pos), Some(0));
        assert_eq!(right(&rope, 3, 1), None, "l fails at the line end");
        assert_eq!(right(&rope, 0, 9).map(|r| r.pos), Some(3));
        assert_eq!(counted_line_end(&rope, 0, 2), 6);
        assert_eq!(
            counted_line_end(&rope, 0, 99),
            8,
            "clamped to the last line"
        );
        assert_eq!(next_line_first_non_blank(&rope, 8, 1), None);
    }

    #[test]
    fn w_stops_on_empty_line() {
        assert_eq!(
            go("foo\n\nbar", 0, Motion::WordStart { big: false }),
            Some(4)
        );
        assert_eq!(
            go("foo\n\nbar", 4, Motion::WordStart { big: false }),
            Some(5)
        );
        assert_eq!(
            go("foo\n\nbar", 5, Motion::WordBack { big: false }),
            Some(4)
        );
        assert_eq!(
            go("foo\n\nbar", 4, Motion::WordBack { big: false }),
            Some(0)
        );
    }

    #[test]
    fn till_and_paragraphs() {
        let tf = |c| Motion::FindChar {
            c,
            forward: true,
            till: true,
        };
        assert_eq!(go("a,b,c", 0, tf(',')), Some(0));
        assert_eq!(go("a,b,c", 0, tf('c')), Some(3));
        assert_eq!(go("a\nb\n\nc\nd", 0, Motion::ParagraphNext), Some(4));
        assert_eq!(go("a\nb\n\nc\nd", 5, Motion::ParagraphNext), Some(8));
        assert_eq!(go("a\nb\n\nc\nd", 7, Motion::ParagraphPrev), Some(4));
    }

    #[test]
    fn last_line_ignores_final_newline() {
        assert_eq!(last_line(&Rope::from_str("a\nb\n")), 1);
        assert_eq!(last_line(&Rope::from_str("a\nb")), 1);
        assert_eq!(last_line(&Rope::from_str("")), 0);
        assert_eq!(go("a\nb\n", 0, Motion::DocEnd), Some(2));
    }
}
