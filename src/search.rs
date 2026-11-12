//! Search engine shared by the find bar and the vim `/` and `:s` commands.

use std::ops::Range;

use regex::{Regex, RegexBuilder};
use ropey::Rope;

use crate::buffer::{Buffer, DirtyLines};
use crate::text_nav;

/// A user search request: pattern text plus matching options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// Pattern text, a regex when `regex` is true, otherwise a literal string.
    pub pattern: String,
    /// Interpret `pattern` as a regular expression.
    pub regex: bool,
    /// Match case exactly.
    pub case_sensitive: bool,
}

/// Errors produced by the search engine.
#[derive(thiserror::Error, Debug)]
pub enum SearchError {
    /// The pattern is not a valid regular expression.
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),
    /// Nothing matched.
    #[error("pattern not found")]
    NotFound,
}

impl SearchQuery {
    /// Compiles the query into a [`Regex`]; literal patterns are escaped first.
    pub fn compile(&self) -> Result<Regex, SearchError> {
        let source = if self.regex {
            self.pattern.clone()
        } else {
            regex::escape(&self.pattern)
        };
        RegexBuilder::new(&source)
            .case_insensitive(!self.case_sensitive)
            .build()
            .map_err(|e| SearchError::InvalidPattern(e.to_string()))
    }
}

/// Returns the non-empty matches of line `index` as document char ranges, in order.
fn line_matches(rope: &Rope, re: &Regex, index: usize) -> Vec<Range<usize>> {
    let text = text_nav::line_text(rope, index);
    let mut char_pos = rope.line_to_char(index);
    let mut byte_pos = 0;
    let mut found = Vec::new();
    for m in re.find_iter(&text) {
        if m.is_empty() {
            continue;
        }
        char_pos += text[byte_pos..m.start()].chars().count();
        let len = m.as_str().chars().count();
        found.push(char_pos..char_pos + len);
        char_pos += len;
        byte_pos = m.end();
    }
    found
}

/// Returns every non-empty match in the rope as a char range, in document order.
pub fn find_all(rope: &Rope, re: &Regex) -> Vec<Range<usize>> {
    (0..rope.len_lines())
        .flat_map(|index| line_matches(rope, re, index))
        .collect()
}

/// Brings `old`, the matches of a previous text of `old_chars` chars, up to date with `rope`
/// after the edits described by `dirty`: matches before the changed lines are kept, the
/// changed lines are searched again and the matches after them are shifted by the length
/// change. Unknown edits (`dirty.old_end == usize::MAX`) search the whole rope.
pub fn rescan_lines(
    old: &[Range<usize>],
    old_chars: usize,
    rope: &Rope,
    re: &Regex,
    dirty: DirtyLines,
) -> Vec<Range<usize>> {
    let lines = rope.len_lines();
    if dirty.old_end == usize::MAX || dirty.start > dirty.end || dirty.end > lines {
        return find_all(rope, re);
    }
    let head_end = rope.line_to_char(dirty.start);
    let tail_new = match dirty.end < lines {
        true => rope.line_to_char(dirty.end),
        false => rope.len_chars(),
    };
    let delta = rope.len_chars() as isize - old_chars as isize;
    let tail_old = (tail_new as isize - delta) as usize;
    let first_tail = old.partition_point(|m| m.start < tail_old);
    let mut out: Vec<Range<usize>> = old
        .iter()
        .take_while(|m| m.start < head_end)
        .cloned()
        .collect();
    out.extend((dirty.start..dirty.end).flat_map(|i| line_matches(rope, re, i)));
    out.extend(old[first_tail..].iter().map(|m| {
        let shift = |p: usize| (p as isize + delta) as usize;
        shift(m.start)..shift(m.end)
    }));
    out
}

/// Finds the next match relative to char index `from`, wrapping around the document.
///
/// Forward returns the first match starting after `from`; backward returns the last
/// match starting before `from`. Lines are scanned outwards from the line of `from` and
/// the scan stops at the first hit, so a nearby match costs only a few lines.
pub fn find_next(rope: &Rope, re: &Regex, from: usize, forward: bool) -> Option<Range<usize>> {
    let lines = rope.len_lines();
    let from_line = rope.char_to_line(from.min(rope.len_chars()));
    if forward {
        let after = (from_line..lines).find_map(|i| {
            line_matches(rope, re, i)
                .into_iter()
                .find(|r| r.start > from)
        });
        // Wrap: the first match of the document (it is at or before `from`).
        return after.or_else(|| {
            (0..=from_line).find_map(|i| line_matches(rope, re, i).into_iter().next())
        });
    }
    let before = (0..=from_line).rev().find_map(|i| {
        line_matches(rope, re, i)
            .into_iter()
            .rev()
            .find(|r| r.start < from)
    });
    // Wrap: the last match of the document (it is at or after `from`).
    before.or_else(|| {
        (from_line..lines)
            .rev()
            .find_map(|i| line_matches(rope, re, i).pop())
    })
}

