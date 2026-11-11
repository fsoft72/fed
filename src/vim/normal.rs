//! Normal-mode key parsing (counts, operators, motions, text objects) and execution.

use crate::search::SearchQuery;
use crate::text_nav::{self, CharClass, char_class};

use super::motion::{LastFind, LastSearch, Motion, MotionResult, last_line, line_end_of, resolve};
use super::textobj::text_object;
use super::{Env, Mode, OpRange, VimKey, VimState, clamp_normal};

/// Upper bound for typed counts, so huge counts cannot stall the editor.
const MAX_COUNT: usize = 99_999;
/// Normal-mode commands that modify the buffer or enter Insert mode.
const CHANGE_COMMANDS: &str = "xXDCsSJpPiaIAoO";
/// Normal-mode single-key commands (besides motions).
const SIMPLE_COMMANDS: &str = "xXDCsSJpPu.*#iaIAoOvV:/?";

/// Result of parsing the keys typed so far.
#[derive(Debug)]
pub(super) enum Parse<T> {
    /// More keys are needed.
    Incomplete,
    /// The keys do not form a command.
    Invalid,
    /// A complete command.
    Done(T),
}

/// A command with its optional count.
#[derive(Debug)]
pub(super) struct Parsed<T> {
    /// Explicit count typed before the command (multiplied for operators).
    pub count: Option<usize>,
    /// The command.
    pub cmd: T,
}

/// An operator that acts on a motion, a text object or lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operator {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
}

/// What an operator acts on.
#[derive(Debug)]
pub(super) enum Target {
    /// Text covered by a motion.
    Motion(Motion),
    /// A text object such as `iw`.
    Object { kind: char, inner: bool },
    /// Doubled operator (`dd`, `yy`, `>>`): whole lines.
    Lines,
}

/// A parsed Normal-mode command.
#[derive(Debug)]
pub(super) enum NormalCmd {
    /// Move the caret.
    Move(Motion),
    /// Operator plus target.
    Op(Operator, Target),
    /// Single-key command from `SIMPLE_COMMANDS`.
    Simple(char),
    /// `r{c}`.
    Replace(char),
    /// `m{a-z}`.
    SetMark(char),
    /// `Ctrl+R`.
    Redo,
}

impl NormalCmd {
    /// Returns true when the command is a change repeatable with `.`.
    fn is_change(&self) -> bool {
        match self {
            NormalCmd::Op(op, _) => *op != Operator::Yank,
            NormalCmd::Replace(_) => true,
            NormalCmd::Simple(c) => CHANGE_COMMANDS.contains(*c),
            _ => false,
        }
    }
}

/// A parsed Visual-mode command.
#[derive(Debug)]
pub(super) enum VisualCmd {
    /// Move the selection head.
    Move(Motion),
    /// One of `d x c y > < o v V :`.
    Action(char),
    /// Leave Visual mode.
    Esc,
}

/// Parses a leading count; returns it and the number of keys consumed.
fn parse_count(keys: &[VimKey]) -> (Option<usize>, usize) {
    let mut count: Option<usize> = None;
    let mut used = 0;
    while let Some(VimKey::Char(c)) = keys.get(used) {
        let Some(d) = c.to_digit(10) else { break };
        if d == 0 && count.is_none() {
            break;
        }
        count = Some((count.unwrap_or(0) * 10 + d as usize).min(MAX_COUNT));
        used += 1;
    }
    (count, used)
}

/// Multiplies two optional counts (`2d3w` = 6).
fn mul_counts(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (None, None) => None,
        _ => Some((a.unwrap_or(1) * b.unwrap_or(1)).min(MAX_COUNT)),
    }
}

