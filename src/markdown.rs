//! Live Markdown preview: path detection, image URL rewriting and the preview panel state.

use std::ops::Range;
use std::path::{Path, PathBuf};

use eframe::egui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::buffer::Buffer;

/// File extensions (lowercase) treated as Markdown.
const MARKDOWN_EXTENSIONS: [&str; 3] = ["md", "markdown", "mdown"];

/// Returns true when `path` has a Markdown extension (case-insensitive).
pub fn is_markdown_path(path: Option<&Path>) -> bool {
    let Some(ext) = path.and_then(Path::extension).and_then(|e| e.to_str()) else {
        return false;
    };
    MARKDOWN_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// URL scheme prefixes (lowercase) that a clicked link may open in the system.
/// `file:` is deliberately absent: opening local files could launch programs
/// (executables, `.desktop` entries) or leak credentials through `file://host/share`.
const OPENABLE_URL_PREFIXES: [&str; 3] = ["http://", "https://", "mailto:"];

/// Returns true when a clicked link may be handed to the system opener: only `http`,
/// `https` and `mailto` URLs (scheme case-insensitive, surrounding whitespace ignored).
/// Anything else is ignored.
pub fn is_openable_url(url: &str) -> bool {
    let url = url.trim();
    OPENABLE_URL_PREFIXES.iter().any(|prefix| {
        url.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })
}

/// Returns true for a URL that would be fetched over the network.
fn is_remote_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Returns true for a URL that already has a scheme (`file:`, `data:`, ...) or is absolute
/// with a scheme-relative prefix.
fn has_scheme(url: &str) -> bool {
    if url.starts_with("//") {
        return true;
    }
    let Some(colon) = url.find(':') else {
        return false;
    };
    // A single letter before the colon is a Windows drive, not a scheme.
    colon > 1
        && url[..colon]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Escapes Markdown punctuation so `text` renders literally.
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Percent-encodes the characters of a path that would end an inline link destination.
fn encode_path(path: &str) -> String {
    path.replace('%', "%25")
        .replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('<', "%3C")
        .replace('>', "%3E")
}

/// Builds the `file://` URL of `dest` resolved against `base_dir`.
fn file_url(base_dir: &Path, dest: &str) -> String {
    let joined = base_dir.join(dest);
    let text = joined.to_string_lossy().replace('\\', "/");
    let encoded = encode_path(&text);
    match encoded.starts_with('/') {
        true => format!("file://{encoded}"),
        false => format!("file:///{encoded}"),
    }
}

/// Rewrites image syntax so the preview never touches the network: remote images become
/// `[alt]` text and relative image paths become absolute `file://` URLs under `base_dir`.
/// Links and other constructs are left untouched.
pub fn rewrite_image_paths(text: &str, base_dir: &Path) -> String {
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut parser = Parser::new_ext(text, Options::all()).into_offset_iter();
    while let Some((event, range)) = parser.next() {
        let Event::Start(Tag::Image { dest_url, .. }) = event else {
            continue;
        };
        let alt = collect_alt(&mut parser);
        if is_remote_url(&dest_url) {
            edits.push((range, format!("\\[{}\\]", escape_markdown(&alt))));
            continue;
        }
        if has_scheme(&dest_url) || dest_url.starts_with('/') || dest_url.is_empty() {
            continue;
        }
        let Some(start) = destination_start(text, range.clone(), &dest_url) else {
            continue;
        };
        edits.push((start..start + dest_url.len(), file_url(base_dir, &dest_url)));
    }
    apply_edits(text, edits)
}

/// Returns the byte offset in `text` where the destination `dest` of the image spanning
/// `range` starts: the first occurrence after the `](` that ends the alt text, optionally
/// wrapped in `<...>`. Searching from the `](` keeps an identical title or alt text from
/// being mistaken for the destination.
fn destination_start(text: &str, range: Range<usize>, dest: &str) -> Option<usize> {
    let image = &text[range.clone()];
    let open = image.find("](")? + 2;
    let offset = image[open..].find(dest)?;
    Some(range.start + open + offset)
}

/// Consumes events up to the end of the current image and returns its plain alt text.
fn collect_alt<'a>(parser: &mut impl Iterator<Item = (Event<'a>, Range<usize>)>) -> String {
    let mut alt = String::new();
    let mut depth = 1usize;
    for (event, _) in parser {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Text(t) | Event::Code(t) => alt.push_str(&t),
            Event::SoftBreak | Event::HardBreak => alt.push(' '),
            _ => {}
        }
    }
    alt
}

/// Applies non-overlapping `edits` (sorted by position) to `text`.
fn apply_edits(text: &str, edits: Vec<(Range<usize>, String)>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for (range, replacement) in edits {
        if range.start < pos {
            continue;
        }
        out.push_str(&text[pos..range.start]);
        out.push_str(&replacement);
        pos = range.end;
    }
    out.push_str(&text[pos..]);
    out
}

