//! Vim text objects: `iw aw`, `i" a"`, `i' a'`, `i( a(`, `i{ a{`, `i[ a[`.

use std::ops::Range;

use ropey::Rope;

use crate::buffer::Buffer;
use crate::text_nav::{self, CharClass, char_class};

/// Returns the char range of text object `kind` around `pos`, `inner` for `i`, else `a`.
///
/// `kind` is one of `w ( { [ " '`; `b` and `)` mean `(`, `B` and `}` mean `{`, `]` means `[`.
pub fn text_object(kind: char, inner: bool, buf: &Buffer, pos: usize) -> Option<Range<usize>> {
    let rope = buf.rope();
    if pos >= rope.len_chars() {
        return None;
    }
    match kind {
        'w' => word_object(rope, pos, inner),
        '(' | ')' | 'b' => bracket_object(rope, pos, '(', ')', inner),
        '{' | '}' | 'B' => bracket_object(rope, pos, '{', '}', inner),
        '[' | ']' => bracket_object(rope, pos, '[', ']', inner),
        '"' | '\'' => quote_object(rope, pos, kind, inner),
        _ => None,
    }
}

/// Returns true for a space or a tab.
fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// `iw` is the run of same-class chars; `aw` adds trailing (or else leading) blanks.
fn word_object(rope: &Rope, pos: usize, inner: bool) -> Option<Range<usize>> {
    let run = text_nav::word_range_at(rope, pos);
    if run.is_empty() {
        return None;
    }
    if inner {
        return Some(run);
    }
    let len = rope.len_chars();
    if char_class(rope.char(run.start), false) == CharClass::Whitespace {
        // On blanks `aw` covers the blanks plus the following word.
        if run.end < len && rope.char(run.end) != '\n' {
            return Some(run.start..text_nav::word_range_at(rope, run.end).end);
        }
        return Some(run);
    }
    Some(with_blanks(rope, run))
}

/// Extends `range` over trailing blanks, or over leading blanks when there are none.
fn with_blanks(rope: &Rope, range: Range<usize>) -> Range<usize> {
    let len = rope.len_chars();
    let mut end = range.end;
    while end < len && is_blank(rope.char(end)) {
        end += 1;
    }
    if end > range.end {
        return range.start..end;
    }
    let mut start = range.start;
    while start > 0 && is_blank(rope.char(start - 1)) {
        start -= 1;
    }
    start..range.end
}

/// Finds the innermost `open`/`close` pair around `pos` and returns its inner or outer range.
fn bracket_object(
    rope: &Rope,
    pos: usize,
    open: char,
    close: char,
    inner: bool,
) -> Option<Range<usize>> {
    let start = enclosing_open(rope, pos, open, close)?;
    let end = text_nav::matching_bracket(rope, start)?;
    if !inner {
        return Some(start..end + 1);
    }
    let mut from = start + 1;
    let mut to = end;
    // Multi-line blocks keep the brackets on their own lines (like vim).
    if from < to && rope.char(from) == '\n' {
        from += 1;
    }
    let close_line = text_nav::line_start(rope, to);
    if close_line > from && (close_line..to).all(|i| is_blank(rope.char(i))) {
        to = close_line;
    }
    Some(from..to.max(from))
}

/// Returns the index of the unmatched `open` at or before `pos`.
fn enclosing_open(rope: &Rope, pos: usize, open: char, close: char) -> Option<usize> {
    let here = rope.char(pos);
    if here == open {
        return Some(pos);
    }
    if here == close {
        return text_nav::matching_bracket(rope, pos);
    }
    let mut depth = 0usize;
    // One rope iterator walking back, instead of a tree lookup per char.
    for (i, c) in (0..pos).rev().zip(rope.chars_at(pos).reversed()) {
        if c == close {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

/// Finds the quoted string around (or after) `pos` on its line.
fn quote_object(rope: &Rope, pos: usize, quote: char, inner: bool) -> Option<Range<usize>> {
    let start = text_nav::line_start(rope, pos);
    let end = text_nav::line_end(rope, pos);
    let quotes: Vec<usize> = (start..end)
        .filter(|&i| rope.char(i) == quote && (i == start || rope.char(i - 1) != '\\'))
        .collect();
    let pairs = quotes.as_chunks::<2>().0;
    let &[open, close] = pairs
        .iter()
        .find(|p| p[0] <= pos && pos <= p[1])
        .or_else(|| pairs.iter().find(|p| p[0] > pos))?;
    if inner {
        return Some(open + 1..close);
    }
    Some(with_blanks(rope, open..close + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the text covered by the object.
    fn obj(text: &str, pos: usize, kind: char, inner: bool) -> Option<String> {
        let buf = Buffer::from_text(text);
        text_object(kind, inner, &buf, pos).map(|r| buf.rope().slice(r).to_string())
    }

    #[test]
    fn words() {
        assert_eq!(obj("foo bar baz", 5, 'w', true).as_deref(), Some("bar"));
        assert_eq!(obj("foo bar baz", 5, 'w', false).as_deref(), Some("bar "));
        assert_eq!(obj("foo bar", 5, 'w', false).as_deref(), Some(" bar"));
        assert_eq!(obj("a\n\nb", 2, 'w', true), None);
    }

    #[test]
    fn brackets() {
        assert_eq!(obj("f(a, (b))", 3, '(', true).as_deref(), Some("a, (b)"));
        assert_eq!(obj("f(a, (b))", 6, 'b', false).as_deref(), Some("(b)"));
        assert_eq!(obj("f(a, (b))", 8, ')', true).as_deref(), Some("a, (b)"));
        assert_eq!(
            obj("x {\n    y;\n}", 8, '{', true).as_deref(),
            Some("    y;\n")
        );
        assert_eq!(obj("none", 1, '[', true), None);
    }

    #[test]
    fn quotes() {
        assert_eq!(
            obj(r#"say "hi there" ok"#, 6, '"', true).as_deref(),
            Some("hi there")
        );
        assert_eq!(
            obj(r#"say "hi" ok"#, 0, '"', false).as_deref(),
            Some("\"hi\" ")
        );
        assert_eq!(
            obj(r#"a 'x\'y' b"#, 4, '\'', true).as_deref(),
            Some(r"x\'y")
        );
        assert_eq!(obj("no quotes", 2, '"', true), None);
    }
}