/// Result of a successful `substitute`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Substitution {
    /// Number of replacements made.
    pub count: usize,
    /// Last line (0-based, before the edit) that had a match.
    pub last_line: usize,
}

/// Rewrites the content of `lines` (end-exclusive, 0-based) and returns the match count and
/// the last line that matched.
///
/// All edits happen inside the caller's undo group; they are applied bottom-up so
/// earlier char offsets stay valid. Lines are borrowed from the rope while matching; only
/// changed lines are copied.
fn rewrite_lines(
    buf: &mut Buffer,
    re: &Regex,
    replacement: &str,
    lines: Range<usize>,
    global: bool,
) -> (usize, Option<usize>) {
    let limit = if global { 0 } else { 1 };
    let end = lines.end.min(buf.rope().len_lines());
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut count = 0;
    let mut last_line = None;
    for index in lines.start..end {
        let rope = buf.rope();
        let text = text_nav::line_text(rope, index);
        let matches = if global {
            re.find_iter(&text).count()
        } else {
            usize::from(re.is_match(&text))
        };
        if matches == 0 {
            continue;
        }
        count += matches;
        last_line = Some(index);
        let new_text = re.replacen(&text, limit, replacement).into_owned();
        let start = rope.line_to_char(index);
        edits.push((start..start + text.chars().count(), new_text));
    }
    for (range, new_text) in edits.into_iter().rev() {
        buf.replace(range, &new_text);
    }
    (count, last_line)
}

/// Substitutes matches within `lines` (end-exclusive, 0-based) as one undo step.
///
/// With `global` false only the first match of each line is replaced. The replacement
/// uses `regex` syntax (`$1`). Returns the number of replacements and the last line that
/// matched, or `NotFound`.
pub fn substitute(
    buf: &mut Buffer,
    re: &Regex,
    replacement: &str,
    lines: Range<usize>,
    global: bool,
) -> Result<Substitution, SearchError> {
    buf.begin_group();
    let (count, last_line) = rewrite_lines(buf, re, replacement, lines, global);
    buf.end_group();
    match last_line {
        Some(last_line) => Ok(Substitution { count, last_line }),
        None => Err(SearchError::NotFound),
    }
}

