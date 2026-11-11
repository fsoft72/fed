//! Non-vim editing logic: selection, caret movement and text edits on a `Buffer`.

use std::ops::Range;

use crate::buffer::Buffer;
use crate::text_nav;

/// A selection with an anchor and a head (the caret); both are char indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Selection {
    /// Fixed end of the selection.
    pub anchor: usize,
    /// Moving end of the selection, where the caret is drawn.
    pub head: usize,
}

impl Selection {
    /// Returns an empty selection (a caret) at `pos`.
    pub fn caret(pos: usize) -> Selection {
        Selection {
            anchor: pos,
            head: pos,
        }
    }

    /// Returns the selected char range in ascending order.
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    /// Returns true when nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// Per-tab editing state.
#[derive(Clone, Debug, Default)]
pub struct EditorState {
    /// Current selection (caret when empty).
    pub selection: Selection,
    /// Column to restore on vertical moves, `None` when the last command was not vertical.
    pub preferred_col: Option<usize>,
}

/// Indentation settings used by `Indent`, `Outdent`.
#[derive(Clone, Copy, Debug)]
pub struct EditSettings {
    /// Width of a tab stop in columns.
    pub tab_width: usize,
    /// Insert spaces instead of a tab char.
    pub insert_spaces: bool,
}

/// A user-level editing action produced by the editor widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditCommand {
    Left { select: bool },
    Right { select: bool },
    Up { select: bool },
    Down { select: bool },
    WordLeft { select: bool },
    WordRight { select: bool },
    Home { select: bool },
    End { select: bool },
    DocStart { select: bool },
    DocEnd { select: bool },
    PageUp { select: bool, lines: usize },
    PageDown { select: bool, lines: usize },
    SelectAll,
    SelectWordAt(usize),
    SelectLineAt(usize),
    SetCaret { pos: usize, select: bool },
    InsertText(String),
    Newline,
    Backspace,
    Delete,
    Indent,
    Outdent,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste(String),
}

