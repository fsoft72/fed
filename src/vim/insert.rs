//! Insert mode: typing, line breaks with auto-indent, soft tabs and leaving with Esc.

use crate::text_nav;

use super::motion::Motion;
use super::{Env, Mode, VimKey, VimState, clamp_normal, leading_blanks};

impl VimState {
    /// Handles a key in Insert mode.
    pub(super) fn insert_key(&mut self, key: VimKey, env: &mut Env) {
        // The group may have been closed from outside (`end_insert_group`).
        self.begin_change(env.buf);
        if let Some(rec) = &mut self.recording {
            rec.push(key.clone());
        }
        if !matches!(key, VimKey::Up | VimKey::Down) {
            env.ed.preferred_col = None;
        }
        if !matches!(key, VimKey::Enter | VimKey::Esc) {
            self.auto_indent = false;
        }
        let pos = env.pos();
        let rope = env.buf.rope();
        match key {
            VimKey::Char(c) => insert_at(env, pos, &c.to_string()),
            VimKey::Text(text) => insert_at(env, pos, &text),
            VimKey::Enter => self.insert_newline(pos, env),
            VimKey::Tab => insert_soft_tab(pos, env),
            VimKey::Backspace if pos > 0 => {
                env.buf.replace(pos - 1..pos, "");
                env.set_caret(pos - 1);
            }
            VimKey::Left if pos > text_nav::line_start(rope, pos) => env.set_caret(pos - 1),
            VimKey::Right if pos < text_nav::line_end(rope, pos) => env.set_caret(pos + 1),
            VimKey::Up | VimKey::Down => {
                let m = match key {
                    VimKey::Up => Motion::Up,
                    _ => Motion::Down,
                };
                if let Some(r) = self.motion(&m, None, pos, env) {
                    env.set_caret(r.pos);
                }
            }
            VimKey::Esc => self.leave_insert(pos, env),
            _ => {}
        }
    }

    /// Enter in Insert mode: breaks the line keeping the indent before the caret, which
    /// counts as auto-indent (dropped again if nothing is typed after it).
    fn insert_newline(&mut self, pos: usize, env: &mut Env) {
        let rope = env.buf.rope();
        let line_start = text_nav::line_start(rope, pos);
        let indent = leading_blanks(rope, line_start);
        let keep: String = indent.chars().take(pos - line_start).collect();
        let pos = self.drop_auto_indent(pos, env);
        insert_at(env, pos, &format!("\n{keep}"));
        self.auto_indent = true;
    }

    /// Esc in Insert mode: drops untouched auto-indent, commits the change (recorded for
    /// `.`) and returns to Normal mode with the caret one char back, like vim.
    fn leave_insert(&mut self, pos: usize, env: &mut Env) {
        let pos = self.drop_auto_indent(pos, env);
        self.end_change(env.buf);
        if let Some(rec) = self.recording.take() {
            self.last_change = rec;
        }
        self.mode = Mode::Normal;
        let rope = env.buf.rope();
        let back = match pos > text_nav::line_start(rope, pos) {
            true => pos - 1,
            false => pos,
        };
        env.set_caret(clamp_normal(rope, back));
    }

    /// Removes the untouched auto-indent of the caret line (vim); returns the new caret.
    pub(super) fn drop_auto_indent(&mut self, pos: usize, env: &mut Env) -> usize {
        if !std::mem::take(&mut self.auto_indent) {
            return pos;
        }
        let rope = env.buf.rope();
        let start = text_nav::line_start(rope, pos);
        let end = text_nav::line_end(rope, pos);
        if pos != end || (start..end).any(|i| !matches!(rope.char(i), ' ' | '\t')) {
            return pos;
        }
        env.buf.replace(start..end, "");
        env.set_caret(start);
        start
    }
}

/// Inserts `text` at `pos` and puts the caret after it (line breaks are normalized to LF).
fn insert_at(env: &mut Env, pos: usize, text: &str) {
    let text = crate::buffer::normalize_newlines(text);
    env.buf.replace(pos..pos, &text);
    env.set_caret(pos + text.chars().count());
}

/// Tab in Insert mode: spaces up to the next tab stop (by display column), or a tab char.
fn insert_soft_tab(pos: usize, env: &mut Env) {
    let width = env.settings.tab_width.max(1);
    let col = text_nav::display_col(env.buf.rope(), pos, width);
    let unit = match env.settings.insert_spaces {
        true => " ".repeat(width - col % width),
        false => "\t".to_string(),
    };
    insert_at(env, pos, &unit);
}
