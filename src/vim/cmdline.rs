//! Ex command line (`:`): parser for the supported commands and their executor.

use std::ops::Range;
use std::path::PathBuf;

use crate::buffer::Buffer;
use crate::editing::{EditorState, Selection};
use crate::search::{self, SearchError, SearchQuery};
use crate::text_nav;

use super::VimEffect;
use super::motion::last_line;

/// Error for a write or quit given without its required argument.
const NO_FILE_NAME: &str = "E32: No file name";
/// Error for a range given to a command that does not take one.
const NO_RANGE_ALLOWED: &str = "E481: No range allowed";
/// Error for a `!` given to a command that does not take one.
const NO_BANG_ALLOWED: &str = "E477: No ! allowed";
/// Error for an address outside the buffer.
const INVALID_RANGE: &str = "E16: Invalid range";
/// Error for a range whose start is after its end.
const BACKWARDS_RANGE: &str = "backwards range";
/// Error for a `'x` address naming a mark that is not set.
const MARK_NOT_SET: &str = "E20: Mark not set";

/// A line address of an ex range.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Address {
    /// A 1-based line number.
    Line(usize),
    /// The caret line (`.`).
    Current,
    /// The last line (`$`).
    Last,
    /// The line of a mark (`'a`, `'<`, `'>`).
    Mark(char),
}

/// An inclusive range of line addresses.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct LineRange {
    /// First line.
    pub start: Address,
    /// Last line (inclusive).
    pub end: Address,
}

/// A parsed ex command.
#[derive(Debug, PartialEq)]
pub enum ExCommand {
    /// `:w`, `:w {file}`.
    Write(Option<PathBuf>),
    /// `:w! {file}`: write a copy even when the file exists.
    ForceWrite(PathBuf),
    /// `:q`, `:q!`.
    Quit {
        /// Discard unsaved changes (`!`).
        force: bool,
    },
    /// `:wq`.
    WriteQuit,
    /// `:x`: write only when modified, then close.
    Exit,
    /// `:e {file}`.
    Edit(PathBuf),
    /// `:{n}`, `:$`, `:'a`: jump to a line.
    Goto(Address),
    /// `:[range]s/pattern/replacement/[g][i]`.
    Substitute {
        /// Lines to act on; `None` is the caret line.
        range: Option<LineRange>,
        /// Rust regex pattern.
        pattern: String,
        /// Replacement in `regex` syntax (`$1`).
        replacement: String,
        /// Replace every match of a line, not only the first.
        global: bool,
        /// Match case insensitively.
        ignore_case: bool,
    },
}

/// Parses an ex command line given without its leading `:`.
pub fn parse(input: &str) -> Result<ExCommand, String> {
    let input = input.trim_start();
    let (range, rest) = parse_range(input)?;
    let rest = rest.trim_start();
    if rest.is_empty() {
        let Some(range) = range else {
            return Err(not_an_editor_command(input));
        };
        return Ok(ExCommand::Goto(range.end));
    }
    if let Some(after) = rest.strip_prefix('s')
        && after.chars().next().is_some_and(is_delimiter)
    {
        return parse_substitute(range, after);
    }
    let name_len = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let (name, tail) = rest.split_at(name_len);
    let (bang, tail) = match tail.strip_prefix('!') {
        Some(t) => (true, t),
        None => (false, tail),
    };
    if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
        return Err(not_an_editor_command(input));
    }
    let arg = tail.trim();
    let cmd = match name {
        "w" | "write" if arg.is_empty() => ExCommand::Write(None),
        "w" | "write" if bang => ExCommand::ForceWrite(PathBuf::from(arg)),
        "w" | "write" => ExCommand::Write(Some(PathBuf::from(arg))),
        "q" | "quit" if arg.is_empty() => ExCommand::Quit { force: bang },
        "wq" if arg.is_empty() => ExCommand::WriteQuit,
        "x" | "xit" if arg.is_empty() => ExCommand::Exit,
        "e" | "edit" if arg.is_empty() => return Err(NO_FILE_NAME.to_string()),
        "e" | "edit" => ExCommand::Edit(PathBuf::from(arg)),
        "q" | "quit" | "wq" | "x" | "xit" => return Err("E488: Trailing characters".into()),
        _ => return Err(not_an_editor_command(input)),
    };
    if range.is_some() {
        return Err(NO_RANGE_ALLOWED.to_string());
    }
    // `:w!` without a file is a plain write.
    let bang_allowed = matches!(
        cmd,
        ExCommand::Quit { .. } | ExCommand::Write(None) | ExCommand::ForceWrite(_)
    );
    if bang && !bang_allowed {
        return Err(NO_BANG_ALLOWED.to_string());
    }
    Ok(cmd)
}