/// Replaces every match in the whole buffer as one undo step and returns the count.
pub fn replace_all(buf: &mut Buffer, re: &Regex, replacement: &str) -> usize {
    let lines = 0..buf.rope().len_lines();
    buf.begin_group();
    let (count, _) = rewrite_lines(buf, re, replacement, lines, true);
    buf.end_group();
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescan_lines_matches_a_full_search_after_edits() {
        let re = Regex::new("ab").unwrap();
        let mut buf = Buffer::from_text("ab x ab\nno\nab\n\nxab ab\nend ab");
        let mut matches = find_all(buf.rope(), &re);
        let edits: [(Range<usize>, &str); 7] = [
            (0..0, "ab "),
            (10..12, "abab\nab"),
            (5..20, ""),
            (3..3, "\n\n"),
            (0..0, "zz"),
            (8..9, "a"),
            (2..4, "b\nab\nab"),
        ];
        for (range, text) in edits {
            let (rev, chars) = (buf.revision(), buf.rope().len_chars());
            let end = range.end.min(chars);
            buf.replace(range.start.min(end)..end, text);
            let dirty = buf.dirty_lines_since(rev).unwrap();
            matches = rescan_lines(&matches, chars, buf.rope(), &re, dirty);
            assert_eq!(matches, find_all(buf.rope(), &re), "after {text:?}");
        }
    }

    /// Builds a query with the given options.
    fn query(pattern: &str, regex: bool, case_sensitive: bool) -> SearchQuery {
        SearchQuery {
            pattern: pattern.to_string(),
            regex,
            case_sensitive,
        }
    }

    /// Compiles a case-sensitive regex query.
    fn re(pattern: &str) -> Regex {
        query(pattern, true, true).compile().unwrap()
    }

    #[test]
    fn literal_query_escapes_metachars() {
        let re = query("a.b", false, true).compile().unwrap();
        assert!(re.is_match("a.b"));
        assert!(!re.is_match("axb"));
    }

    #[test]
    fn case_insensitive_matches() {
        let re = query("hello", false, false).compile().unwrap();
        assert!(re.is_match("HeLLo"));
    }

    #[test]
    fn find_next_wraps() {
        let rope = Rope::from_str("ab ab ab");
        let re = re("ab");
        assert_eq!(find_next(&rope, &re, 0, true), Some(3..5));
        assert_eq!(find_next(&rope, &re, 3, true), Some(6..8));
        assert_eq!(find_next(&rope, &re, 6, true), Some(0..2));
    }

    #[test]
    fn find_next_backward() {
        let rope = Rope::from_str("ab ab ab");
        let re = re("ab");
        assert_eq!(find_next(&rope, &re, 6, false), Some(3..5));
        assert_eq!(find_next(&rope, &re, 0, false), Some(6..8));
    }

    #[test]
    fn find_next_matches_whole_document_reference() {
        let rope = Rope::from_str("ab x\n\nxx ab ab\nnone\nab");
        let ab = re("ab");
        let all = find_all(&rope, &ab);
        for from in 0..=rope.len_chars() {
            let fwd = all.iter().find(|r| r.start > from).or(all.first()).cloned();
            let back = all
                .iter()
                .rev()
                .find(|r| r.start < from)
                .or(all.last())
                .cloned();
            assert_eq!(
                find_next(&rope, &ab, from, true),
                fwd,
                "forward from {from}"
            );
            assert_eq!(
                find_next(&rope, &ab, from, false),
                back,
                "backward from {from}"
            );
        }
        let none = Rope::from_str("abc\ndef");
        assert_eq!(find_next(&none, &re("z"), 2, true), None);
        assert_eq!(find_next(&none, &re("z"), 2, false), None);
    }

    #[test]
    fn find_all_counts_chars_between_multibyte_matches() {
        let rope = Rope::from_str("è a è a\nè");
        assert_eq!(find_all(&rope, &re("è")), vec![0..1, 4..5, 8..9]);
    }

    #[test]
    fn search_handles_multibyte() {
        let rope = Rope::from_str("perché è così");
        let re = query("è", false, true).compile().unwrap();
        assert_eq!(find_all(&rope, &re), vec![7..8]);
    }

    #[test]
    fn invalid_regex_is_error() {
        let result = query("(", true, true).compile();
        assert!(matches!(result, Err(SearchError::InvalidPattern(_))));
    }

    #[test]
    fn substitute_line_range_global() {
        let mut buf = Buffer::from_text("aa\naa\naa");
        let done = substitute(&mut buf, &re("a"), "b", 1..3, true).unwrap();
        assert_eq!(
            done,
            Substitution {
                count: 4,
                last_line: 2
            }
        );
        assert_eq!(buf.text(), "aa\nbb\nbb");
    }

    #[test]
    fn substitute_first_only() {
        let mut buf = Buffer::from_text("aa\naa\naa");
        let done = substitute(&mut buf, &re("a"), "b", 0..3, false).unwrap();
        assert_eq!(done.count, 3);
        assert_eq!(buf.text(), "ba\nba\nba");
    }

    #[test]
    fn substitute_reports_the_last_matching_line() {
        let mut buf = Buffer::from_text("x\na\nx\na\nx");
        let done = substitute(&mut buf, &re("a"), "b", 0..5, true).unwrap();
        assert_eq!(
            done,
            Substitution {
                count: 2,
                last_line: 3
            }
        );
    }

    #[test]
    fn substitute_capture_groups() {
        let mut buf = Buffer::from_text("joe@x\nann@y");
        substitute(&mut buf, &re(r"(\w+)@"), "[$1]", 0..2, true).unwrap();
        assert_eq!(buf.text(), "[joe]x\n[ann]y");
    }

    #[test]
    fn substitute_single_undo() {
        let mut buf = Buffer::from_text("aa\naa\naa");
        substitute(&mut buf, &re("a"), "b", 0..3, true).unwrap();
        buf.undo();
        assert_eq!(buf.text(), "aa\naa\naa");
    }

    #[test]
    fn substitute_not_found() {
        let mut buf = Buffer::from_text("aa");
        let result = substitute(&mut buf, &re("z"), "b", 0..1, true);
        assert!(matches!(result, Err(SearchError::NotFound)));
        assert_eq!(buf.text(), "aa");
    }

    #[test]
    fn replace_all_whole_buffer_single_undo() {
        let mut buf = Buffer::from_text("aa\naa");
        assert_eq!(replace_all(&mut buf, &re("a"), "b"), 4);
        assert_eq!(buf.text(), "bb\nbb");
        buf.undo();
        assert_eq!(buf.text(), "aa\naa");
    }
}
