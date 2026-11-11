//! Word tokenizing and selection of the text regions that should be spell checked.

use std::ops::Range;

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};

use crate::highlight::{SpanKind, StyledSpan};

/// Shortest run of letters that is checked.
const MIN_WORD_CHARS: usize = 2;
/// ALL-CAPS runs up to this length are treated as acronyms and skipped.
const MAX_ACRONYM_CHARS: usize = 5;

/// True for the characters that separate words but are part of identifiers or numbers
/// when glued to letters: letters, digits and `_`.
fn is_run_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when a run of chars should be spell checked (not an identifier, number, acronym or
/// camelCase word).
fn is_checkable(run: &[char]) -> bool {
    if run.len() < MIN_WORD_CHARS {
        return false;
    }
    if run.iter().any(|c| c.is_numeric() || *c == '_') {
        return false;
    }
    let all_caps = run.iter().all(|c| c.is_uppercase());
    if all_caps {
        return run.len() > MAX_ACRONYM_CHARS;
    }
    !run[1..].iter().any(|c| c.is_uppercase())
}

/// One spell checked token: a word, or an apostrophe joined group of words ("don't").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Range of the whole `letters'letters` group; `None` for a single word.
    pub whole: Option<Range<usize>>,
    /// Checkable parts when the whole group is not accepted (the word itself for a
    /// single word, the elision-split pieces such as "albero" in "l'albero" otherwise).
    pub parts: Vec<Range<usize>>,
}

/// True for the apostrophes that join or split words.
fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '\u{2019}'
}

/// Returns the char ranges of bare URLs (`http://`, `https://`, `www.`, up to whitespace)
/// and HTML entities (`&name;`), which are never spell checked.
fn skip_ranges(chars: &[char]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let starts_with = |i: usize, p: &str| {
        p.chars().enumerate().all(|(k, c)| {
            chars
                .get(i + k)
                .is_some_and(|x| x.to_ascii_lowercase() == c)
        })
    };
    let mut i = 0;
    while i < chars.len() {
        let boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        let url = boundary
            && (starts_with(i, "http://") || starts_with(i, "https://") || starts_with(i, "www."));
        if url {
            let end = chars[i..]
                .iter()
                .position(|c| c.is_whitespace())
                .map_or(chars.len(), |n| i + n);
            out.push(i..end);
            i = end;
            continue;
        }
        if chars[i] == '&' {
            let body = chars[i + 1..]
                .iter()
                .take_while(|c| c.is_alphanumeric() || **c == '#')
                .count();
            if body > 0 && chars.get(i + 1 + body) == Some(&';') {
                out.push(i..i + body + 2);
                i += body + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Returns the checkable tokens of `text`. Words are runs of letters; punctuation splits
/// them, and `letters'letters` groups keep their whole range so the caller can try the
/// contraction first and the elision split second. Runs with digits or `_`, short ALL-CAPS
/// acronyms, camelCase words, bare URLs and HTML entities are skipped.
pub fn tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let skip = skip_ranges(&chars);
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some(r) = skip.iter().find(|r| r.contains(&i)) {
            i = r.end;
            continue;
        }
        if !is_run_char(chars[i]) {
            i += 1;
            continue;
        }
        let mut runs: Vec<Range<usize>> = Vec::new();
        loop {
            let start = i;
            while i < chars.len() && is_run_char(chars[i]) {
                i += 1;
            }
            runs.push(start..i);
            let joined = i + 1 < chars.len()
                && is_apostrophe(chars[i])
                && is_run_char(chars[i + 1])
                && !skip.iter().any(|r| r.contains(&(i + 1)));
            if !joined {
                break;
            }
            i += 1;
        }
        let parts: Vec<Range<usize>> = runs
            .iter()
            .filter(|r| is_checkable(&chars[r.start..r.end]))
            .cloned()
            .collect();
        let pure = runs
            .iter()
            .all(|r| chars[r.clone()].iter().all(|c| c.is_alphabetic()));
        let whole = (runs.len() > 1 && pure).then(|| runs[0].start..runs[runs.len() - 1].end);
        if whole.is_some() || !parts.is_empty() {
            out.push(Token { whole, parts });
        }
    }
    out
}

/// Returns the char ranges of the words of `text` (the split parts of every token).
pub fn words(text: &str) -> Vec<Range<usize>> {
    tokens(text).into_iter().flat_map(|t| t.parts).collect()
}

/// Returns the tokens inside `ranges` (sorted char ranges of `text`), with ranges relative
/// to `text`.
pub fn tokens_in_ranges(text: &str, ranges: &[Range<usize>]) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let shift = |r: Range<usize>, by: usize| r.start + by..r.end + by;
    let mut out = Vec::new();
    for r in ranges {
        let (start, end) = (r.start.min(chars.len()), r.end.min(chars.len()));
        if start >= end {
            continue;
        }
        let piece: String = chars[start..end].iter().collect();
        out.extend(tokens(&piece).into_iter().map(|t| Token {
            whole: t.whole.map(|w| shift(w, start)),
            parts: t.parts.into_iter().map(|p| shift(p, start)).collect(),
        }));
    }
    out
}

/// Returns the merged char ranges of Comment and String spans (the only code regions that
/// are spell checked).
pub fn code_checkable(spans: &[StyledSpan]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for s in spans
        .iter()
        .filter(|s| matches!(s.kind, SpanKind::Comment | SpanKind::String))
    {
        match out.last_mut() {
            Some(last) if last.end == s.range.start => last.end = s.range.end,
            _ => out.push(s.range.clone()),
        }
    }
    out
}

/// Converts increasing byte offsets of a string to char offsets without rescanning.
struct ByteToChar<'a> {
    /// The text being measured.
    text: &'a str,
    /// Last byte offset converted.
    byte: usize,
    /// Char offset of `byte`.
    chars: usize,
}