/// Parses keys that must form exactly one motion.
fn parse_motion(keys: &[VimKey]) -> Parse<Motion> {
    let Some(first) = keys.first() else {
        return Parse::Incomplete;
    };
    let with_char = |make: &dyn Fn(char) -> Option<Motion>| match keys.get(1) {
        None => Parse::Incomplete,
        Some(VimKey::Char(c)) if keys.len() == 2 => make(*c).map_or(Parse::Invalid, Parse::Done),
        _ => Parse::Invalid,
    };
    let motion = match first {
        VimKey::Char('h') | VimKey::Left | VimKey::Backspace => Motion::Left,
        VimKey::Char('l') | VimKey::Right | VimKey::Char(' ') => Motion::Right,
        VimKey::Char('k') | VimKey::Up => Motion::Up,
        VimKey::Char('j') | VimKey::Down => Motion::Down,
        VimKey::Char(c @ ('w' | 'W')) => Motion::WordStart { big: *c == 'W' },
        VimKey::Char(c @ ('b' | 'B')) => Motion::WordBack { big: *c == 'B' },
        VimKey::Char(c @ ('e' | 'E')) => Motion::WordEnd { big: *c == 'E' },
        VimKey::Char('0') => Motion::LineStart,
        VimKey::Char('^') => Motion::FirstNonBlank,
        VimKey::Char('$') => Motion::LineEnd,
        VimKey::Char('G') => Motion::DocEnd,
        VimKey::Char(c @ (';' | ',')) => Motion::RepeatFind { reverse: *c == ',' },
        VimKey::Char('%') => Motion::MatchBracket,
        VimKey::Char('}') => Motion::ParagraphNext,
        VimKey::Char('{') => Motion::ParagraphPrev,
        VimKey::Char(c @ ('n' | 'N')) => Motion::SearchNext { reverse: *c == 'N' },
        VimKey::Char('g') => {
            return match keys.get(1) {
                None => Parse::Incomplete,
                Some(VimKey::Char('g')) if keys.len() == 2 => Parse::Done(Motion::DocStart),
                _ => Parse::Invalid,
            };
        }
        VimKey::Char(f @ ('f' | 'F' | 't' | 'T')) => {
            let forward = matches!(f, 'f' | 't');
            let till = matches!(f, 't' | 'T');
            return with_char(&|c| Some(Motion::FindChar { c, forward, till }));
        }
        VimKey::Char(m @ ('\'' | '`')) => {
            let exact = *m == '`';
            return with_char(&|c| {
                let mark = match exact {
                    true => Motion::MarkExact(c),
                    false => Motion::MarkLine(c),
                };
                c.is_ascii_lowercase().then_some(mark)
            });
        }
        _ => return Parse::Invalid,
    };
    if keys.len() == 1 {
        Parse::Done(motion)
    } else {
        Parse::Invalid
    }
}

/// Maps an operator key to its operator.
fn operator_of(key: &VimKey) -> Option<Operator> {
    match key {
        VimKey::Char('d') => Some(Operator::Delete),
        VimKey::Char('c') => Some(Operator::Change),
        VimKey::Char('y') => Some(Operator::Yank),
        VimKey::Char('>') => Some(Operator::Indent),
        VimKey::Char('<') => Some(Operator::Outdent),
        _ => None,
    }
}

/// Parses the keys after an operator: a count, then the doubled operator, a text object or a motion.
fn parse_operator_target(op_key: &VimKey, keys: &[VimKey]) -> Parse<(Option<usize>, Target)> {
    let (count, used) = parse_count(keys);
    let rest = &keys[used..];
    let Some(first) = rest.first() else {
        return Parse::Incomplete;
    };
    if first == op_key {
        return match rest.len() {
            1 => Parse::Done((count, Target::Lines)),
            _ => Parse::Invalid,
        };
    }
    if let VimKey::Char(ia @ ('i' | 'a')) = first {
        return match rest.get(1) {
            None => Parse::Incomplete,
            Some(VimKey::Char(kind)) if rest.len() == 2 && "w({[\"')}]bB".contains(*kind) => {
                Parse::Done((
                    count,
                    Target::Object {
                        kind: *kind,
                        inner: *ia == 'i',
                    },
                ))
            }
            _ => Parse::Invalid,
        };
    }
    match parse_motion(rest) {
        Parse::Done(m) => Parse::Done((count, Target::Motion(m))),
        Parse::Incomplete => Parse::Incomplete,
        Parse::Invalid => Parse::Invalid,
    }
}

