//! Character classes, word boundaries and line helpers shared by the editor widget and vim.

use std::borrow::Cow;
use std::ops::Range;

use ropey::Rope;

/// Classification of a char for word motions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharClass {
    /// Space, tab, newline and other whitespace.
    Whitespace,
    /// Alphanumeric or `_` (or any non-whitespace when `big`).
    Word,
    /// Any other punctuation.
    Punct,
}

/// Classifies `c`; with `big` every non-whitespace char is `Word` (vim WORD).
pub fn char_class(c: char, big: bool) -> CharClass {
    if c.is_whitespace() {
        return CharClass::Whitespace;
    }
    if big || c.is_alphanumeric() || c == '_' {
        return CharClass::Word;
    }
    CharClass::Punct
}

/// Class of the char at `pos`, or `None` past the end.
fn class_at(rope: &Rope, pos: usize, big: bool) -> Option<CharClass> {
    if pos >= rope.len_chars() {
        return None;
    }
    Some(char_class(rope.char(pos), big))
}

/// Returns the start of the next word after `pos` (vim `w`), or the document end.
pub fn next_word_start(rope: &Rope, pos: usize, big: bool) -> usize {
    next_word_start_with(rope, pos, big, false)
}

/// Returns the start of the word before `pos` (vim `b`), or 0.
pub fn prev_word_start(rope: &Rope, pos: usize, big: bool) -> usize {
    prev_word_start_with(rope, pos, big, false)
}

/// Returns true when line `line` has no chars besides its line break (the last line may have
/// none at all).
pub fn is_empty_line(rope: &Rope, line: usize) -> bool {
    let start = rope.line_to_char(line);
    line_end(rope, start) == start
}

/// True when the char at `i` is the line break of an empty line (see `is_empty_line`, by
/// char index for scans that walk the text).
pub fn is_empty_line_at(rope: &Rope, i: usize) -> bool {
    i < rope.len_chars() && rope.char(i) == '\n' && (i == 0 || rope.char(i - 1) == '\n')
}

/// `next_word_start`; with `empty_lines`, an empty line counts as a word (vim `w`).
pub fn next_word_start_with(rope: &Rope, pos: usize, big: bool, empty_lines: bool) -> usize {
    let len = rope.len_chars();
    let mut i = pos.min(len);
    let Some(start_class) = class_at(rope, i, big) else {
        return len;
    };
    if start_class != CharClass::Whitespace {
        while class_at(rope, i, big) == Some(start_class) {
            i += 1;
        }
    }
    while class_at(rope, i, big) == Some(CharClass::Whitespace) {
        if empty_lines && rope.char(i) == '\n' && is_empty_line_at(rope, i + 1) {
            return i + 1;
        }
        i += 1;
    }
    i
}

