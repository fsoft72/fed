//! Live Markdown preview: path detection, image URL rewriting and the preview panel state.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_commonmark::{CommonMarkCache, CommonMarkScrollOptions, CommonMarkViewer};
use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::buffer::Buffer;
use crate::job::BackgroundJob;

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

/// Heading size relative to the preview base font size.
const HEADING_SCALE: f32 = 1.6;
/// Small text size relative to the preview base font size.
const SMALL_SCALE: f32 = 0.8;

/// Points the text styles of `style` at the preview family, sized from `base` points. The
/// monospace style (code) keeps its own font and only follows the size.
fn apply_preview_fonts(style: &mut egui::Style, base: f32) {
    use egui::TextStyle;
    let sizes = [
        (TextStyle::Body, base),
        (TextStyle::Button, base),
        (TextStyle::Heading, base * HEADING_SCALE),
        (TextStyle::Small, base * SMALL_SCALE),
    ];
    for (text_style, size) in sizes {
        style
            .text_styles
            .insert(text_style, crate::fonts::preview_font_id(size));
    }
    if let Some(mono) = style.text_styles.get_mut(&TextStyle::Monospace) {
        mono.size = base;
    }
}

/// How long the buffer must stay unchanged before the preview source is rebuilt.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(250);

/// Decides when the preview source should be rebuilt: at once the first time, then only
/// after the buffer revision has been stable for `PREVIEW_DEBOUNCE`, so typing does not
/// copy and re-parse the document on every keystroke.
#[derive(Debug, Default)]
struct SourceDebounce {
    /// Revision of the prepared source, `None` before the first build.
    built: Option<u64>,
    /// Latest revision seen and when it was first seen.
    seen: Option<(u64, Instant)>,
}

impl SourceDebounce {
    /// Returns `Ok(true)` when the source must be rebuilt now for `revision`, `Ok(false)` when
    /// it is current, and `Err(wait)` when a rebuild is due after `wait` (repaint then).
    fn check(&mut self, revision: u64, now: Instant) -> Result<bool, Duration> {
        if self.built == Some(revision) {
            return Ok(false);
        }
        if self.built.is_none() {
            return Ok(true);
        }
        let since = match self.seen {
            Some((rev, at)) if rev == revision => at,
            _ => {
                self.seen = Some((revision, now));
                now
            }
        };
        let elapsed = now.duration_since(since);
        match elapsed >= PREVIEW_DEBOUNCE {
            true => Ok(true),
            false => Err(PREVIEW_DEBOUNCE - elapsed),
        }
    }

    /// Records that the source was rebuilt for `revision`.
    fn built(&mut self, revision: u64) {
        self.built = Some(revision);
        self.seen = None;
    }
}

/// Rendered-preview state of one tab: the Markdown render cache and the last prepared source.
pub struct Preview {
    /// Render cache of egui_commonmark (images, layout, visible-slice split points).
    cache: CommonMarkCache,
    /// The rewritten Markdown source last prepared.
    source: String,
    /// Throttles source rebuilds while the buffer is being edited.
    debounce: SourceDebounce,
    /// Font size and line spacing of the last render (the cached block positions use them).
    layout: Option<PreviewLayout>,
    /// Source being rebuilt on a background thread (text copy and image rewrite).
    building: Option<BackgroundJob<String>>,
    /// Id of the scroll area and viewer cache entry, unique per tab, so every tab keeps its
    /// own preview scroll position.
    id: egui::Id,
}

/// Text settings of the Markdown preview that change the height of the rendered blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewLayout {
    /// Base font size in points.
    pub font_size: f32,
    /// Line spacing, a factor of the font row height.
    pub line_spacing: f32,
}

/// Id salt of the preview scroll area and of its viewer cache entry.
const PREVIEW_ID: &str = "fed_preview_viewer";

/// Id of the preview scroll area of the tab with id `tab_id`.
fn preview_id(tab_id: u64) -> egui::Id {
    egui::Id::unique((PREVIEW_ID, tab_id))
}

impl Preview {
    /// Creates an empty preview for the tab with id `tab_id`; it renders on the first `show`.
    pub fn new(tab_id: u64) -> Preview {
        Preview {
            cache: CommonMarkCache::default(),
            source: String::new(),
            debounce: SourceDebounce::default(),
            layout: None,
            building: None,
            id: preview_id(tab_id),
        }
    }