/// Parses the keys typed so far in Normal mode.
pub(super) fn parse_normal(keys: &[VimKey]) -> Parse<Parsed<NormalCmd>> {
    let (count, used) = parse_count(keys);
    let rest = &keys[used..];
    let Some(first) = rest.first() else {
        return Parse::Incomplete;
    };
    let done = |cmd| Parse::Done(Parsed { count, cmd });
    if let Some(op) = operator_of(first) {
        return match parse_operator_target(first, &rest[1..]) {
            Parse::Done((inner, target)) => Parse::Done(Parsed {
                count: mul_counts(count, inner),
                cmd: NormalCmd::Op(op, target),
            }),
            Parse::Incomplete => Parse::Incomplete,
            Parse::Invalid => Parse::Invalid,
        };
    }
    match first {
        VimKey::Char(c @ ('r' | 'm')) => match rest.get(1) {
            None => Parse::Incomplete,
            Some(VimKey::Char(x)) if rest.len() == 2 => match c {
                'r' => done(NormalCmd::Replace(*x)),
                _ if x.is_ascii_lowercase() => done(NormalCmd::SetMark(*x)),
                _ => Parse::Invalid,
            },
            _ => Parse::Invalid,
        },
        VimKey::Ctrl('r') if rest.len() == 1 => done(NormalCmd::Redo),
        VimKey::Char(c) if rest.len() == 1 && SIMPLE_COMMANDS.contains(*c) => {
            done(NormalCmd::Simple(*c))
        }
        _ => match parse_motion(rest) {
            Parse::Done(m) => done(NormalCmd::Move(m)),
            Parse::Incomplete => Parse::Incomplete,
            Parse::Invalid => Parse::Invalid,
        },
    }
}

/// Parses the keys typed so far in Visual mode.
pub(super) fn parse_visual(keys: &[VimKey]) -> Parse<Parsed<VisualCmd>> {
    let (count, used) = parse_count(keys);
    let rest = &keys[used..];
    let Some(first) = rest.first() else {
        return Parse::Incomplete;
    };
    let cmd = match first {
        VimKey::Esc => VisualCmd::Esc,
        VimKey::Char(c) if "dxcy><ovV:".contains(*c) => VisualCmd::Action(*c),
        _ => match parse_motion(rest) {
            Parse::Done(m) => VisualCmd::Move(m),
            Parse::Incomplete => return Parse::Incomplete,
            Parse::Invalid => return Parse::Invalid,
        },
    };
    Parse::Done(Parsed { count, cmd })
}

/// Replaces the leading count of a recorded change with `count`.
fn with_count(keys: &[VimKey], count: usize) -> Vec<VimKey> {
    let (_, used) = parse_count(keys);
    let mut out: Vec<VimKey> = count.to_string().chars().map(VimKey::Char).collect();
    out.extend_from_slice(&keys[used..]);
    out
}

impl VimState {
    /// Handles a key in Normal mode.
    pub(super) fn normal_key(&mut self, key: VimKey, env: &mut Env) {
        self.keys.push(key);
        let parsed = match parse_normal(&self.keys) {
            Parse::Incomplete => return,
            Parse::Invalid => return self.keys.clear(),
            Parse::Done(parsed) => parsed,
        };
        let keys = std::mem::take(&mut self.keys);
        // The UI may have left the caret past the line end (e.g. a click).
        let p = clamp_normal(env.buf.rope(), env.pos());
        env.set_caret(p);
        let change = parsed.cmd.is_change();
        if change {
            self.begin_change(env.buf);
        }
        self.run_normal(parsed, env);
        if change && self.mode == Mode::Insert {
            self.recording = Some(keys);
        } else if change {
            self.end_change(env.buf);
            self.last_change = keys;
        }
        if self.mode == Mode::Normal {
            let p = clamp_normal(env.buf.rope(), env.pos());
            env.set_caret(p);
        }
    }

