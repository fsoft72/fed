//! Operators (`d c y > <`) applied to a char range or to whole lines.

use std::ops::Range;

use ropey::Rope;

use crate::text_nav;

use super::motion::{last_line, line_end_of};
use super::normal::Operator;
use super::{Env, OpRange, VimState, leading_blanks};

impl VimState {
    /// Applies `op` to `range`; Change leaves the editor in Insert mode.
    pub(super) fn apply_operator(&mut self, op: Operator, range: OpRange, env: &mut Env) {
        if matches!(op, Operator::Indent | Operator::Outdent) {
            let rope = env.buf.rope();
            let (first, last) = match range {
                OpRange::Lines(a, b) => (a, b),
                // The end is exclusive: a range ending on a line break stays on its line.
                OpRange::Chars(r) => (
                    rope.char_to_line(r.start),
                    rope.char_to_line(r.end.saturating_sub(1).max(r.start)),
                ),
            };
            shift_lines(env, first, last, op == Operator::Outdent);
            let start = env.buf.rope().line_to_char(first);
            env.set_caret(text_nav::first_non_blank(env.buf.rope(), start));
            return;
        }
        match range {
            OpRange::Chars(r) => self.apply_chars(op, r, env),
            OpRange::Lines(a, b) => self.apply_lines(op, a, b, env),
        }
    }

    /// Delete, change or yank a char range.
    pub(super) fn apply_chars(&mut self, op: Operator, r: Range<usize>, env: &mut Env) {
        if r.is_empty() {
            if op == Operator::Change {
                self.enter_insert(r.start, env);
            }
            return;
        }
        let text = env.buf.rope().slice(r.clone()).to_string();
        self.set_register(text, false, env);
        if op != Operator::Yank {
            env.buf.replace(r.clone(), "");
        }
        if op == Operator::Change {
            return self.enter_insert(r.start, env);
        }
        env.set_caret(r.start);
    }

    /// Delete, change or yank lines `a..=b`.
    pub(super) fn apply_lines(&mut self, op: Operator, a: usize, b: usize, env: &mut Env) {
        let rope = env.buf.rope().clone();
        let start = rope.line_to_char(a);
        let text = format!("{}\n", rope.slice(start..line_end_of(&rope, b)));
        self.set_register(text, true, env);
        match op {
            Operator::Yank => {
                if rope.char_to_line(env.pos()) > a {
                    env.set_caret(text_nav::first_non_blank(&rope, start));
                }
            }
            Operator::Change => {
                let indent = leading_blanks(&rope, start);
                let indent_len = indent.chars().count();
                env.buf.replace(start..line_end_of(&rope, b), &indent);
                self.enter_insert(start + indent_len, env);
                self.auto_indent = true;
            }
            _ => {
                env.buf.replace(linewise_delete_range(&rope, a, b), "");
                let rope = env.buf.rope();
                let line_start = rope.line_to_char(a.min(last_line(rope)));
                let target = text_nav::first_non_blank(rope, line_start);
                env.set_caret(target);
            }
        }
    }
}

/// Returns the range removed by deleting lines `a..=b`, including one line break.
fn linewise_delete_range(rope: &Rope, a: usize, b: usize) -> Range<usize> {
    let len = rope.len_chars();
    let start = rope.line_to_char(a);
    let end = line_end_of(rope, b);
    if end < len {
        return start..end + 1;
    }
    // The last line has no line break: take the one before it instead.
    start.saturating_sub(usize::from(a > 0))..len
}

/// Indents (or outdents) non-empty lines `first..=last` by one tab stop.
fn shift_lines(env: &mut Env, first: usize, last: usize, outdent: bool) {
    let width = env.settings.tab_width.max(1);
    let unit = match env.settings.insert_spaces {
        true => " ".repeat(width),
        false => "\t".to_string(),
    };
    for line in (first..=last).rev() {
        let rope = env.buf.rope();
        let start = rope.line_to_char(line);
        let end = text_nav::line_end(rope, start);
        if start == end {
            continue;
        }
        if !outdent {
            env.buf.replace(start..start, &unit);
            continue;
        }
        let n = if rope.char(start) == '\t' {
            1
        } else {
            (start..end.min(start + width))
                .take_while(|&i| rope.char(i) == ' ')
                .count()
        };
        env.buf.replace(start..start + n, "");
    }
}