/// Applies `cmd` to `buf` and `state`. Returns clipboard text for `Cut` and `Copy`.
pub fn apply_command(
    state: &mut EditorState,
    buf: &mut Buffer,
    cmd: EditCommand,
    settings: &EditSettings,
) -> Option<String> {
    let vertical = matches!(
        cmd,
        EditCommand::Up { .. }
            | EditCommand::Down { .. }
            | EditCommand::PageUp { .. }
            | EditCommand::PageDown { .. }
    );
    let saved_col = state.preferred_col;
    if !vertical {
        state.preferred_col = None;
    }
    let head = state.selection.head;
    let rope = buf.rope().clone();
    let len = rope.len_chars();
    match cmd {
        EditCommand::Left { select } => {
            let sel = state.selection;
            if !select && !sel.is_empty() {
                state.selection = Selection::caret(sel.range().start);
            } else {
                move_head(state, head.saturating_sub(1), select);
            }
        }
        EditCommand::Right { select } => {
            let sel = state.selection;
            if !select && !sel.is_empty() {
                state.selection = Selection::caret(sel.range().end);
            } else {
                move_head(state, (head + 1).min(len), select);
            }
        }
        EditCommand::Up { select } => move_vertical(state, buf, -1, select, saved_col),
        EditCommand::Down { select } => move_vertical(state, buf, 1, select, saved_col),
        EditCommand::PageUp { select, lines } => {
            move_vertical(state, buf, -(lines as isize), select, saved_col)
        }
        EditCommand::PageDown { select, lines } => {
            move_vertical(state, buf, lines as isize, select, saved_col)
        }
        EditCommand::WordLeft { select } => {
            move_head(state, text_nav::prev_word_start(&rope, head, false), select)
        }
        EditCommand::WordRight { select } => {
            move_head(state, text_nav::next_word_start(&rope, head, false), select)
        }
        EditCommand::Home { select } => {
            let first = text_nav::first_non_blank(&rope, head);
            let target = if head == first {
                text_nav::line_start(&rope, head)
            } else {
                first
            };
            move_head(state, target, select);
        }
        EditCommand::End { select } => move_head(state, text_nav::line_end(&rope, head), select),
        EditCommand::DocStart { select } => move_head(state, 0, select),
        EditCommand::DocEnd { select } => move_head(state, len, select),
        EditCommand::SelectAll => {
            state.selection = Selection {
                anchor: 0,
                head: len,
            }
        }
        EditCommand::SelectWordAt(pos) => {
            let r = text_nav::word_range_at(&rope, pos.min(len));
            state.selection = Selection {
                anchor: r.start,
                head: r.end,
            };
        }
        EditCommand::SelectLineAt(pos) => {
            let start = text_nav::line_start(&rope, pos);
            let end = (text_nav::line_end(&rope, pos) + 1).min(len);
            state.selection = Selection {
                anchor: start,
                head: end,
            };
        }
        EditCommand::SetCaret { pos, select } => move_head(state, pos.min(len), select),
        EditCommand::InsertText(text) | EditCommand::Paste(text) => insert_text(state, buf, &text),
        EditCommand::Newline => {
            let range = state.selection.range();
            let line_start = text_nav::line_start(&rope, range.start);
            let indent: String = rope
                .slice(line_start..range.start)
                .chars()
                .take_while(|c| matches!(c, ' ' | '\t'))
                .collect();
            insert_text(state, buf, &format!("\n{indent}"));
        }
        EditCommand::Backspace => {
            let range = delete_range(state, head.saturating_sub(1)..head);
            delete(state, buf, range);
        }
        EditCommand::Delete => {
            let range = delete_range(state, head..(head + 1).min(len));
            delete(state, buf, range);
        }
        EditCommand::Indent => indent(state, buf, settings),
        EditCommand::Outdent => outdent(state, buf, settings),
        EditCommand::Undo => {
            if let Some(pos) = buf.undo() {
                state.selection = Selection::caret(pos);
            }
        }
        EditCommand::Redo => {
            if let Some(pos) = buf.redo() {
                state.selection = Selection::caret(pos);
            }
        }
        EditCommand::Copy => return Some(clipboard_range(state, &rope).1),
        EditCommand::Cut => {
            let (range, text) = clipboard_range(state, &rope);
            let start = range.start;
            buf.replace(range, "");
            state.selection = Selection::caret(start);
            return Some(text);
        }
    }
    None
}

/// Moves the head to `pos`, extending the selection when `select` is set.
fn move_head(state: &mut EditorState, pos: usize, select: bool) {
    state.selection.head = pos;
    if !select {
        state.selection.anchor = pos;
    }
}

/// Moves the head by `delta` lines keeping the preferred column.
fn move_vertical(
    state: &mut EditorState,
    buf: &Buffer,
    delta: isize,
    select: bool,
    saved_col: Option<usize>,
) {
    let rope = buf.rope();
    let (line, col) = text_nav::line_col(rope, state.selection.head);
    let want = saved_col.unwrap_or(col);
    let last = rope.len_lines().saturating_sub(1);
    let target = (line as isize + delta).clamp(0, last as isize) as usize;
    move_head(state, text_nav::pos_at(rope, target, want), select);
    state.preferred_col = Some(want);
}

/// Replaces the selection with `text` and puts the caret after it.
fn insert_text(state: &mut EditorState, buf: &mut Buffer, text: &str) {
    let range = state.selection.range();
    let pos = range.start
        + text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .count();
    buf.replace(range, text);
    state.selection = Selection::caret(pos);
}

/// Returns the selection when non-empty, otherwise `fallback`.
fn delete_range(state: &EditorState, fallback: Range<usize>) -> Range<usize> {
    if state.selection.is_empty() {
        fallback
    } else {
        state.selection.range()
    }
}

/// Deletes `range` and puts the caret at its start.
fn delete(state: &mut EditorState, buf: &mut Buffer, range: Range<usize>) {
    let start = range.start;
    buf.replace(range, "");
    state.selection = Selection::caret(start);
}

