//! Vim emulation core: modes, key handling, operators, registers and marks.
//!
//! Pure logic over a [`Buffer`] and an [`EditorState`]; the UI translates input into
//! [`VimKey`]s and acts on the returned [`VimEffect`]s.
//!
//! `keys` translates egui keyboard events into `VimKey`s.
//!
//! One file per mode: `normal` (parsing and Normal commands), `insert`, `visual` and
//! `command_mode`; `operator` applies `d c y > <`, `motion` and `textobj` find their ranges
//! and `cmdline` parses and runs ex commands.

pub mod cmdline;
mod command_mode;
mod insert;
pub mod keys;
pub mod motion;
mod normal;
mod operator;
pub mod textobj;
mod visual;

#[cfg(test)]
mod tests;

use std::ops::Range;
use std::path::PathBuf;

use ropey::Rope;

use crate::buffer::Buffer;
use crate::editing::{EditSettings, EditorState, Selection};
use crate::text_nav;

use motion::MotionContext;

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
    /// Text pasted in Insert mode, inserted as one edit (ignored in the other modes).
    Text(String),
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
    /// Write a copy to the path even when that file exists (`:w! {file}`).
    ForceWrite(PathBuf),
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
    /// The caret line holds only indentation inserted automatically (removed on Esc).
    auto_indent: bool,
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
            auto_indent: false,
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

    /// Closes the undo group of an Insert session (before saving, switching tabs or editing
    /// the buffer from outside). Insert mode continues; the next typed key opens a new group.
    pub fn end_insert_group(&mut self, buf: &mut Buffer) {
        self.end_change(buf);
    }

    /// Syncs the mode with a selection the mouse just made: a non-empty selection enters
    /// Visual mode (the head becomes the inclusive end), an empty one leaves Visual mode.
    pub fn mouse_selection(&mut self, buf: &mut Buffer, ed: &mut EditorState) {
        let sel = ed.selection;
        if sel.is_empty() {
            if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
                self.mode = Mode::Normal;
            }
            if self.mode == Mode::Normal {
                ed.selection = Selection::caret(clamp_normal(buf.rope(), sel.head));
            }
            return;
        }
        if self.mode == Mode::Insert {
            self.end_change(buf);
            if let Some(rec) = self.recording.take() {
                self.last_change = rec;
            }
        }
        self.keys.clear();
        self.pending_text.clear();
        self.cmdline.clear();
        if self.mode != Mode::VisualLine {
            self.mode = Mode::Visual;
        }
        let head = match sel.head > sel.anchor {
            true => sel.head - 1,
            false => sel.head,
        };
        ed.selection.head = clamp_normal(buf.rope(), head);
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
        self.auto_indent = false;
        env.ed.preferred_col = None;
        env.set_caret(pos);
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
/// The empty line after a final `\n` is kept when an edit left the caret there; motions never
/// target it (see `motion::last_line`).
fn clamp_normal(rope: &Rope, pos: usize) -> usize {
    let len = rope.len_chars();
    if len == 0 {
        return 0;
    }
    let p = pos.min(len);
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
