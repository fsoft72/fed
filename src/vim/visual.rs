//! Visual and VisualLine modes: moving the selection, its operators and the `.` replay of
//! a Visual change.

use ropey::Rope;

use crate::editing::Selection;

use super::motion::{Motion, last_line};
use super::normal::{Operator, Parse, VisualCmd, parse_visual};
use super::{Env, Mode, OpRange, VimKey, VimState, clamp_normal};

impl VimState {
    /// Handles a key in Visual and VisualLine mode.
    pub(super) fn visual_key(&mut self, key: VimKey, env: &mut Env) {
        self.keys.push(key);
        let parsed = match parse_visual(&self.keys) {
            Parse::Incomplete => return,
            Parse::Invalid => return self.keys.clear(),
            Parse::Done(parsed) => parsed,
        };
        self.keys.clear();
        let pos = env.pos();
        match parsed.cmd {
            VisualCmd::Esc => self.leave_visual(env),
            VisualCmd::Move(m) => {
                let Some(r) = self.motion(&m, parsed.count, pos, env) else {
                    return;
                };
                let rope = env.buf.rope();
                let head = match m {
                    // `$` selects the line break too, like vim.
                    Motion::LineEnd => r.pos.min(rope.len_chars().saturating_sub(1)),
                    _ => clamp_normal(rope, r.pos),
                };
                env.ed.selection.head = head;
            }
            VisualCmd::Action('o') => {
                let sel = env.ed.selection;
                env.ed.selection = Selection {
                    anchor: sel.head,
                    head: sel.anchor,
                };
            }
            VisualCmd::Action(c @ ('v' | 'V')) => {
                let target = if c == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                if self.mode == target {
                    return self.leave_visual(env);
                }
                self.mode = target;
            }
            VisualCmd::Action(':') => {
                self.leave_visual(env);
                self.mode = Mode::CommandLine;
                self.cmdline = ":'<,'>".to_string();
            }
            VisualCmd::Action(c) => self.visual_operator(c, env),
        }
    }

    /// Applies the Visual-mode operator `c` (`d x c y > <`) to the selection.
    pub(super) fn visual_operator(&mut self, c: char, env: &mut Env) {
        let op = match c {
            'd' | 'x' => Operator::Delete,
            'c' => Operator::Change,
            'y' => Operator::Yank,
            '>' => Operator::Indent,
            '<' => Operator::Outdent,
            _ => return,
        };
        let rope = env.buf.rope();
        let range = match self.mode {
            Mode::VisualLine => {
                let sel = env.ed.selection.range();
                let first = rope.char_to_line(sel.start);
                let last = rope.char_to_line(sel.end).min(last_line(rope)).max(first);
                OpRange::Lines(first, last)
            }
            _ => {
                let Some(r) = self.visual_range(env.buf, env.ed) else {
                    return;
                };
                OpRange::Chars(r)
            }
        };
        let sel_start = env.ed.selection.range().start;
        let replay = visual_replay_keys(self.mode, env.buf.rope(), env.ed.selection, c);
        self.leave_visual(env);
        if op != Operator::Yank {
            self.begin_change(env.buf);
        }
        env.set_caret(match &range {
            OpRange::Chars(r) => r.start,
            OpRange::Lines(..) => sel_start,
        });
        self.apply_operator(op, range, env);
        env.ed.preferred_col = None;
        if op != Operator::Yank {
            // `.` reselects the same extent from the caret and applies the operator again;
            // a Change records the typed text after it until Esc.
            match self.mode {
                Mode::Insert => self.recording = Some(replay),
                _ => self.last_change = replay,
            }
        }
        if self.mode != Mode::Insert {
            self.end_change(env.buf);
            let p = clamp_normal(env.buf.rope(), env.pos());
            env.set_caret(p);
        }
    }

    /// Returns to Normal mode, recording the selection in marks `<` and `>` (buffer marks,
    /// so they follow later edits; `:'<,'>` reads them).
    pub(super) fn leave_visual(&mut self, env: &mut Env) {
        let sel = env.ed.selection;
        let (first, last) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
        env.buf.set_mark('<', first);
        env.buf.set_mark('>', last);
        self.mode = Mode::Normal;
        let p = clamp_normal(env.buf.rope(), sel.head);
        env.set_caret(p);
    }
}

/// Returns the keys of `count` typed in front of `key` (just `key` when `count` is 0).
fn counted(count: usize, key: char) -> Vec<VimKey> {
    if count == 0 {
        return Vec::new();
    }
    let mut keys: Vec<VimKey> = match count {
        1 => Vec::new(),
        n => n.to_string().chars().map(VimKey::Char).collect(),
    };
    keys.push(VimKey::Char(key));
    keys
}

/// Returns the keys that repeat the Visual operator `op` on a selection like `sel` (in
/// `mode`) from the caret, for `.`: the same number of lines, and for a charwise selection
/// the same end column (or the line break) on its last line, like vim.
fn visual_replay_keys(mode: Mode, rope: &Rope, sel: Selection, op: char) -> Vec<VimKey> {
    let len = rope.len_chars();
    let (start, end) = (
        sel.anchor.min(sel.head).min(len),
        sel.anchor.max(sel.head).min(len),
    );
    let (first, last) = (rope.char_to_line(start), rope.char_to_line(end));
    let mut keys = Vec::new();
    if mode == Mode::VisualLine {
        keys.push(VimKey::Char('V'));
        keys.extend(counted(last - first, 'j'));
    } else {
        keys.push(VimKey::Char('v'));
        keys.extend(counted(last - first, 'j'));
        let on_break = end < len && rope.char(end) == '\n';
        let end_col = end - rope.line_to_char(last);
        match (on_break, last == first) {
            (true, _) => keys.push(VimKey::Char('$')),
            (false, true) => keys.extend(counted(end - start, 'l')),
            (false, false) => {
                keys.push(VimKey::Char('0'));
                keys.extend(counted(end_col, 'l'));
            }
        }
    }
    keys.push(VimKey::Char(op));
    keys
}
