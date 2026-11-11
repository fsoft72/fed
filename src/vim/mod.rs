//! Vim emulation core: modes, key handling, operators, registers and marks.
//!
//! Pure logic over a [`Buffer`] and an [`EditorState`]; the UI translates input into
//! [`VimKey`]s and acts on the returned [`VimEffect`]s.

pub mod motion;
mod normal;
pub mod textobj;

#[cfg(test)]
mod tests;

use std::ops::Range;
use std::path::PathBuf;

use ropey::Rope;

use crate::buffer::Buffer;
use crate::editing::{EditSettings, EditorState, Selection};
use crate::search::SearchQuery;
use crate::text_nav;

use motion::{LastSearch, Motion, MotionContext, last_line, line_end_of};
use normal::{Operator, Parse, VisualCmd, parse_visual};

/// Vim editing mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Commands and motions.
    Normal,
    /// Typing text.
    Insert,
    /// Charwise selection.
    Visual,
    /// Linewise selection.
    VisualLine,
    /// Typing a `:` command or a `/` `?` search.
    CommandLine,
}

/// A key as seen by the vim state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VimKey {
    /// A printable char.
    Char(char),
    /// Escape.
    Esc,
    /// Enter / Return.
    Enter,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Control plus a (lowercase) char, e.g. `Ctrl('r')`.
    Ctrl(char),
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
}

/// A side effect the UI must perform after a key.
#[derive(Clone, Debug, PartialEq)]
pub enum VimEffect {
    /// Copy text to the system clipboard (the unnamed register changed).
    SetClipboard(String),
    /// Show an informational message.
    Message(String),
    /// Show an error message.
    Error(String),
    /// Write the buffer, to the given path when present.
    Write(Option<PathBuf>),
    /// Close the tab, discarding changes when `force`.
    Quit {
        /// Discard unsaved changes.
        force: bool,
    },
    /// Write the buffer and close the tab.
    WriteQuit,
    /// Open a file.
    Edit(PathBuf),
    /// Jump to a 1-based line (reserved for the command line).
    GotoLine(usize),
}

/// The single unnamed register.
#[derive(Clone, Debug, Default)]
struct Register {
    /// Register content (ends with `\n` when linewise).
    text: String,
    /// Content is whole lines.
    linewise: bool,
}

/// Text an operator acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OpRange {
    /// A char range.
    Chars(Range<usize>),
    /// Lines `first..=last` (0-based).
    Lines(usize, usize),
}

/// Mutable editing context passed through the key handlers.
struct Env<'a> {
    buf: &'a mut Buffer,
    ed: &'a mut EditorState,
    settings: &'a EditSettings,
    fx: Vec<VimEffect>,
}

impl Env<'_> {
    /// Returns the caret, clamped to the buffer.
    fn pos(&self) -> usize {
        self.ed.selection.head.min(self.buf.rope().len_chars())
    }

    /// Collapses the selection to a caret at `pos`.
    fn set_caret(&mut self, pos: usize) {
        self.ed.selection = Selection::caret(pos);
    }

    /// Reports an error message.
    fn error(&mut self, msg: impl Into<String>) {
        self.fx.push(VimEffect::Error(msg.into()));
    }
}

/// Vim state of one tab.
#[derive(Debug)]
pub struct VimState {
    mode: Mode,
    /// Keys of the normal/visual command being typed.
    keys: Vec<VimKey>,
    /// Rendering of `keys` for the status bar.
    pending_text: String,
    /// Command line text including its `:` `/` `?` prefix.
    cmdline: String,
    register: Register,
    ctx: MotionContext,
    /// Keys of the last change, replayed by `.`.
    last_change: Vec<VimKey>,
    /// Keys of the change in progress while in Insert mode.
    recording: Option<Vec<VimKey>>,
    /// An undo group opened by a change is still open.
    group_open: bool,
}

impl Default for VimState {
    fn default() -> Self {
        Self::new()
    }
}

impl VimState {
    /// Creates a state in Normal mode with an empty register.
    pub fn new() -> Self {
        VimState {
            mode: Mode::Normal,
            keys: Vec::new(),
            pending_text: String::new(),
            cmdline: String::new(),
            register: Register::default(),
            ctx: MotionContext::default(),
            last_change: Vec::new(),
            recording: None,
            group_open: false,
        }
    }

    /// Returns the current mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Returns the command line text including its prefix (e.g. `":w"`, `"/foo"`).
    pub fn cmdline(&self) -> &str {
        &self.cmdline
    }

    /// Returns the pending normal-mode keys (e.g. `"2d"`).
    pub fn pending(&self) -> &str {
        &self.pending_text
    }