/// Error message for an unknown command.
fn not_an_editor_command(input: &str) -> String {
    format!("E492: not an editor command: {input}")
}

/// True when `c` may delimit the parts of a substitute command.
fn is_delimiter(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && c != '\\' && c != '"' && c != '|'
}

/// Parses an optional leading range (`%`, `a`, `a,b`); returns it and the remaining text.
fn parse_range(input: &str) -> Result<(Option<LineRange>, &str), String> {
    if let Some(rest) = input.strip_prefix('%') {
        let all = LineRange {
            start: Address::Line(1),
            end: Address::Last,
        };
        return Ok((Some(all), rest));
    }
    let Some((start, rest)) = parse_address(input)? else {
        return Ok((None, input));
    };
    let Some(after_comma) = rest.strip_prefix(',') else {
        return Ok((Some(LineRange { start, end: start }), rest));
    };
    let Some((end, rest)) = parse_address(after_comma)? else {
        return Err(INVALID_RANGE.to_string());
    };
    Ok((Some(LineRange { start, end }), rest))
}

/// Parses one address at the start of `input`; returns it and the remaining text.
fn parse_address(input: &str) -> Result<Option<(Address, &str)>, String> {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return Ok(None);
    };
    match first {
        '.' => Ok(Some((Address::Current, chars.as_str()))),
        '$' => Ok(Some((Address::Last, chars.as_str()))),
        '\'' => match chars.next() {
            Some(c) if c.is_ascii_lowercase() || c == '<' || c == '>' => {
                Ok(Some((Address::Mark(c), chars.as_str())))
            }
            _ => Err(MARK_NOT_SET.to_string()),
        },
        c if c.is_ascii_digit() => {
            let digits = input
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(input.len());
            let n = input[..digits]
                .parse()
                .map_err(|_| INVALID_RANGE.to_string())?;
            Ok(Some((Address::Line(n), &input[digits..])))
        }
        _ => Ok(None),
    }
}

/// Parses the part of a substitute after `s`: `{d}pattern{d}replacement{d}flags`.
fn parse_substitute(range: Option<LineRange>, input: &str) -> Result<ExCommand, String> {
    let mut chars = input.chars();
    let delim = chars.next().expect("caller checked the delimiter");
    let (pattern, rest) = split_part(chars.as_str(), delim);
    let (replacement, flags) = match rest {
        Some(rest) => {
            let (replacement, flags) = split_part(rest, delim);
            (replacement, flags.unwrap_or(""))
        }
        None => (String::new(), ""),
    };
    let (mut global, mut ignore_case) = (false, false);
    for flag in flags.trim_end().chars() {
        match flag {
            'g' => global = true,
            'i' => ignore_case = true,
            _ => return Err("E488: Trailing characters".to_string()),
        }
    }
    Ok(ExCommand::Substitute {
        range,
        pattern,
        replacement,
        global,
        ignore_case,
    })
}

/// Splits `input` at the first `delim` not escaped by `\`. Returns the text before it, with
/// `\{delim}` turned into `{delim}` (other escapes kept), and the text after it, if present.
fn split_part(input: &str, delim: char) -> (String, Option<&str>) {
    let mut part = String::new();
    let mut chars = input.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == delim {
            return (part, Some(&input[i + c.len_utf8()..]));
        }
        if c != '\\' {
            part.push(c);
            continue;
        }
        match chars.next() {
            Some((_, next)) if next == delim => part.push(next),
            Some((_, next)) => {
                part.push('\\');
                part.push(next);
            }
            None => part.push('\\'),
        }
    }
    (part, None)
}

/// Resolves `range` to 0-based, end-exclusive lines. `None` is the caret line; marks
/// (including `'<` and `'>`, set when Visual mode ends) are buffer marks, which follow edits.
pub fn resolve_range(
    range: Option<LineRange>,
    buf: &Buffer,
    cursor_line: usize,
) -> Result<Range<usize>, String> {
    let Some(range) = range else {
        return Ok(cursor_line..cursor_line + 1);
    };
    let start = resolve_address(range.start, buf, cursor_line)?;
    let end = resolve_address(range.end, buf, cursor_line)?;
    if start > end {
        return Err(BACKWARDS_RANGE.to_string());
    }
    Ok(start..end + 1)
}