impl<'a> ByteToChar<'a> {
    /// Creates a converter positioned at the start of `text`.
    fn new(text: &'a str) -> ByteToChar<'a> {
        ByteToChar {
            text,
            byte: 0,
            chars: 0,
        }
    }

    /// Returns the char offset of byte offset `to` (any order; going back rescans).
    fn convert(&mut self, to: usize) -> usize {
        if to < self.byte {
            self.byte = 0;
            self.chars = 0;
        }
        self.chars += self.text[self.byte..to].chars().count();
        self.byte = to;
        self.chars
    }
}

/// Returns the char ranges of prose in a Markdown document: text outside code blocks,
/// front matter, link URLs and autolinks (link text and image alt text are included).
pub fn markdown_checkable(text: &str) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut conv = ByteToChar::new(text);
    let mut skip_depth = 0usize;
    for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_) | Tag::MetadataBlock(_)) => skip_depth += 1,
            Event::Start(Tag::Link {
                link_type: LinkType::Autolink | LinkType::Email,
                ..
            }) => skip_depth += 1,
            Event::End(TagEnd::CodeBlock | TagEnd::MetadataBlock(_) | TagEnd::Link)
                if skip_depth > 0 =>
            {
                skip_depth -= 1;
            }
            Event::Text(_) if skip_depth == 0 => {
                let (start, end) = (conv.convert(range.start), conv.convert(range.end));
                match out.last_mut() {
                    Some(last) if last.end == start => last.end = end,
                    _ => out.push(start..end),
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;

    /// Returns the words of `text` as strings.
    fn word_strings(text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        words(text)
            .into_iter()
            .map(|r| chars[r].iter().collect())
            .collect()
    }

    /// Builds a span of `kind` over `range`.
    fn span(range: Range<usize>, kind: SpanKind) -> StyledSpan {
        StyledSpan {
            range,
            color: Color32::WHITE,
            italic: false,
            bold: false,
            underline: false,
            kind,
        }
    }

    #[test]
    fn words_split_apostrophe() {
        assert_eq!(word_strings("l'albero"), vec!["albero"]);
        assert_eq!(word_strings("un'altra casa"), vec!["un", "altra", "casa"]);
        assert_eq!(word_strings("l\u{2019}albero"), vec!["albero"]);
    }

    #[test]
    fn tokens_keep_whole_contraction_range() {
        let t = tokens("don't l'albero x");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].whole, Some(0..5));
        assert_eq!(t[0].parts, vec![0..3]);
        assert_eq!(t[1].whole, Some(6..14));
        assert_eq!(t[1].parts, vec![8..14]);
        assert_eq!(tokens("hello")[0].whole, None);
    }

    #[test]
    fn words_skip_urls_and_entities() {
        assert_eq!(
            word_strings(
                "see https://exmple.com/pagina_x, www.exmple.org and (http://abcdef.it) &nbsp; &amp;done wrld"
            ),
            vec!["see", "and", "done", "wrld"]
        );
    }

    #[test]
    fn words_skip_identifiers_and_numbers() {
        assert_eq!(
            word_strings("foo_bar abc123 42 camelCase HTTP URLS hello WORLDS x"),
            vec!["hello", "WORLDS"]
        );
    }

    #[test]
    fn words_multibyte_ranges() {
        assert_eq!(words("perché così"), vec![0..6, 7..11]);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn tokens_in_ranges_offsets_are_relative_to_text() {
        let text = "let ciao = 1; // hello wrld";
        let start = text.find("hello").unwrap();
        let ranges = vec![start..text.len()];
        let got: Vec<_> = tokens_in_ranges(text, &ranges)
            .into_iter()
            .flat_map(|t| t.parts)
            .collect();
        assert_eq!(got, vec![17..22, 23..27]);
    }

    #[test]
    fn code_checkable_merges_comment_and_string_spans() {
        let spans = vec![
            span(0..3, SpanKind::Other),
            span(3..6, SpanKind::String),
            span(6..9, SpanKind::String),
            span(9..10, SpanKind::Other),
            span(10..14, SpanKind::Comment),
            span(14..20, SpanKind::Comment),
        ];
        assert_eq!(code_checkable(&spans), vec![3..9, 10..20]);
    }

    #[test]
    fn markdown_excludes_code() {
        let text = "Hello wrld `inlinecode` and [link text](http://exmple.com/xyzzy)\n\n```\nfencedcode here\n```\n\n<http://autolink.example>\n";
        let chars: Vec<char> = text.chars().collect();
        let got: String = markdown_checkable(text)
            .into_iter()
            .map(|r| chars[r].iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("|");
        assert!(got.contains("Hello wrld"), "{got}");
        assert!(got.contains("link text"), "{got}");
        for banned in ["inlinecode", "exmple", "xyzzy", "fencedcode", "autolink"] {
            assert!(!got.contains(banned), "{banned} in {got}");
        }
    }

    #[test]
    fn markdown_ranges_are_char_offsets() {
        let text = "è così\n\nperché";
        let ranges = markdown_checkable(text);
        let chars: Vec<char> = text.chars().collect();
        let got: Vec<String> = ranges
            .into_iter()
            .map(|r| chars[r].iter().collect())
            .collect();
        assert_eq!(got, vec!["è così", "perché"]);
    }
}