/// Returns the absolute directory that relative image paths of the file at `path` resolve
/// against. A relative path (`fed README.md`) is made absolute against the current
/// directory, so the rewritten `file://` URLs never become `file:///img.png`.
fn absolute_base_dir(path: Option<&Path>) -> PathBuf {
    let dir = path
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    if dir.is_absolute() {
        return dir;
    }
    std::path::absolute(&dir).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|cwd| cwd.join(&dir))
            .unwrap_or(dir)
    })
}

/// Rendered-preview state of one tab: the Markdown render cache and the last prepared source.
pub struct Preview {
    /// Render cache of egui_commonmark (images, layout).
    cache: CommonMarkCache,
    /// Buffer revision and the rewritten text prepared for that revision.
    source: (u64, String),
}

impl Default for Preview {
    fn default() -> Self {
        Self::new()
    }
}

impl Preview {
    /// Creates an empty preview that renders on the first `show`.
    pub fn new() -> Preview {
        Preview {
            cache: CommonMarkCache::default(),
            // `u64::MAX` is never a real revision, so the first `show` always prepares.
            source: (u64::MAX, String::new()),
        }
    }

    /// Draws the rendered Markdown of `buf` inside a scroll area. The text is re-read and
    /// image paths are rewritten only when the buffer revision changed.
    pub fn show(&mut self, ui: &mut egui::Ui, buf: &Buffer) {
        if self.source.0 != buf.revision() {
            let base = absolute_base_dir(buf.path());
            self.source = (
                buf.revision(),
                rewrite_image_paths(&buf.rope().to_string(), &base),
            );
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                CommonMarkViewer::new().show(ui, &mut self.cache, &self.source.1);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openable_urls_are_web_and_mail_only() {
        for url in [
            "http://example.com",
            "HTTPS://example.com/a?b",
            "mailto:me@example.com",
            "  https://example.com  ",
        ] {
            assert!(is_openable_url(url), "{url}");
        }
        for url in [
            "",
            "notes.md",
            "../other.md",
            "#heading",
            "javascript:alert(1)",
            "ftp://example.com",
            "C:\\file.txt",
            "smb://server/share",
            "file:///tmp/notes.md",
            "FILE:///usr/bin/xterm",
            "file://host/share/x.exe",
            " file:///etc/passwd",
            "httpx://example.com",
            "é",
        ] {
            assert!(!is_openable_url(url), "{url}");
        }
    }

    #[test]
    fn is_markdown_path_cases() {
        assert!(is_markdown_path(Some(Path::new("a.md"))));
        assert!(is_markdown_path(Some(Path::new("/x/A.MARKDOWN"))));
        assert!(is_markdown_path(Some(Path::new("b.Mdown"))));
        assert!(!is_markdown_path(Some(Path::new("a.txt"))));
        assert!(!is_markdown_path(Some(Path::new("md"))));
        assert!(!is_markdown_path(None));
    }

    #[test]
    fn relative_image_becomes_file_url() {
        let base = PathBuf::from("/abs/path");
        let out = rewrite_image_paths("see ![a](img/x.png) ok", &base);
        assert_eq!(out, "see ![a](file:///abs/path/img/x.png) ok");
    }

    #[test]
    fn destination_found_after_bracket_not_in_title() {
        let out = rewrite_image_paths("![x.png](x.png \"x.png\")", Path::new("/d"));
        assert_eq!(out, "![x.png](file:///d/x.png \"x.png\")");
    }

    #[test]
    fn relative_base_dir_becomes_absolute() {
        let base = absolute_base_dir(Some(Path::new("README.md")));
        assert!(base.is_absolute(), "{base:?}");
        assert_eq!(base, std::env::current_dir().unwrap());
        let base = absolute_base_dir(Some(Path::new("docs/a.md")));
        assert_eq!(base, std::env::current_dir().unwrap().join("docs"));
        let out = rewrite_image_paths("![a](img/x.png)", &base);
        assert!(!out.contains("file:///img"), "{out}");
        assert!(out.contains("/docs/img/x.png"), "{out}");
        assert_eq!(
            absolute_base_dir(Some(Path::new("/abs/a.md"))),
            PathBuf::from("/abs")
        );
    }

    #[test]
    fn relative_image_with_spaces_is_encoded() {
        let out = rewrite_image_paths("![a](<my img.png>)", Path::new("/d"));
        assert_eq!(out, "![a](<file:///d/my%20img.png>)");
    }

    #[test]
    fn file_and_absolute_images_left_alone() {
        let base = Path::new("/abs");
        let src = "![a](file:///x/y.png) ![b](/x/z.png)";
        assert_eq!(rewrite_image_paths(src, base), src);
    }

    #[test]
    fn remote_image_replaced_by_alt() {
        let out = rewrite_image_paths("a ![logo *x*](https://e.com/l.png) b", Path::new("/d"));
        assert_eq!(out, "a \\[logo x\\] b");
        let out = rewrite_image_paths("![](HTTP://e.com/l.png)", Path::new("/d"));
        assert_eq!(out, "\\[\\]");
    }

    #[test]
    fn non_image_links_untouched() {
        let src = "[a](img/x.png) and [b](https://e.com) and `![c](d.png)`";
        assert_eq!(rewrite_image_paths(src, Path::new("/d")), src);
    }
}