    /// Draws the rendered Markdown of `buf` inside a scroll area, laying out only the visible
    /// part of the document. Once the buffer has stopped changing for `PREVIEW_DEBOUNCE`
    /// the text is copied and its image paths rewritten on a background thread; until the
    /// new source arrives the previous render stays on screen. `layout` sets the font size
    /// and the line spacing of the text.
    pub fn show(&mut self, ui: &mut egui::Ui, buf: &Buffer, layout: PreviewLayout) {
        apply_preview_fonts(ui.style_mut(), layout.font_size);
        egui_commonmark_backend::misc::set_line_spacing(ui.ctx(), layout.line_spacing);
        let id = self.id;
        if self.layout.replace(layout) != Some(layout) {
            // The cached block positions were measured with the old text size.
            self.cache.clear_viewer(&id);
        }
        match self.debounce.check(buf.revision(), Instant::now()) {
            Ok(true) => {
                // A build of an older revision still running is dropped: its text is stale.
                self.building = Some(spawn_source_build(buf, ui.ctx()));
                self.debounce.built(buf.revision());
            }
            Ok(false) => {}
            Err(wait) => ui.ctx().request_repaint_after(wait),
        }
        if self.take_built_source() {
            // The visible-slice split points belong to the old text.
            self.cache.clear_viewer(&id);
        }
        let scroll = CommonMarkScrollOptions::default().viewport_cache(true);
        CommonMarkViewer::new().show_scrollable(id, ui, &mut self.cache, &scroll, &self.source);
    }

    /// Takes the source of a finished background build. Returns true when the source
    /// changed; a crashed build keeps the previous source and shows why in the preview.
    fn take_built_source(&mut self) -> bool {
        let Some(result) = self.building.as_ref().and_then(BackgroundJob::poll) else {
            return false;
        };
        self.building = None;
        self.source = result.unwrap_or_else(|e| {
            format!(
                "*The preview could not be built: {}*",
                escape_markdown(&e.to_string())
            )
        });
        true
    }
}

/// Starts building the preview source of `buf` (its text with image paths rewritten, see
/// `rewrite_image_paths`) on a background thread; `ctx` is repainted when it is done.
fn spawn_source_build(buf: &Buffer, ctx: &egui::Context) -> BackgroundJob<String> {
    let rope = buf.rope().clone();
    let base = absolute_base_dir(buf.path());
    BackgroundJob::spawn(Some(ctx), move || {
        rewrite_image_paths(&rope.to_string(), &base)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_is_built_at_once_then_debounced() {
        let t0 = Instant::now();
        let mut d = SourceDebounce::default();
        assert_eq!(d.check(0, t0), Ok(true));
        d.built(0);
        assert_eq!(d.check(0, t0), Ok(false));
        // Typing: each new revision restarts the wait.
        assert_eq!(d.check(1, t0), Err(PREVIEW_DEBOUNCE));
        let t1 = t0 + PREVIEW_DEBOUNCE / 2;
        assert_eq!(d.check(2, t1), Err(PREVIEW_DEBOUNCE));
        assert!(d.check(2, t1 + PREVIEW_DEBOUNCE / 2).is_err());
        // Stable for the whole delay: rebuild.
        assert_eq!(d.check(2, t1 + PREVIEW_DEBOUNCE), Ok(true));
        d.built(2);
        assert_eq!(d.check(2, t1 + PREVIEW_DEBOUNCE * 3), Ok(false));
    }

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

    /// Height of `text` rendered by egui_commonmark in a 300 point wide panel with the given
    /// line spacing.
    fn rendered_height(text: &str, line_spacing: f32) -> f32 {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(300.0, 2000.0),
            )),
            ..Default::default()
        };
        let mut height = 0.0;
        let mut cache = CommonMarkCache::default();
        let mut output = ctx.run_ui(input, |ui| {
            egui_commonmark_backend::misc::set_line_spacing(ui.ctx(), line_spacing);
            height = CommonMarkViewer::new()
                .show(ui, &mut cache, text)
                .response
                .rect
                .height();
        });
        output.textures_delta.clear();
        height
    }

    #[test]
    fn every_tab_has_its_own_preview_scroll_state() {
        assert_ne!(Preview::new(1).id, Preview::new(2).id);
        assert_eq!(Preview::new(3).id, Preview::new(3).id);
    }

    #[test]
    fn the_preview_source_is_built_in_the_background() {
        let ctx = egui::Context::default();
        let mut buf = Buffer::from_text("# Title\n\n![pic](img.png)\n");
        buf.set_path(PathBuf::from("/notes/a.md"));
        let mut preview = Preview::new(1);
        let layout = PreviewLayout {
            font_size: 14.0,
            line_spacing: 1.0,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while preview.source.is_empty() {
            assert!(Instant::now() < deadline, "the source was never built");
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                preview.show(ui, &buf, layout);
            });
            output.textures_delta.clear();
        }
        assert!(
            preview.source.contains("file:///notes/img.png"),
            "{}",
            preview.source
        );
        assert!(preview.building.is_none());
    }

    #[test]
    fn line_spacing_makes_wrapped_text_taller() {
        let paragraph = "word ".repeat(200);
        let single = rendered_height(&paragraph, 1.0);
        let double = rendered_height(&paragraph, 2.0);
        assert!(double > single * 1.8, "single {single}, double {double}");
    }
}