/// Returns the range and text for Cut/Copy: the selection, or the whole current line.
fn clipboard_range(state: &EditorState, rope: &ropey::Rope) -> (Range<usize>, String) {
    let range = if state.selection.is_empty() {
        let head = state.selection.head;
        text_nav::line_start(rope, head)..(text_nav::line_end(rope, head) + 1).min(rope.len_chars())
    } else {
        state.selection.range()
    };
    let text = rope.slice(range.clone()).to_string();
    (range, text)
}

/// Returns the line start indices touched by the selection (a trailing line is skipped when
/// the selection ends at its column 0).
fn selected_line_starts(state: &EditorState, rope: &ropey::Rope) -> Vec<usize> {
    let range = state.selection.range();
    let first = rope.char_to_line(range.start);
    let mut last = rope.char_to_line(range.end);
    if last > first && rope.line_to_char(last) == range.end {
        last -= 1;
    }
    (first..=last).map(|l| rope.line_to_char(l)).collect()
}

/// Indents the selected lines, or inserts a tab stop at the caret when nothing is selected.
fn indent(state: &mut EditorState, buf: &mut Buffer, settings: &EditSettings) {
    let tab_width = settings.tab_width.max(1);
    if state.selection.is_empty() {
        let rope = buf.rope();
        let col = state.selection.head - text_nav::line_start(rope, state.selection.head);
        let unit = if settings.insert_spaces {
            " ".repeat(tab_width - col % tab_width)
        } else {
            "\t".into()
        };
        insert_text(state, buf, &unit);
        return;
    }
    let unit = if settings.insert_spaces {
        " ".repeat(tab_width)
    } else {
        "\t".into()
    };
    let unit_len = unit.chars().count();
    let rope = buf.rope().clone();
    let starts = selected_line_starts(state, &rope);
    let sel = state.selection;
    buf.begin_group();
    for &ls in starts.iter().rev() {
        if text_nav::line_end(&rope, ls) > ls {
            buf.replace(ls..ls, &unit);
        }
    }
    buf.end_group();
    let shift = |p: usize| {
        let n = starts
            .iter()
            .filter(|&&ls| ls < p && text_nav::line_end(&rope, ls) > ls)
            .count();
        p + n * unit_len
    };
    state.selection = Selection {
        anchor: shift(sel.anchor),
        head: shift(sel.head),
    };
}

/// Removes up to one tab stop of leading whitespace from the selected (or current) lines.
fn outdent(state: &mut EditorState, buf: &mut Buffer, settings: &EditSettings) {
    let tab_width = settings.tab_width.max(1);
    let rope = buf.rope().clone();
    let starts = selected_line_starts(state, &rope);
    let removable: Vec<(usize, usize)> = starts
        .iter()
        .map(|&ls| (ls, leading_removable(&rope, ls, tab_width)))
        .filter(|&(_, n)| n > 0)
        .collect();
    if removable.is_empty() {
        return;
    }
    let sel = state.selection;
    buf.begin_group();
    for &(ls, n) in removable.iter().rev() {
        buf.replace(ls..ls + n, "");
    }
    buf.end_group();
    let shift = |p: usize| {
        let removed: usize = removable
            .iter()
            .map(|&(ls, n)| p.saturating_sub(ls).min(n))
            .sum();
        p - removed
    };
    state.selection = Selection {
        anchor: shift(sel.anchor),
        head: shift(sel.head),
    };
}