/// `prev_word_start`; with `empty_lines`, an empty line counts as a word (vim `b`).
pub fn prev_word_start_with(rope: &Rope, pos: usize, big: bool, empty_lines: bool) -> usize {
    let mut i = pos.min(rope.len_chars());
    while i > 0 && class_at(rope, i - 1, big) == Some(CharClass::Whitespace) {
        if empty_lines && is_empty_line_at(rope, i - 1) {
            return i - 1;
        }
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let class = class_at(rope, i - 1, big);
    while i > 0 && class_at(rope, i - 1, big) == class {
        i -= 1;
    }
    i
}

/// Returns the index of the last char of the next word end after `pos` (vim `e`, inclusive).
pub fn word_end(rope: &Rope, pos: usize, big: bool) -> usize {
    let len = rope.len_chars();
    if len == 0 {
        return 0;
    }
    let mut i = pos + 1;
    while class_at(rope, i, big) == Some(CharClass::Whitespace) {
        i += 1;
    }
    let Some(class) = class_at(rope, i, big) else {
        return len - 1;
    };
    while class_at(rope, i + 1, big) == Some(class) {
        i += 1;
    }
    i
}

/// Returns the range of the run of same-class chars around `pos` (whitespace runs stop at `\n`).
pub fn word_range_at(rope: &Rope, pos: usize) -> Range<usize> {
    let len = rope.len_chars();
    if len == 0 {
        return 0..0;
    }
    let pos = if pos >= len { len - 1 } else { pos };
    let class = char_class(rope.char(pos), false);
    let same = |i: usize| {
        let c = rope.char(i);
        c != '\n' && char_class(c, false) == class
    };
    if rope.char(pos) == '\n' {
        return pos..pos;
    }
    let mut start = pos;
    while start > 0 && same(start - 1) {
        start -= 1;
    }
    let mut end = pos + 1;
    while end < len && same(end) {
        end += 1;
    }
    start..end
}

/// Returns the char index of the start of the line containing `pos`.
pub fn line_start(rope: &Rope, pos: usize) -> usize {
    rope.line_to_char(rope.char_to_line(pos.min(rope.len_chars())))
}

/// Returns the index of the `\n` ending the line containing `pos`, or the document length.
pub fn line_end(rope: &Rope, pos: usize) -> usize {
    let line = rope.char_to_line(pos.min(rope.len_chars()));
    let start = rope.line_to_char(line);
    let mut len = rope.line(line).len_chars();
    if len > 0 && rope.char(start + len - 1) == '\n' {
        len -= 1;
    }
    start + len
}

/// Returns the first non-blank char of the line containing `pos` (line end if all blank).
pub fn first_non_blank(rope: &Rope, pos: usize) -> usize {
    let end = line_end(rope, pos);
    let mut i = line_start(rope, pos);
    while i < end && matches!(rope.char(i), ' ' | '\t') {
        i += 1;
    }
    i
}

/// Returns the zero-based `(line, column)` of `pos`, columns counted in chars.
pub fn line_col(rope: &Rope, pos: usize) -> (usize, usize) {
    let pos = pos.min(rope.len_chars());
    let line = rope.char_to_line(pos);
    (line, pos - rope.line_to_char(line))
}

/// Returns the char index of `(line, col)`; line and col are clamped to the text.
pub fn pos_at(rope: &Rope, line: usize, col: usize) -> usize {
    let line = line.min(rope.len_lines().saturating_sub(1));
    let start = rope.line_to_char(line);
    (start + col).min(line_end(rope, start))
}

/// Returns the text of line `index` without its trailing line break, borrowed from the rope
/// when the line is stored in one chunk (the common case), so drawing and searching a line
/// usually allocate nothing.
pub fn line_text(rope: &Rope, index: usize) -> Cow<'_, str> {
    let mut line = rope.line(index);
    let len = line.len_chars();
    if len > 0 && line.char(len - 1) == '\n' {
        line = line.slice(..len - 1);
    }
    match line.as_str() {
        Some(text) => Cow::Borrowed(text),
        None => Cow::Owned(line.to_string()),
    }
}

/// Columns taken by a tab starting at display column `col`: up to the next multiple of
/// `tab_width` (a width of 0 counts as 1).
pub fn tab_advance(col: usize, tab_width: usize) -> usize {
    let width = tab_width.max(1);
    width - col % width
}

/// Columns taken by `c` starting at display column `col`: tabs advance to the next tab stop,
/// other chars take one column.
pub fn char_advance(c: char, col: usize, tab_width: usize) -> usize {
    match c {
        '\t' => tab_advance(col, tab_width),
        _ => 1,
    }
}

/// Display width in columns of `chars` (a line; a line break is not counted): tabs advance
/// to the next multiple of `tab_width`, other chars take one column.
pub fn text_width(chars: impl Iterator<Item = char>, tab_width: usize) -> usize {
    chars
        .filter(|&c| c != '\n')
        .fold(0, |col, c| col + char_advance(c, col, tab_width))
}

/// Returns the display column of `pos` in its line (see `text_width`).
pub fn display_col(rope: &Rope, pos: usize, tab_width: usize) -> usize {
    let start = line_start(rope, pos);
    text_width(rope.slice(start..pos).chars(), tab_width)
}

/// Converts char column `col` of the line text `line` to a display column (see `text_width`).
pub fn display_col_in(line: &str, col: usize, tab_width: usize) -> usize {
    text_width(line.chars().take(col), tab_width)
}

/// Converts display column `dcol` to the nearest char column of the line text `line` (a
/// click inside a tab goes to the closer side).
pub fn char_col_in(line: &str, dcol: usize, tab_width: usize) -> usize {
    let mut x = 0;
    for (i, c) in line.chars().enumerate() {
        let w = char_advance(c, x, tab_width);
        if dcol < x + w {
            return if dcol - x <= w / 2 { i } else { i + 1 };
        }
        x += w;
    }
    line.chars().count()
}