    /// Returns the selected char range to render in Visual modes, `None` otherwise.
    pub fn visual_range(&self, buf: &Buffer, ed: &EditorState) -> Option<Range<usize>> {
        let rope = buf.rope();
        let len = rope.len_chars();
        let sel = ed.selection;
        let (start, end) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
        match self.mode {
            Mode::Visual => Some(start.min(len)..(end + 1).min(len)),
            Mode::VisualLine => Some(
                text_nav::line_start(rope, start)..(text_nav::line_end(rope, end) + 1).min(len),
            ),
            _ => None,
        }
    }

    /// Feeds one key; returns the effects the UI must perform.
    pub fn handle_key(
        &mut self,
        key: VimKey,
        buf: &mut Buffer,
        ed: &mut EditorState,
        settings: &EditSettings,
    ) -> Vec<VimEffect> {
        let mut env = Env {
            buf,
            ed,
            settings,
            fx: Vec::new(),
        };
        self.dispatch(key, &mut env);
        self.pending_text = render_keys(&self.keys);
        env.fx
    }

    /// Routes a key to the handler of the current mode.
    fn dispatch(&mut self, key: VimKey, env: &mut Env) {
        match self.mode {
            Mode::Normal => self.normal_key(key, env),
            Mode::Insert => self.insert_key(key, env),
            Mode::Visual | Mode::VisualLine => self.visual_key(key, env),
            Mode::CommandLine => self.cmdline_key(key, env),
        }
    }

    /// Executes an ex command line (text after `:`). Task 13 replaces this stub.
    fn run_cmdline(
        &mut self,
        line: &str,
        _buf: &mut Buffer,
        _ed: &mut EditorState,
        _settings: &EditSettings,
    ) -> Vec<VimEffect> {
        vec![VimEffect::Error(format!(
            "E492: Not an editor command: {line}"
        ))]
    }

    /// Opens the undo group of a change.
    fn begin_change(&mut self, buf: &mut Buffer) {
        if self.group_open {
            return;
        }
        buf.begin_group();
        self.group_open = true;
    }

    /// Closes the undo group of a change, if open.
    fn end_change(&mut self, buf: &mut Buffer) {
        if !self.group_open {
            return;
        }
        buf.end_group();
        self.group_open = false;
    }

    /// Stores text in the unnamed register and syncs the clipboard.
    fn set_register(&mut self, text: String, linewise: bool, env: &mut Env) {
        env.fx.push(VimEffect::SetClipboard(text.clone()));
        self.register = Register { text, linewise };
    }

    /// Switches to Insert mode with the caret at `pos`.
    fn enter_insert(&mut self, pos: usize, env: &mut Env) {
        self.mode = Mode::Insert;
        env.ed.preferred_col = None;
        env.set_caret(pos);
    }

