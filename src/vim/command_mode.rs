//! CommandLine mode: typing a `:` command or a `/` `?` search and running it.

use crate::buffer::Buffer;
use crate::editing::EditorState;
use crate::search::SearchQuery;

use super::cmdline;
use super::motion::{LastSearch, Motion};
use super::{Env, Mode, VimEffect, VimKey, VimState, clamp_normal};

impl VimState {
    /// Parses and executes an ex command line (text after `:`).
    pub(super) fn run_cmdline(
        &mut self,
        line: &str,
        buf: &mut Buffer,
        ed: &mut EditorState,
    ) -> Vec<VimEffect> {
        match cmdline::parse(line) {
            Ok(cmd) => cmdline::execute(cmd, buf, ed),
            Err(e) => vec![VimEffect::Error(e)],
        }
    }

    /// Handles a key in CommandLine mode.
    pub(super) fn cmdline_key(&mut self, key: VimKey, env: &mut Env) {
        match key {
            VimKey::Esc => self.leave_cmdline(),
            VimKey::Backspace if self.cmdline.chars().count() <= 1 => self.leave_cmdline(),
            VimKey::Backspace => {
                self.cmdline.pop();
            }
            VimKey::Char(c) => self.cmdline.push(c),
            VimKey::Enter => {
                let line = std::mem::take(&mut self.cmdline);
                self.mode = Mode::Normal;
                let mut chars = line.chars();
                let prefix = chars.next();
                let rest = chars.as_str();
                match prefix {
                    Some(':') if rest.trim().is_empty() => {}
                    Some(':') => {
                        let fx = self.run_cmdline(rest, env.buf, env.ed);
                        env.fx.extend(fx);
                    }
                    Some('/') => self.run_search(rest, true, env),
                    Some('?') => self.run_search(rest, false, env),
                    _ => {}
                }
                if self.mode == Mode::Normal {
                    let p = clamp_normal(env.buf.rope(), env.pos());
                    env.set_caret(p);
                }
            }
            _ => {}
        }
    }

    /// Cancels the command line and returns to Normal mode.
    pub(super) fn leave_cmdline(&mut self) {
        self.cmdline.clear();
        self.mode = Mode::Normal;
    }

    /// Runs a `/` or `?` search; an empty pattern reuses the last one.
    pub(super) fn run_search(&mut self, pattern: &str, forward: bool, env: &mut Env) {
        let pattern = match (pattern.is_empty(), &self.ctx.last_search) {
            (false, _) => pattern.to_string(),
            (true, Some(last)) => last.query.pattern.clone(),
            (true, None) => return env.error("E35: No previous regular expression"),
        };
        let query = SearchQuery {
            pattern,
            regex: true,
            case_sensitive: true,
        };
        match LastSearch::new(query, forward) {
            Ok(last) => self.ctx.last_search = Some(last),
            Err(e) => return env.error(e.to_string()),
        }
        let pos = env.pos();
        if let Some(r) = self.motion(&Motion::SearchNext { reverse: false }, None, pos, env) {
            env.set_caret(r.pos);
        }
    }
}