    /// Executes a parsed Normal-mode command.
    fn run_normal(&mut self, parsed: Parsed<NormalCmd>, env: &mut Env) {
        let count = parsed.count;
        let n = count.unwrap_or(1);
        let pos = env.pos();
        match parsed.cmd {
            NormalCmd::Move(m) => {
                if let Some(r) = self.motion(&m, count, pos, env) {
                    env.set_caret(r.pos);
                }
            }
            NormalCmd::Op(op, target) => {
                if let Some(range) = self.op_range(op, target, count, pos, env) {
                    self.apply_operator(op, range, env);
                }
            }
            NormalCmd::Replace(c) => replace_chars(env, pos, c, n),
            NormalCmd::SetMark(c) => env.buf.set_mark(c, pos),
            NormalCmd::Redo => self.undo_redo(false, n, env),
            NormalCmd::Simple(c) => self.simple(c, count, pos, env),
        }
    }

    /// Executes a single-key command.
    fn simple(&mut self, c: char, count: Option<usize>, pos: usize, env: &mut Env) {
        let n = count.unwrap_or(1);
        let rope = env.buf.rope().clone();
        let start = text_nav::line_start(&rope, pos);
        let end = text_nav::line_end(&rope, pos);
        let line = rope.char_to_line(pos);
        let chars_right = OpRange::Chars(pos..(pos + n).min(end));
        match c {
            'x' => self.apply_operator(Operator::Delete, chars_right, env),
            'X' => {
                let range = OpRange::Chars(pos.saturating_sub(n).max(start)..pos);
                self.apply_operator(Operator::Delete, range, env);
            }
            's' => self.apply_operator(Operator::Change, chars_right, env),
            'S' => {
                let range = OpRange::Lines(line, (line + n - 1).min(last_line(&rope)));
                self.apply_operator(Operator::Change, range, env);
            }
            'D' | 'C' => {
                let op = if c == 'D' {
                    Operator::Delete
                } else {
                    Operator::Change
                };
                let target = Target::Motion(Motion::LineEnd);
                if let Some(range) = self.op_range(op, target, count, pos, env) {
                    self.apply_operator(op, range, env);
                }
            }
            'J' => join_lines(env, pos, n),
            'p' | 'P' => self.put(c == 'p', n, pos, env),
            'u' => self.undo_redo(true, n, env),
            '.' => self.repeat_change(count, env),
            '*' | '#' => self.search_word(c == '*', n, pos, env),
            'i' => self.enter_insert(pos, env),
            'a' => self.enter_insert((pos + 1).min(end), env),
            'I' => self.enter_insert(text_nav::first_non_blank(&rope, pos), env),
            'A' => self.enter_insert(end, env),
            'o' | 'O' => {
                let indent = super::leading_blanks(&rope, start);
                let (at, text) = match c {
                    'o' => (end, format!("\n{indent}")),
                    _ => (start, format!("{indent}\n")),
                };
                env.buf.replace(at..at, &text);
                // The caret goes after the indent of the new line.
                let caret = at + text.chars().count() - usize::from(c == 'O');
                self.enter_insert(caret, env);
            }
            'v' | 'V' => {
                self.mode = if c == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                env.ed.selection.anchor = pos;
            }
            _ => {
                self.mode = Mode::CommandLine;
                self.cmdline = c.to_string();
            }
        }
    }

    /// Resolves a motion with error reporting, `{n}gg`/`{n}G` mapping and state updates.
    pub(super) fn motion(
        &mut self,
        m: &Motion,
        count: Option<usize>,
        pos: usize,
        env: &mut Env,
    ) -> Option<MotionResult> {
        let m = match (m, count) {
            (Motion::DocStart | Motion::DocEnd, Some(n)) => Motion::GotoLine(n),
            _ => m.clone(),
        };
        match &m {
            Motion::MarkLine(c) | Motion::MarkExact(c) if env.buf.mark(*c).is_none() => {
                env.error("E20: Mark not set");
                return None;
            }
            Motion::SearchNext { .. } if self.ctx.last_search.is_none() => {
                env.error("E35: No previous regular expression");
                return None;
            }
            Motion::FindChar { c, forward, till } => {
                self.ctx.last_find = Some(LastFind {
                    c: *c,
                    forward: *forward,
                    till: *till,
                });
            }
            _ => {}
        }
        self.ctx.preferred_col = env.ed.preferred_col;
        let result = resolve(&m, count.unwrap_or(1), env.buf, pos, &self.ctx);
        if result.is_none()
            && let (Motion::SearchNext { .. }, Some(last)) = (&m, &self.ctx.last_search)
        {
            env.error(format!("E486: Pattern not found: {}", last.query.pattern));
        }
        env.ed.preferred_col = match m {
            Motion::Up | Motion::Down => {
                let col = text_nav::line_col(env.buf.rope(), pos).1;
                Some(self.ctx.preferred_col.unwrap_or(col))
            }
            Motion::LineEnd => Some(usize::MAX),
            _ => None,
        };
        result
    }