/// Returns how many leading chars of the line at `ls` outdent removes.
fn leading_removable(rope: &ropey::Rope, ls: usize, tab_width: usize) -> usize {
    let end = text_nav::line_end(rope, ls);
    if ls < end && rope.char(ls) == '\t' {
        return 1;
    }
    let mut n = 0;
    while n < tab_width && ls + n < end && rope.char(ls + n) == ' ' {
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    const SET: EditSettings = EditSettings {
        tab_width: 4,
        insert_spaces: true,
    };

    fn setup(text: &str, anchor: usize, head: usize) -> (EditorState, Buffer) {
        let st = EditorState {
            selection: Selection { anchor, head },
            preferred_col: None,
        };
        (st, Buffer::from_text(text))
    }

    #[test]
    fn insert_replaces_selection() {
        let (mut st, mut b) = setup("hello world", 0, 5);
        apply_command(&mut st, &mut b, EditCommand::InsertText("bye".into()), &SET);
        assert_eq!(b.text(), "bye world");
        assert_eq!(st.selection, Selection::caret(3));
    }

    #[test]
    fn newline_keeps_indent() {
        let (mut st, mut b) = setup("  foo", 5, 5);
        apply_command(&mut st, &mut b, EditCommand::Newline, &SET);
        assert_eq!(b.text(), "  foo\n  ");
        assert_eq!(st.selection.head, 8);
    }

    #[test]
    fn backspace_at_line_start_joins() {
        let (mut st, mut b) = setup("ab\ncd", 3, 3);
        apply_command(&mut st, &mut b, EditCommand::Backspace, &SET);
        assert_eq!(b.text(), "abcd");
        assert_eq!(st.selection.head, 2);
    }

    #[test]
    fn indent_multiline_selection_spaces() {
        let (mut st, mut b) = setup("a\nb\nc", 0, 3);
        apply_command(&mut st, &mut b, EditCommand::Indent, &SET);
        assert_eq!(b.text(), "    a\n    b\nc");
        assert!(b.undo().is_some());
        assert_eq!(b.text(), "a\nb\nc");
    }

    #[test]
    fn indent_without_selection_goes_to_tab_stop() {
        let (mut st, mut b) = setup("ab", 2, 2);
        apply_command(&mut st, &mut b, EditCommand::Indent, &SET);
        assert_eq!(b.text(), "ab  ");
        assert_eq!(st.selection.head, 4);
    }

    #[test]
    fn outdent_removes_up_to_tab_width() {
        let (mut st, mut b) = setup("      x\n  y\n\tz", 0, 14);
        apply_command(&mut st, &mut b, EditCommand::Outdent, &SET);
        assert_eq!(b.text(), "  x\ny\nz");
    }

    #[test]
    fn up_down_keep_preferred_col() {
        let (mut st, mut b) = setup("abcdef\nab\nabcdef", 5, 5);
        apply_command(&mut st, &mut b, EditCommand::Down { select: false }, &SET);
        assert_eq!(st.selection.head, 9);
        apply_command(&mut st, &mut b, EditCommand::Down { select: false }, &SET);
        assert_eq!(st.selection.head, 15);
        apply_command(&mut st, &mut b, EditCommand::Up { select: false }, &SET);
        assert_eq!(st.selection.head, 9);
    }

    #[test]
    fn home_toggles_first_non_blank() {
        let (mut st, mut b) = setup("  foo", 5, 5);
        apply_command(&mut st, &mut b, EditCommand::Home { select: false }, &SET);
        assert_eq!(st.selection.head, 2);
        apply_command(&mut st, &mut b, EditCommand::Home { select: false }, &SET);
        assert_eq!(st.selection.head, 0);
        apply_command(&mut st, &mut b, EditCommand::Home { select: false }, &SET);
        assert_eq!(st.selection.head, 2);
    }

    #[test]
    fn copy_without_selection_copies_line() {
        let (mut st, mut b) = setup("one\ntwo\n", 5, 5);
        let out = apply_command(&mut st, &mut b, EditCommand::Copy, &SET);
        assert_eq!(out.as_deref(), Some("two\n"));
        assert_eq!(b.text(), "one\ntwo\n");
        let out = apply_command(&mut st, &mut b, EditCommand::Cut, &SET);
        assert_eq!(out.as_deref(), Some("two\n"));
        assert_eq!(b.text(), "one\n");
    }

    #[test]
    fn select_word_at_middle() {
        let (mut st, mut b) = setup("foo barbaz qux", 0, 0);
        apply_command(&mut st, &mut b, EditCommand::SelectWordAt(6), &SET);
        assert_eq!(st.selection.range(), 4..10);
    }

    #[test]
    fn undo_sets_caret() {
        let (mut st, mut b) = setup("ab", 2, 2);
        apply_command(&mut st, &mut b, EditCommand::InsertText("c".into()), &SET);
        apply_command(&mut st, &mut b, EditCommand::Undo, &SET);
        assert_eq!(b.text(), "ab");
        assert_eq!(st.selection, Selection::caret(2));
    }
}