/// Resolves one address to a 0-based line.
fn resolve_address(addr: Address, buf: &Buffer, cursor_line: usize) -> Result<usize, String> {
    let rope = buf.rope();
    let last = last_line(rope);
    match addr {
        Address::Line(n) if n > last + 1 => Err(INVALID_RANGE.to_string()),
        Address::Line(n) => Ok(n.saturating_sub(1)),
        Address::Current => Ok(cursor_line.min(last)),
        Address::Last => Ok(last),
        Address::Mark(c) => {
            let pos = buf.mark(c).ok_or_else(|| MARK_NOT_SET.to_string())?;
            Ok(rope.char_to_line(pos.min(rope.len_chars())).min(last))
        }
    }
}

/// Executes `cmd` on the buffer; file and tab commands become effects for the UI.
pub fn execute(cmd: ExCommand, buf: &mut Buffer, ed: &mut EditorState) -> Vec<VimEffect> {
    let head = ed.selection.head.min(buf.rope().len_chars());
    let cursor_line = buf.rope().char_to_line(head);
    match cmd {
        ExCommand::Write(path) => vec![VimEffect::Write(path)],
        ExCommand::Quit { force } => vec![VimEffect::Quit { force }],
        ExCommand::ForceWrite(path) => vec![VimEffect::ForceWrite(path)],
        ExCommand::WriteQuit => vec![VimEffect::WriteQuit],
        ExCommand::Exit if buf.is_modified() => vec![VimEffect::WriteQuit],
        ExCommand::Exit => vec![VimEffect::Quit { force: false }],
        ExCommand::Edit(path) => vec![VimEffect::Edit(path)],
        ExCommand::Goto(addr) => {
            // A line past the end jumps to the last line, like vim.
            let addr = match addr {
                Address::Line(n) => Address::Line(n.clamp(1, last_line(buf.rope()) + 1)),
                other => other,
            };
            match resolve_address(addr, buf, cursor_line) {
                Ok(line) => {
                    caret_to_first_non_blank(buf, ed, line);
                    Vec::new()
                }
                Err(e) => vec![VimEffect::Error(e)],
            }
        }
        ExCommand::Substitute {
            range,
            pattern,
            replacement,
            global,
            ignore_case,
        } => {
            let lines = match resolve_range(range, buf, cursor_line) {
                Ok(lines) => lines,
                Err(e) => return vec![VimEffect::Error(e)],
            };
            let query = SearchQuery {
                pattern,
                regex: true,
                case_sensitive: !ignore_case,
            };
            substitute_lines(buf, ed, &query, &replacement, lines, global)
        }
    }
}

/// Runs a substitution over `lines` and reports the count or the error.
fn substitute_lines(
    buf: &mut Buffer,
    ed: &mut EditorState,
    query: &SearchQuery,
    replacement: &str,
    lines: Range<usize>,
    global: bool,
) -> Vec<VimEffect> {
    if query.pattern.is_empty() {
        return vec![VimEffect::Error(
            "E35: No previous regular expression".into(),
        )];
    }
    let re = match query.compile() {
        Ok(re) => re,
        Err(e) => return vec![VimEffect::Error(e.to_string())],
    };
    match search::substitute(buf, &re, replacement, lines, global) {
        Ok(done) => {
            // Like vim, the caret ends on the last line that had a match.
            caret_to_first_non_blank(buf, ed, done.last_line);
            let n = done.count;
            let plural = if n == 1 { "" } else { "s" };
            vec![VimEffect::Message(format!("{n} substitution{plural}"))]
        }
        Err(SearchError::NotFound) => vec![VimEffect::Error(format!(
            "pattern not found: {}",
            query.pattern
        ))],
        Err(e) => vec![VimEffect::Error(e.to_string())],
    }
}