    /// Applies `op` to `range`; Change leaves the editor in Insert mode.
    fn apply_operator(&mut self, op: Operator, range: OpRange, env: &mut Env) {
        if matches!(op, Operator::Indent | Operator::Outdent) {
            let rope = env.buf.rope();
            let (first, last) = match range {
                OpRange::Lines(a, b) => (a, b),
                OpRange::Chars(r) => (rope.char_to_line(r.start), rope.char_to_line(r.end)),
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
    fn apply_chars(&mut self, op: Operator, r: Range<usize>, env: &mut Env) {
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
    fn apply_lines(&mut self, op: Operator, a: usize, b: usize, env: &mut Env) {
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

    /// Handles a key in Insert mode.
    fn insert_key(&mut self, key: VimKey, env: &mut Env) {
        if let Some(rec) = &mut self.recording {
            rec.push(key.clone());
        }
        let pos = env.pos();
        let rope = env.buf.rope().clone();
        match key {
            VimKey::Char(c) => insert_at(env, pos, &c.to_string()),
            VimKey::Enter => {
                let indent = leading_blanks(&rope, text_nav::line_start(&rope, pos));
                let keep: String = indent
                    .chars()
                    .take(pos - text_nav::line_start(&rope, pos))
                    .collect();
                insert_at(env, pos, &format!("\n{keep}"));
            }
            VimKey::Tab => {
                let col = pos - text_nav::line_start(&rope, pos);
                let width = env.settings.tab_width.max(1);
                let unit = match env.settings.insert_spaces {
                    true => " ".repeat(width - col % width),
                    false => "\t".to_string(),
                };
                insert_at(env, pos, &unit);
            }
            VimKey::Backspace => {
                if pos == 0 {
                    return;
                }
                env.buf.replace(pos - 1..pos, "");
                env.set_caret(pos - 1);
            }
            VimKey::Left if pos > text_nav::line_start(&rope, pos) => env.set_caret(pos - 1),
            VimKey::Right if pos < text_nav::line_end(&rope, pos) => env.set_caret(pos + 1),
            VimKey::Up | VimKey::Down => {
                let m = if key == VimKey::Up {
                    Motion::Up
                } else {
                    Motion::Down
                };
                if let Some(r) = self.motion(&m, None, pos, env) {
                    env.set_caret(r.pos);
                }
            }
            VimKey::Esc => {
                self.end_change(env.buf);
                if let Some(rec) = self.recording.take() {
                    self.last_change = rec;
                }
                self.mode = Mode::Normal;
                let back = if pos > text_nav::line_start(&rope, pos) {
                    pos - 1
                } else {
                    pos
                };
                env.set_caret(clamp_normal(&rope, back));
            }
            _ => {}
        }
    }

    /// Handles a key in Visual and VisualLine mode.
    fn visual_key(&mut self, key: VimKey, env: &mut Env) {
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
    fn visual_operator(&mut self, c: char, env: &mut Env) {
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
                let last = rope.char_to_line(sel.end).min(last_line(rope));
                OpRange::Lines(rope.char_to_line(sel.start), last)
            }
            _ => {
                let Some(r) = self.visual_range(env.buf, env.ed) else {
                    return;
                };
                OpRange::Chars(r)
            }
        };
        let sel_start = env.ed.selection.range().start;
        self.leave_visual(env);
        if op != Operator::Yank {
            self.begin_change(env.buf);
        }
        env.set_caret(match &range {
            OpRange::Chars(r) => r.start,
            OpRange::Lines(..) => sel_start,
        });
        self.apply_operator(op, range, env);
        if self.mode != Mode::Insert {
            self.end_change(env.buf);
            let p = clamp_normal(env.buf.rope(), env.pos());
            env.set_caret(p);
        }
    }

    /// Returns to Normal mode, recording the selection in marks `<` and `>`.
    fn leave_visual(&mut self, env: &mut Env) {
        let sel = env.ed.selection;
        env.buf.set_mark('<', sel.anchor.min(sel.head));
        env.buf.set_mark('>', sel.anchor.max(sel.head));
        self.mode = Mode::Normal;
        let p = clamp_normal(env.buf.rope(), sel.head);
        env.set_caret(p);
    }

    /// Handles a key in CommandLine mode.
    fn cmdline_key(&mut self, key: VimKey, env: &mut Env) {
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
                    Some(':') => {
                        let fx = self.run_cmdline(rest, env.buf, env.ed, env.settings);
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
    fn leave_cmdline(&mut self) {
        self.cmdline.clear();
        self.mode = Mode::Normal;
    }

    /// Runs a `/` or `?` search; an empty pattern reuses the last one.
    fn run_search(&mut self, pattern: &str, forward: bool, env: &mut Env) {
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
        if let Err(e) = query.compile() {
            return env.error(e.to_string());
        }
        self.ctx.last_search = Some(LastSearch { query, forward });
        let pos = env.pos();
        if let Some(r) = self.motion(&Motion::SearchNext { reverse: false }, None, pos, env) {
            env.set_caret(r.pos);
        }
    }
}

/// Renders pending keys for the status bar.
fn render_keys(keys: &[VimKey]) -> String {
    keys.iter()
        .filter_map(|k| match k {
            VimKey::Char(c) => Some(c.to_string()),
            VimKey::Ctrl(c) => Some(format!("^{}", c.to_ascii_uppercase())),
            _ => None,
        })
        .collect()
}

/// Clamps `pos` to a Normal-mode caret: on a char, never on `\n` unless the line is empty.
fn clamp_normal(rope: &Rope, pos: usize) -> usize {
    let len = rope.len_chars();
    if len == 0 {
        return 0;
    }
    let mut p = pos.min(len);
    if p == len && rope.char(len - 1) == '\n' {
        p = len - 1;
    }
    let start = text_nav::line_start(rope, p);
    let end = text_nav::line_end(rope, p);
    if end == start {
        return start;
    }
    p.min(end - 1)
}

/// Returns the leading spaces and tabs of the line starting at `line_start`.
fn leading_blanks(rope: &Rope, line_start: usize) -> String {
    rope.slice(line_start..text_nav::line_end(rope, line_start))
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .collect()
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

/// Inserts `text` at `pos` and puts the caret after it.
fn insert_at(env: &mut Env, pos: usize, text: &str) {
    env.buf.replace(pos..pos, text);
    env.set_caret(pos + text.chars().count());
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