    /// Computes the range an operator acts on, applying vim's exclusive-motion rules.
    fn op_range(
        &mut self,
        op: Operator,
        target: Target,
        count: Option<usize>,
        pos: usize,
        env: &mut Env,
    ) -> Option<OpRange> {
        let rope = env.buf.rope().clone();
        let n = count.unwrap_or(1);
        let line = rope.char_to_line(pos);
        let m = match target {
            Target::Lines => {
                return Some(OpRange::Lines(line, (line + n - 1).min(last_line(&rope))));
            }
            Target::Object { kind, inner } => {
                return text_object(kind, inner, env.buf, pos).map(OpRange::Chars);
            }
            Target::Motion(m) => m,
        };
        if let Motion::WordStart { big } = m
            && op == Operator::Change
            && let Some(range) = change_word_range(&rope, pos, n, big)
        {
            return Some(range);
        }
        let r = self.motion(&m, count, pos, env)?;
        let target_line = rope.char_to_line(r.pos);
        if r.linewise {
            return Some(OpRange::Lines(line.min(target_line), line.max(target_line)));
        }
        let (start, mut end) = (pos.min(r.pos), pos.max(r.pos));
        if r.inclusive {
            end = (end + 1).min(rope.len_chars());
        }
        if let Motion::WordStart { big } = m
            && r.pos > pos
        {
            // The last word moved over ends the range when it is at the end of a line.
            let prev = (1..n).fold(pos, |p, _| super::motion::next_word_start(&rope, p, big));
            if target_line > rope.char_to_line(prev) {
                end = line_end_of(&rope, rope.char_to_line(prev));
            }
            if end <= start && super::motion::is_empty_line(&rope, line) {
                return Some(OpRange::Lines(line, line));
            }
        }
        let end_line = rope.char_to_line(end);
        if !r.inclusive && end > start && end_line > line && rope.line_to_char(end_line) == end {
            // `:help exclusive`: an end in column 0 moves back to the previous line end,
            // and the whole thing becomes linewise when it started before the first non-blank.
            if start <= text_nav::first_non_blank(&rope, start) {
                return Some(OpRange::Lines(line, end_line - 1));
            }
            end = line_end_of(&rope, end_line - 1);
        }
        Some(OpRange::Chars(start..end))
    }

    /// Pastes the register `count` times after (`p`) or before (`P`) the caret.
    fn put(&mut self, after: bool, count: usize, pos: usize, env: &mut Env) {
        if self.register.text.is_empty() {
            return;
        }
        let text = self.register.text.repeat(count);
        let rope = env.buf.rope().clone();
        let len = rope.len_chars();
        let line = rope.char_to_line(pos);
        if self.register.linewise {
            let line_end = line_end_of(&rope, line);
            let (at, text) = match (after, line_end < len) {
                (false, _) => (rope.line_to_char(line), text),
                (true, true) => (line_end + 1, text),
                (true, false) => (len, format!("\n{}", &text[..text.len() - 1])),
            };
            env.buf.replace(at..at, &text);
            let target = if after { line + 1 } else { line };
            let start = env.buf.rope().line_to_char(target);
            env.set_caret(text_nav::first_non_blank(env.buf.rope(), start));
            return;
        }
        let at = if after && text_nav::line_end(&rope, pos) > pos {
            pos + 1
        } else {
            pos
        };
        env.buf.replace(at..at, &text);
        let caret = match text.contains('\n') {
            true => at,
            false => at + text.chars().count() - 1,
        };
        env.set_caret(caret);
    }