/// Finds the bracket matching the one at `pos` for `()[]{}`, or `None`.
pub fn matching_bracket(rope: &Rope, pos: usize) -> Option<usize> {
    if pos >= rope.len_chars() {
        return None;
    }
    let c = rope.char(pos);
    let (open, close, forward) = match c {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let (same, other) = if forward {
        (open, close)
    } else {
        (close, open)
    };
    let mut depth = 0usize;
    let mut step = |ch: char| {
        if ch == same {
            depth += 1;
        } else if ch == other {
            depth -= 1;
            return depth == 0;
        }
        false
    };
    // Rope iterators instead of a tree lookup per char.
    if forward {
        return (pos..)
            .zip(rope.chars_at(pos))
            .find_map(|(i, ch)| step(ch).then_some(i));
    }
    (0..=pos)
        .rev()
        .zip(rope.chars_at(pos + 1).reversed())
        .find_map(|(i, ch)| step(ch).then_some(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line_by_index_and_by_char_agree() {
        let rope = Rope::from_str("a\n\nb\n\n");
        let by_line: Vec<bool> = (0..rope.len_lines())
            .map(|l| is_empty_line(&rope, l))
            .collect();
        assert_eq!(by_line, vec![false, true, false, true, true]);
        for (l, &empty) in by_line.iter().enumerate().take(rope.len_lines() - 1) {
            let at = rope.line_to_char(l);
            assert_eq!(is_empty_line_at(&rope, at), empty, "line {l}");
        }
    }

    #[test]
    fn line_text_strips_the_break_and_borrows() {
        let rope = Rope::from_str("ab\ncd\n");
        assert_eq!(line_text(&rope, 0), "ab");
        assert!(matches!(line_text(&rope, 1), Cow::Borrowed("cd")));
        assert_eq!(line_text(&rope, 2), "");
        let long = "x".repeat(5000) + "\n" + &"y".repeat(5000);
        let rope = Rope::from_str(&long);
        assert_eq!(line_text(&rope, 0).len(), 5000);
        assert_eq!(line_text(&rope, 1).len(), 5000);
    }
    use ropey::Rope;

    #[test]
    fn display_col_follows_tab_stops() {
        let rope = Rope::from_str("a\tb\n\t\tc");
        assert_eq!(display_col(&rope, 1, 4), 1);
        assert_eq!(display_col(&rope, 2, 4), 4);
        assert_eq!(display_col(&rope, 3, 4), 5);
        assert_eq!(display_col(&rope, 6, 4), 8);
        assert_eq!(display_col(&rope, 4, 0), 0);
    }

    #[test]
    fn empty_lines_count_as_words_only_on_request() {
        let rope = Rope::from_str("a\n\nb");
        assert_eq!(next_word_start_with(&rope, 0, false, false), 3);
        assert_eq!(next_word_start_with(&rope, 0, false, true), 2);
        assert_eq!(prev_word_start_with(&rope, 3, false, false), 0);
        assert_eq!(prev_word_start_with(&rope, 3, false, true), 2);
    }

    #[test]
    fn move_word_over_accented_text() {
        let r = Rope::from_str("città è bella");
        assert_eq!(next_word_start(&r, 0, false), 6);
        assert_eq!(next_word_start(&r, 6, false), 8);
    }

    #[test]
    fn word_end_vim_semantics() {
        let r = Rope::from_str("foo bar.baz");
        assert_eq!(word_end(&r, 0, false), 2);
        assert_eq!(word_end(&r, 2, false), 6);
        assert_eq!(word_end(&r, 6, false), 7);
        assert_eq!(word_end(&r, 0, true), 2);
        assert_eq!(word_end(&r, 3, true), 10);
    }

    #[test]
    fn matching_bracket_nested() {
        let r = Rope::from_str("a(b[c]{d})e");
        assert_eq!(matching_bracket(&r, 1), Some(9));
        assert_eq!(matching_bracket(&r, 9), Some(1));
        assert_eq!(matching_bracket(&r, 3), Some(5));
        assert_eq!(matching_bracket(&r, 0), None);
    }

    #[test]
    fn prev_word_and_lines() {
        let r = Rope::from_str("foo bar\n  baz");
        assert_eq!(prev_word_start(&r, 7, false), 4);
        assert_eq!(prev_word_start(&r, 4, false), 0);
        assert_eq!(line_start(&r, 10), 8);
        assert_eq!(line_end(&r, 2), 7);
        assert_eq!(line_end(&r, 10), 13);
        assert_eq!(first_non_blank(&r, 9), 10);
        assert_eq!(line_col(&r, 10), (1, 2));
        assert_eq!(pos_at(&r, 1, 99), 13);
        assert_eq!(pos_at(&r, 0, 99), 7);
    }
}