/// Puts the caret on the first non-blank char of `line`.
fn caret_to_first_non_blank(buf: &Buffer, ed: &mut EditorState, line: usize) {
    let rope = buf.rope();
    let start = rope.line_to_char(line.min(rope.len_lines().saturating_sub(1)));
    ed.selection = Selection::caret(text_nav::first_non_blank(rope, start));
    ed.preferred_col = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a substitute command for comparisons.
    fn subst(range: Option<LineRange>, p: &str, r: &str, global: bool, ic: bool) -> ExCommand {
        ExCommand::Substitute {
            range,
            pattern: p.into(),
            replacement: r.into(),
            global,
            ignore_case: ic,
        }
    }

    /// Range helper.
    fn lr(start: Address, end: Address) -> Option<LineRange> {
        Some(LineRange { start, end })
    }

    #[test]
    fn parse_write_quit_variants() {
        assert_eq!(parse("w"), Ok(ExCommand::Write(None)));
        assert_eq!(
            parse("w out.txt"),
            Ok(ExCommand::Write(Some(PathBuf::from("out.txt"))))
        );
        assert_eq!(parse("q"), Ok(ExCommand::Quit { force: false }));
        assert_eq!(parse("q!"), Ok(ExCommand::Quit { force: true }));
        assert_eq!(parse("wq"), Ok(ExCommand::WriteQuit));
        assert_eq!(parse("x"), Ok(ExCommand::Exit));
        assert_eq!(parse("w!"), Ok(ExCommand::Write(None)));
        assert_eq!(
            parse("w! out.txt"),
            Ok(ExCommand::ForceWrite(PathBuf::from("out.txt")))
        );
        assert!(parse("wq!").is_err());
        assert_eq!(
            parse("e other.txt"),
            Ok(ExCommand::Edit(PathBuf::from("other.txt")))
        );
        assert_eq!(parse("e"), Err(NO_FILE_NAME.to_string()));
        assert!(parse("1,2w").is_err());
    }

    #[test]
    fn parse_substitute_full_range() {
        assert_eq!(
            parse("1,$s/aaa/bbb/"),
            Ok(subst(
                lr(Address::Line(1), Address::Last),
                "aaa",
                "bbb",
                false,
                false
            ))
        );
        assert_eq!(
            parse(".,3s/a/b"),
            Ok(subst(
                lr(Address::Current, Address::Line(3)),
                "a",
                "b",
                false,
                false
            ))
        );
        assert_eq!(
            parse("s/a/"),
            Ok(subst(None, "a", "", false, false)),
            "empty replacement"
        );
        assert_eq!(parse("s/a"), Ok(subst(None, "a", "", false, false)));
    }

    #[test]
    fn parse_substitute_percent_global_ignorecase() {
        assert_eq!(
            parse("%s/x/y/gi"),
            Ok(subst(
                lr(Address::Line(1), Address::Last),
                "x",
                "y",
                true,
                true
            ))
        );
        assert!(parse("%s/x/y/z").is_err());
    }

    #[test]
    fn parse_substitute_custom_delimiter_and_escape() {
        assert_eq!(parse("s#a/b#c#g"), Ok(subst(None, "a/b", "c", true, false)));
        assert_eq!(
            parse("s/a\\/b/c/"),
            Ok(subst(None, "a/b", "c", false, false))
        );
        assert_eq!(
            parse("s/a\\d/\\//"),
            Ok(subst(None, "a\\d", "/", false, false)),
            "other escapes are kept for the regex"
        );
    }

    #[test]
    fn parse_visual_range() {
        assert_eq!(
            parse("'<,'>s/a/b/"),
            Ok(subst(
                lr(Address::Mark('<'), Address::Mark('>')),
                "a",
                "b",
                false,
                false
            ))
        );
        assert_eq!(
            parse("'a,'bs/a/b/"),
            Ok(subst(
                lr(Address::Mark('a'), Address::Mark('b')),
                "a",
                "b",
                false,
                false
            ))
        );
    }

    #[test]
    fn parse_goto_line() {
        assert_eq!(parse("12"), Ok(ExCommand::Goto(Address::Line(12))));
        assert_eq!(parse("$"), Ok(ExCommand::Goto(Address::Last)));
        assert_eq!(parse("'a"), Ok(ExCommand::Goto(Address::Mark('a'))));
    }

    #[test]
    fn parse_rejects_unknown() {
        let err = parse("foo").unwrap_err();
        assert!(err.contains("not an editor command"), "{err}");
        assert!(parse("wfoo").is_err());
    }

    #[test]
    fn resolve_backwards_range_errors() {
        let buf = Buffer::from_text("a\nb\nc\n");
        let range = lr(Address::Line(3), Address::Line(1));
        assert_eq!(
            resolve_range(range, &buf, 0),
            Err("backwards range".to_string())
        );
        assert_eq!(resolve_range(None, &buf, 1), Ok(1..2));
        let all = lr(Address::Line(1), Address::Last);
        assert_eq!(resolve_range(all, &buf, 0), Ok(0..3));
        let visual = lr(Address::Mark('<'), Address::Mark('>'));
        assert!(resolve_range(visual, &buf, 0).is_err(), "marks not set yet");
        let mut buf = buf;
        buf.set_mark('<', 2);
        buf.set_mark('>', 4);
        assert_eq!(resolve_range(visual, &buf, 0), Ok(1..3));
        let too_far = lr(Address::Line(1), Address::Line(9));
        assert!(resolve_range(too_far, &buf, 0).is_err());
    }

    #[test]
    fn resolve_buffer_mark() {
        let mut buf = Buffer::from_text("a\nb\nc\n");
        buf.set_mark('a', 2);
        let range = lr(Address::Mark('a'), Address::Last);
        assert_eq!(resolve_range(range, &buf, 0), Ok(1..3));
        let missing = lr(Address::Mark('b'), Address::Last);
        assert!(resolve_range(missing, &buf, 0).is_err());
    }

    #[test]
    fn substitute_invalid_regex_reports_error() {
        let mut buf = Buffer::from_text("abc\n");
        let mut ed = EditorState::default();
        let cmd = parse("s/(/x/").unwrap();
        let fx = execute(cmd, &mut buf, &mut ed);
        assert!(matches!(&fx[..], [VimEffect::Error(e)] if e.contains("invalid pattern")));
        assert_eq!(buf.text(), "abc\n");
    }

    #[test]
    fn execute_substitute_reports_count() {
        let mut buf = Buffer::from_text("aaa x\nbbb\naaa aaa\n");
        let mut ed = EditorState::default();
        let fx = execute(parse("1,$s/aaa/bbb/").unwrap(), &mut buf, &mut ed);
        assert_eq!(fx, vec![VimEffect::Message("2 substitutions".into())]);
        assert_eq!(buf.text(), "bbb x\nbbb\nbbb aaa\n");
        assert_eq!(
            ed.selection,
            Selection::caret(10),
            "caret on the last changed line"
        );
        assert!(buf.undo().is_some(), "one undo step");
        assert_eq!(buf.text(), "aaa x\nbbb\naaa aaa\n");

        let fx = execute(parse("%s/a/y/g").unwrap(), &mut buf, &mut ed);
        assert_eq!(fx, vec![VimEffect::Message("9 substitutions".into())]);

        let fx = execute(parse("s/zzz/q/").unwrap(), &mut buf, &mut ed);
        assert_eq!(fx, vec![VimEffect::Error("pattern not found: zzz".into())]);
    }

    #[test]
    fn execute_goto_moves_to_first_non_blank() {
        let mut buf = Buffer::from_text("a\n  b\nc\n");
        let mut ed = EditorState::default();
        assert!(execute(parse("2").unwrap(), &mut buf, &mut ed).is_empty());
        assert_eq!(ed.selection, Selection::caret(4));
        execute(parse("99").unwrap(), &mut buf, &mut ed);
        assert_eq!(ed.selection, Selection::caret(6));
        execute(parse("0").unwrap(), &mut buf, &mut ed);
        assert_eq!(ed.selection, Selection::caret(0));
    }

    #[test]
    fn exit_writes_only_when_modified() {
        let mut buf = Buffer::from_text("a\n");
        let mut ed = EditorState::default();
        let fx = execute(ExCommand::Exit, &mut buf, &mut ed);
        assert_eq!(fx, vec![VimEffect::Quit { force: false }]);
        buf.replace(0..0, "x");
        let fx = execute(ExCommand::Exit, &mut buf, &mut ed);
        assert_eq!(fx, vec![VimEffect::WriteQuit]);
    }

    #[test]
    fn quit_effect_on_modified_buffer() {
        let mut buf = Buffer::from_text("a\n");
        buf.replace(0..0, "x");
        assert!(buf.is_modified());
        let mut ed = EditorState::default();
        let fx = execute(parse("q").unwrap(), &mut buf, &mut ed);
        assert_eq!(fx, vec![VimEffect::Quit { force: false }]);
    }
}