    /// Undoes (or redoes) `count` steps.
    fn undo_redo(&mut self, undo: bool, count: usize, env: &mut Env) {
        let mut last = None;
        for _ in 0..count {
            let step = if undo { env.buf.undo() } else { env.buf.redo() };
            match step {
                Some(p) => last = Some(p),
                None => break,
            }
        }
        env.ed.preferred_col = None;
        let Some(p) = last else {
            let msg = match undo {
                true => "Already at oldest change",
                false => "Already at newest change",
            };
            return env.fx.push(super::VimEffect::Message(msg.into()));
        };
        let p = clamp_normal(env.buf.rope(), p);
        env.set_caret(p);
    }

    /// `.`: replays the last change, with `count` replacing its count.
    fn repeat_change(&mut self, count: Option<usize>, env: &mut Env) {
        if self.last_change.is_empty() {
            return;
        }
        let keys = match count {
            Some(n) => with_count(&self.last_change, n),
            None => self.last_change.clone(),
        };
        for key in keys {
            self.dispatch(key, env);
        }
    }

    /// `*` / `#`: searches the word under (or after) the caret as a whole word.
    fn search_word(&mut self, forward: bool, count: usize, pos: usize, env: &mut Env) {
        let rope = env.buf.rope().clone();
        let end = text_nav::line_end(&rope, pos);
        let Some(at) = (pos..end).find(|&i| char_class(rope.char(i), false) == CharClass::Word)
        else {
            return env.error("E348: No string under cursor");
        };
        let word = text_nav::word_range_at(&rope, at);
        let pattern = format!(
            r"\b{}\b",
            regex::escape(&rope.slice(word.clone()).to_string())
        );
        let query = SearchQuery {
            pattern,
            regex: true,
            case_sensitive: true,
        };
        self.ctx.last_search = Some(LastSearch { query, forward });
        let next = Motion::SearchNext { reverse: false };
        if let Some(r) = self.motion(&next, Some(count), word.start, env) {
            env.set_caret(r.pos);
        }
    }
}

/// `cw` on a non-blank changes to the end of the word (vim special case); `None` otherwise.
fn change_word_range(rope: &ropey::Rope, pos: usize, count: usize, big: bool) -> Option<OpRange> {
    if pos >= rope.len_chars() {
        return None;
    }
    let class = char_class(rope.char(pos), big);
    if class == CharClass::Whitespace {
        return None;
    }
    let mut end = pos;
    while end + 1 < rope.len_chars() && char_class(rope.char(end + 1), big) == class {
        end += 1;
    }
    let end = (1..count).fold(end, |p, _| text_nav::word_end(rope, p, big));
    Some(OpRange::Chars(pos..end + 1))
}

/// `r{c}`: replaces `count` chars from `pos`; fails when the line is too short.
fn replace_chars(env: &mut Env, pos: usize, c: char, count: usize) {
    let end = text_nav::line_end(env.buf.rope(), pos);
    if pos + count > end {
        return;
    }
    env.buf
        .replace(pos..pos + count, &c.to_string().repeat(count));
    env.set_caret(pos + count - 1);
}

/// `J`: joins `count` lines (at least two), separating them with one space.
fn join_lines(env: &mut Env, pos: usize, count: usize) {
    // Each join pulls the next line into this one.
    let line = env.buf.rope().char_to_line(pos);
    for _ in 1..count.max(2) {
        let rope = env.buf.rope();
        if line >= last_line(rope) {
            break;
        }
        let end = line_end_of(rope, line);
        let next_end = line_end_of(rope, line + 1);
        let mut skip = end + 1;
        while skip < next_end && matches!(rope.char(skip), ' ' | '\t') {
            skip += 1;
        }
        let line_empty = end == rope.line_to_char(line);
        let ends_blank = end > 0 && matches!(rope.char(end - 1), ' ' | '\t');
        let next_empty = skip == next_end;
        let sep = if line_empty || ends_blank || next_empty || rope.char(skip) == ')' {
            ""
        } else {
            " "
        };
        env.buf.replace(end..skip, sep);
        env.set_caret(end);
    }
}
