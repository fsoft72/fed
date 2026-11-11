//! Search engine shared by the find bar and the vim `/` and `:s` commands.

use std::ops::Range;

use regex::{Regex, RegexBuilder};
use ropey::Rope;

use crate::buffer::Buffer;

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

/// Returns the text of line `index` without its trailing line break.
fn line_content(rope: &Rope, index: usize) -> String {
    let mut text = rope.line(index).to_string();
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Returns every non-empty match in the rope as a char range, in document order.
pub fn find_all(rope: &Rope, re: &Regex) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    for index in 0..rope.len_lines() {
        let text = line_content(rope, index);
        let line_start = rope.line_to_char(index);
        for m in re.find_iter(&text) {
            if m.is_empty() {
                continue;
            }
            let start = line_start + text[..m.start()].chars().count();
            let len = text[m.start()..m.end()].chars().count();
            found.push(start..start + len);
        }
    }
    found
}

/// Finds the next match relative to char index `from`, wrapping around the document.
///
/// Forward returns the first match starting after `from`; backward returns the last
/// match starting before `from`.
pub fn find_next(rope: &Rope, re: &Regex, from: usize, forward: bool) -> Option<Range<usize>> {
    let all = find_all(rope, re);
    if forward {
        let next = all.iter().find(|r| r.start > from).or(all.first());
        return next.cloned();
    }
    let prev = all.iter().rev().find(|r| r.start < from).or(all.last());
    prev.cloned()
}

/// Rewrites the content of `lines` (end-exclusive, 0-based) and returns the match count.
///
/// All edits happen inside the caller's undo group; they are applied bottom-up so
/// earlier char offsets stay valid.
fn rewrite_lines(
    buf: &mut Buffer,
    re: &Regex,
    replacement: &str,
    lines: Range<usize>,
    global: bool,
) -> usize {
    let limit = if global { 0 } else { 1 };
    let end = lines.end.min(buf.rope().len_lines());
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut count = 0;
    for index in lines.start..end {
        let text = line_content(buf.rope(), index);
        let matches = if global {
            re.find_iter(&text).count()
        } else {
            re.find(&text).iter().count()
        };
        if matches == 0 {
            continue;
        }
        count += matches;
        let new_text = re.replacen(&text, limit, replacement).into_owned();
        let start = buf.rope().line_to_char(index);
        edits.push((start..start + text.chars().count(), new_text));
    }
    for (range, new_text) in edits.into_iter().rev() {
        buf.replace(range, &new_text);
    }
    count
}

/// Substitutes matches within `lines` (end-exclusive, 0-based) as one undo step.
///
/// With `global` false only the first match of each line is replaced. The replacement
/// uses `regex` syntax (`$1`). Returns the number of replacements or `NotFound`.
pub fn substitute(
    buf: &mut Buffer,
    re: &Regex,
    replacement: &str,
    lines: Range<usize>,
    global: bool,
) -> Result<usize, SearchError> {
    buf.begin_group();
    let count = rewrite_lines(buf, re, replacement, lines, global);
    buf.end_group();
    if count == 0 {
        return Err(SearchError::NotFound);
    }
    Ok(count)
}

/// Replaces every match in the whole buffer as one undo step and returns the count.
pub fn replace_all(buf: &mut Buffer, re: &Regex, replacement: &str) -> usize {
    let lines = 0..buf.rope().len_lines();
    buf.begin_group();
    let count = rewrite_lines(buf, re, replacement, lines, true);
    buf.end_group();
    count
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let count = substitute(&mut buf, &re("a"), "b", 1..3, true).unwrap();
        assert_eq!(count, 4);
        assert_eq!(buf.text(), "aa\nbb\nbb");
    }

    #[test]
    fn substitute_first_only() {
        let mut buf = Buffer::from_text("aa\naa\naa");
        let count = substitute(&mut buf, &re("a"), "b", 0..3, false).unwrap();
        assert_eq!(count, 3);
        assert_eq!(buf.text(), "ba\nba\nba");
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
