//! Incremental syntax highlighting with syntect: asset loading, per-line cache and decorator.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Stroke, TextFormat};
use ropey::Rope;
use syntect::highlighting::ThemeSet;
use syntect::highlighting::{
    FontStyle, HighlightState, Highlighter, RangedHighlightIterator, Style,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use crate::buffer::Buffer;
use crate::view::Decorator;

/// Name of the fallback syntax.
const PLAIN_TEXT: &str = "Plain Text";
/// Scope prefix of comments.
const COMMENT_SCOPE: &str = "comment";
/// Scope prefix of string literals.
const STRING_SCOPE: &str = "string";
/// `COMMENT_SCOPE` as a parsed scope, for cheap prefix tests.
static COMMENT_PREFIX: LazyLock<Scope> =
    LazyLock::new(|| Scope::new(COMMENT_SCOPE).expect("valid scope"));
/// `STRING_SCOPE` as a parsed scope, for cheap prefix tests.
static STRING_PREFIX: LazyLock<Scope> =
    LazyLock::new(|| Scope::new(STRING_SCOPE).expect("valid scope"));
/// Width of the underline stroke for underlined tokens.
const UNDERLINE_WIDTH: f32 = 1.0;
/// Parser states are kept at the start of every this many lines.
const CHECKPOINT_LINES: usize = 32;
/// Time a frame may spend parsing lines before the rest waits for the next frame.
const FRAME_PARSE_BUDGET: Duration = Duration::from_millis(8);
/// Maximum number of lines a background warm-up parses.
const WARM_UP_LINES: usize = 2000;

/// Warm-up progress of one syntax.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Warmth {
    /// A background thread is compiling the syntax regexes.
    Pending,
    /// The syntax regexes are compiled: parsing on the UI thread is cheap.
    Ready,
}

/// Syntax definitions and themes, loaded once off the UI thread.
pub struct HighlightAssets {
    /// Syntax definitions (newline-aware).
    pub syntaxes: SyntaxSet,
    /// Color themes.
    pub themes: ThemeSet,
    /// Warm-up progress by syntax name (see `ensure_warm`).
    warm: Mutex<HashMap<String, Warmth>>,
    /// Names of `themes`, sorted, computed once.
    theme_names: Vec<String>,
}

impl HighlightAssets {
    /// Loads the bundled syntaxes and themes (slow: call off the UI thread).
    pub fn load() -> HighlightAssets {
        let themes = ThemeSet::load_defaults();
        // `themes` is a BTreeMap: the keys come out sorted.
        let theme_names = themes.themes.keys().cloned().collect();
        HighlightAssets {
            syntaxes: two_face::syntax::extra_newlines(),
            themes,
            warm: Mutex::new(HashMap::new()),
            theme_names,
        }
    }

    /// Returns true when `syntax` is ready to be highlighted on the UI thread.
    ///
    /// syntect compiles a syntax's regexes lazily, the first time the parser meets them, and
    /// big grammars (TypeScript) take hundreds of milliseconds to compile. The first call for
    /// a syntax returns false and parses `sample` on a background thread instead: the compiled
    /// regexes are cached in the shared `SyntaxSet`, and `ctx` is repainted when done.
    pub fn ensure_warm(self: &Arc<Self>, syntax: &str, sample: &Rope, ctx: &egui::Context) -> bool {
        let mut warm = self.warm.lock().expect("warm-up map poisoned");
        match warm.get(syntax) {
            Some(Warmth::Ready) => return true,
            Some(Warmth::Pending) => return false,
            None => {}
        }
        warm.insert(syntax.to_string(), Warmth::Pending);
        drop(warm);
        let assets = Arc::clone(self);
        let syntax = syntax.to_string();
        let sample = sample.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            assets.warm_up(&syntax, &sample);
            let mut warm = assets.warm.lock().expect("warm-up map poisoned");
            warm.insert(syntax, Warmth::Ready);
            drop(warm);
            ctx.request_repaint();
        });
        false
    }

    /// Parses the first `WARM_UP_LINES` lines of `sample` with `syntax`, so the regexes the
    /// text needs get compiled. The parse result is thrown away.
    fn warm_up(&self, syntax: &str, sample: &Rope) {
        let mut state = ParseState::new(find_syntax(self, syntax));
        for line in sample.lines().take(WARM_UP_LINES) {
            // A parse error only means fewer regexes are warmed: the UI parse reports it.
            if state.parse_line(&line.to_string(), &self.syntaxes).is_err() {
                return;
            }
        }
    }

    /// Returns the names of the available themes, sorted (computed once at load).
    pub fn theme_names(&self) -> &[String] {
        &self.theme_names
    }
}

/// Handle to assets that load on a background thread.
pub struct AssetsHandle {
    /// Filled by the loader thread when loading is done.
    slot: Arc<Mutex<Option<Arc<HighlightAssets>>>>,
}

impl AssetsHandle {
    /// Starts loading the assets in a thread and requests a repaint when they are ready.
    pub fn spawn(ctx: egui::Context) -> AssetsHandle {
        let slot = Arc::new(Mutex::new(None));
        let thread_slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            let assets = Arc::new(HighlightAssets::load());
            *thread_slot.lock().expect("assets slot poisoned") = Some(assets);
            ctx.request_repaint();
        });
        AssetsHandle { slot }
    }

    /// Returns the assets once loaded, without blocking on the loader.
    pub fn get(&self) -> Option<Arc<HighlightAssets>> {
        self.slot.lock().expect("assets slot poisoned").clone()
    }
}

/// Coarse token class, used by the spell checker to pick checkable text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpanKind {
    /// Inside a comment.
    Comment,
    /// Inside a string literal.
    String,
    /// Anything else.
    Other,
}

/// Classifies a scope stack: comment wins over string, anything else is `Other`.
pub fn span_kind(stack: &ScopeStack) -> SpanKind {
    let comment = *COMMENT_PREFIX;
    let string = *STRING_PREFIX;
    let mut kind = SpanKind::Other;
    for &scope in stack.as_slice() {
        if comment.is_prefix_of(scope) {
            return SpanKind::Comment;
        }
        if string.is_prefix_of(scope) {
            kind = SpanKind::String;
        }
    }
    kind
}

/// A styled run of text within one line.
#[derive(Clone, Debug)]
pub struct StyledSpan {
    /// Char range within the line (newline excluded).
    pub range: Range<usize>,
    /// Foreground color.
    pub color: Color32,
    /// Italic font style. (Bold theme styles are not kept: egui's `TextFormat` has no bold
    /// attribute and the editor fonts have no bold face to switch to.)
    pub italic: bool,
    /// Underlined font style.
    pub underline: bool,
    /// Token class.
    pub kind: SpanKind,
}

/// Returns the name of the syntax for `path` by extension, or `"Plain Text"`.
pub fn syntax_for_path(assets: &HighlightAssets, path: Option<&Path>) -> String {
    let ext = path.and_then(|p| p.extension()).and_then(|e| e.to_str());
    ext.and_then(|e| assets.syntaxes.find_syntax_by_extension(e))
        .map_or_else(|| PLAIN_TEXT.to_string(), |s| s.name.clone())
}

/// Finds a syntax by name, then by extension, falling back to plain text.
fn find_syntax<'a>(assets: &'a HighlightAssets, name_or_ext: &str) -> &'a SyntaxReference {
    let syntaxes = &assets.syntaxes;
    syntaxes
        .find_syntax_by_name(name_or_ext)
        .or_else(|| syntaxes.find_syntax_by_extension(name_or_ext))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
}

/// Parser and highlighter state at the start of a line.
#[derive(Clone)]
struct LineState {
    /// Syntax parser state.
    parse: ParseState,
    /// Theme highlighter state.
    highlight: HighlightState,
}

/// Per-line highlight cache: spans of every parsed line, plus parser states at checkpoints
/// (every `CHECKPOINT_LINES` lines) and at the parse frontier, so parsing can resume after an
/// edit without keeping a state per line.
#[derive(Default)]
pub struct HighlightCache {
    /// `(syntax, theme)` the cache was built for.
    key: Option<(String, String)>,
    /// State at the start of line `k * CHECKPOINT_LINES` for every checkpoint `k` reached.
    checkpoints: Vec<LineState>,
    /// State at the start of line `spans.len()`; `None` when that line is a checkpoint.
    next_state: Option<LineState>,
    /// Spans of every line parsed so far.
    spans: Vec<Vec<StyledSpan>>,
    /// Buffer revision the spans were last checked against (see `sync`).
    revision: Option<u64>,
    /// Parsing stops (until the next frame) once this instant is passed; `None`: no limit.
    deadline: Option<Instant>,
    /// True when a request was left unanswered because the deadline passed.
    starved: bool,
}

impl HighlightCache {
    /// Creates an empty cache.
    pub fn new() -> HighlightCache {
        HighlightCache::default()
    }

    /// Starts a frame: parsing stops at `deadline` (`None`: never) and the starved flag is
    /// cleared.
    pub fn begin_frame(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
        self.starved = false;
    }

    /// True when a line was not highlighted this frame because the parse budget ran out.
    pub fn is_starved(&self) -> bool {
        self.starved
    }

    /// Forgets everything computed for `line` and the lines after it. Parsing resumes from
    /// the checkpoint at or before `line`.
    pub fn invalidate_from(&mut self, line: usize) {
        if line >= self.spans.len() {
            return;
        }
        let keep = line / CHECKPOINT_LINES;
        self.checkpoints.truncate(keep + 1);
        self.spans.truncate(keep * CHECKPOINT_LINES);
        self.next_state = None;
    }

    /// Brings the cache up to `buffer`: forgets the lines from the first one edited since the
    /// last call (everything when the edits are no longer known).
    pub fn sync(&mut self, buffer: &Buffer) {
        let dirty = match self.revision {
            Some(rev) => buffer.first_dirty_line_since(rev),
            None => Some(0),
        };
        if let Some(line) = dirty {
            self.invalidate_from(line);
        }
        self.revision = Some(buffer.revision());
    }

    /// Drops the whole cache (theme or syntax change).
    pub fn reset(&mut self) {
        self.key = None;
        self.checkpoints.clear();
        self.next_state = None;
        self.spans.clear();
    }

    /// True when the cache was built for `syntax` and `theme`.
    fn has_key(&self, syntax: &str, theme: &str) -> bool {
        self.key
            .as_ref()
            .is_some_and(|(s, t)| s == syntax && t == theme)
    }

    /// Returns the styled spans of `line`, parsing forward from the parse frontier. Spans
    /// cover the line text without its newline. A line already in the cache is returned as
    /// is: callers must `invalidate_from` the first edited line. Returns `None` when the
    /// frame deadline passed before `line` was reached (the progress made is kept).
    pub fn line_spans(
        &mut self,
        assets: &HighlightAssets,
        syntax_name_or_ext: &str,
        theme: &str,
        rope: &Rope,
        line: usize,
    ) -> Option<&[StyledSpan]> {
        if !self.has_key(syntax_name_or_ext, theme) {
            self.reset();
            self.key = Some((syntax_name_or_ext.to_string(), theme.to_string()));
        }
        if line < self.spans.len() {
            return Some(&self.spans[line]);
        }
        let Some(theme) = assets
            .themes
            .themes
            .get(theme)
            .or_else(|| assets.themes.themes.values().next())
        else {
            return Some(&[]);
        };
        if line >= rope.len_lines() {
            return Some(&[]);
        }
        let highlighter = Highlighter::new(theme);
        if self.checkpoints.is_empty() {
            let syntax = find_syntax(assets, syntax_name_or_ext);
            self.checkpoints.push(LineState {
                parse: ParseState::new(syntax),
                highlight: HighlightState::new(&highlighter, ScopeStack::new()),
            });
        }
        let mut state = match self.next_state.take() {
            Some(state) => state,
            None => self.checkpoints[self.spans.len() / CHECKPOINT_LINES].clone(),
        };
        while self.spans.len() <= line {
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                self.starved = true;
                self.next_state = Some(state);
                return None;
            }
            let text = rope.line(self.spans.len()).to_string();
            let spans = parse_line(assets, &highlighter, &mut state, &text);
            self.spans.push(spans);
            if self.spans.len().is_multiple_of(CHECKPOINT_LINES) {
                self.checkpoints.push(state.clone());
            }
        }
        self.next_state = Some(state);
        Some(&self.spans[line])
    }
}

/// Parses and highlights one line (with its newline), advancing `state`.
fn parse_line(
    assets: &HighlightAssets,
    highlighter: &Highlighter<'_>,
    state: &mut LineState,
    text: &str,
) -> Vec<StyledSpan> {
    let Ok(ops) = state.parse.parse_line(text, &assets.syntaxes) else {
        return Vec::new();
    };
    let content_len = text.trim_end_matches(['\n', '\r']).len();
    let mut stack = state.highlight.path.clone();
    let mut out = Vec::new();
    let iter = RangedHighlightIterator::new(&mut state.highlight, &ops, text, highlighter);
    let mut next_op = 0;
    for (style, _, range) in iter {
        // The iterator skips empty segments, so sync the stack by position, not by index.
        while next_op < ops.len() && ops[next_op].0 <= range.start {
            // A scope stack error cannot be recovered here; keep the stack as is.
            let _ = stack.apply(&ops[next_op].1);
            next_op += 1;
        }
        let end = range.end.min(content_len);
        if range.start < end {
            let start_char = text[..range.start].chars().count();
            let end_char = start_char + text[range.start..end].chars().count();
            out.push(styled_span(start_char..end_char, style, span_kind(&stack)));
        }
    }
    out
}

/// Builds a `StyledSpan` from a syntect style.
fn styled_span(range: Range<usize>, style: Style, kind: SpanKind) -> StyledSpan {
    let fg = style.foreground;
    StyledSpan {
        range,
        color: Color32::from_rgb(fg.r, fg.g, fg.b),
        italic: style.font_style.contains(FontStyle::ITALIC),
        underline: style.font_style.contains(FontStyle::UNDERLINE),
        kind,
    }
}

/// Decorator that colors lines with the highlight cache of the active tab.
pub struct HighlightDecorator<'a> {
    /// Cache of the tab being drawn.
    cache: &'a mut HighlightCache,
    /// Loaded assets; `None` (or `enabled == false`) draws plain text.
    assets: Option<&'a HighlightAssets>,
    /// Syntax name of the tab.
    syntax: &'a str,
    /// Theme name to use.
    theme: &'a str,
    /// False for large files.
    enabled: bool,
    /// Snapshot of the buffer text taken in `prepare`.
    rope: Option<Rope>,
}

impl<'a> HighlightDecorator<'a> {
    /// Creates a decorator; with `assets == None` or `enabled == false` lines stay plain.
    pub fn new(
        cache: &'a mut HighlightCache,
        assets: Option<&'a HighlightAssets>,
        syntax: &'a str,
        theme: &'a str,
        enabled: bool,
    ) -> HighlightDecorator<'a> {
        HighlightDecorator {
            cache,
            assets,
            syntax,
            theme,
            enabled,
            rope: None,
        }
    }

    /// Returns the spans of `line_idx`, or `None` when highlighting is off or the line was not
    /// reached within this frame's parse budget.
    pub fn spans(&mut self, line_idx: usize) -> Option<&[StyledSpan]> {
        if !self.enabled {
            return None;
        }
        let assets = self.assets?;
        let rope = self.rope.as_ref()?;
        self.cache
            .line_spans(assets, self.syntax, self.theme, rope, line_idx)
    }

    /// True when some line was drawn plain this frame because the parse budget ran out:
    /// the caller should repaint so highlighting catches up.
    pub fn is_incomplete(&self) -> bool {
        self.cache.is_starved()
    }
}

impl Decorator for HighlightDecorator<'_> {
    /// Invalidates the cache from the first line edited since the previous frame, snapshots
    /// the text and starts this frame's parse budget.
    fn prepare(&mut self, buffer: &Buffer) {
        self.cache
            .begin_frame(Some(Instant::now() + FRAME_PARSE_BUDGET));
        self.cache.sync(buffer);
        self.rope = Some(buffer.rope().clone());
    }

    /// Returns the colored runs of the line, with unstyled gaps left to `base`.
    fn decorate(
        &mut self,
        line_idx: usize,
        line: &str,
        base: &TextFormat,
    ) -> Vec<(Range<usize>, TextFormat)> {
        let len = line.chars().count();
        let plain = || vec![(0..len, base.clone())];
        let Some(spans) = self.spans(line_idx) else {
            return plain();
        };
        let mut out = Vec::with_capacity(spans.len());
        for span in spans {
            let range = span.range.start.min(len)..span.range.end.min(len);
            if range.start >= range.end {
                continue;
            }
            let mut format = base.clone();
            format.color = span.color;
            format.italics = span.italic;
            if span.underline {
                format.underline = Stroke::new(UNDERLINE_WIDTH, span.color);
            }
            out.push((range, format));
        }
        if out.is_empty() { plain() } else { out }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn theme_names_are_sorted_and_cached() {
        let assets = HighlightAssets::load();
        let names = assets.theme_names();
        assert!(!names.is_empty());
        assert!(names.windows(2).all(|w| w[0] < w[1]));
        assert!(std::ptr::eq(names, assets.theme_names()));
    }

    #[test]
    fn typescript_extension_has_a_syntax() {
        let assets = HighlightAssets::load();
        assert!(assets.syntaxes.find_syntax_by_extension("ts").is_some());
        assert!(assets.syntaxes.find_syntax_by_extension("tsx").is_some());
    }

    use super::*;

    /// Loads the assets synchronously for tests.
    fn assets() -> HighlightAssets {
        HighlightAssets::load()
    }

    /// Returns the char ranges of spans of `kind`, merged when adjacent.
    fn ranges_of(spans: &[StyledSpan], kind: SpanKind) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = Vec::new();
        for s in spans.iter().filter(|s| s.kind == kind) {
            match out.last_mut() {
                Some(last) if last.end == s.range.start => last.end = s.range.end,
                _ => out.push(s.range.clone()),
            }
        }
        out
    }

    #[test]
    fn span_kind_detects_comment_and_string() {
        let a = assets();
        let rope = Rope::from_str("let s = \"hi\"; // note\n");
        let mut cache = HighlightCache::new();
        let spans = cache
            .line_spans(&a, "Rust", "base16-ocean.dark", &rope, 0)
            .unwrap();
        let strings = ranges_of(spans, SpanKind::String);
        let comments = ranges_of(spans, SpanKind::Comment);
        assert!(
            strings.iter().any(|r| r.start <= 8 && r.end >= 12),
            "{strings:?}"
        );
        assert!(
            comments.iter().any(|r| r.start <= 14 && r.end == 21),
            "{comments:?}"
        );
    }

    #[test]
    fn sync_invalidates_from_the_first_edited_line() {
        let a = assets();
        let theme = "base16-ocean.dark";
        let mut buf = Buffer::from_text("/* open\nstill comment\n");
        let mut cache = HighlightCache::new();
        cache.sync(&buf);
        cache.line_spans(&a, "Rust", theme, buf.rope(), 1).unwrap();
        cache.sync(&buf);
        assert_eq!(cache.spans.len(), 2, "no edit: nothing is dropped");
        buf.replace(0..2, "//");
        cache.sync(&buf);
        assert!(cache.spans.is_empty());
        let spans = cache.line_spans(&a, "Rust", theme, buf.rope(), 1).unwrap();
        assert!(ranges_of(spans, SpanKind::Comment).is_empty());
    }

    #[test]
    fn cache_invalidation_reparses_from_edited_line() {
        let a = assets();
        let mut cache = HighlightCache::new();
        let theme = "base16-ocean.dark";
        let rope = Rope::from_str("/* open\nstill comment\n");
        let spans = cache.line_spans(&a, "Rust", theme, &rope, 1).unwrap();
        assert_eq!(ranges_of(spans, SpanKind::Comment), vec![0..13]);

        let edited = Rope::from_str("// open\nstill comment\n");
        cache.invalidate_from(0);
        let spans = cache.line_spans(&a, "Rust", theme, &edited, 1).unwrap();
        assert!(ranges_of(spans, SpanKind::Comment).is_empty());
    }

    #[test]
    fn cache_hit_returns_cached_spans_without_reparsing() {
        let a = assets();
        let mut cache = HighlightCache::new();
        let theme = "base16-ocean.dark";
        let rope = Rope::from_str("// note\n");
        let first = cache
            .line_spans(&a, "Rust", theme, &rope, 0)
            .unwrap()
            .to_vec();
        assert_eq!(ranges_of(&first, SpanKind::Comment), vec![0..7]);
        // Without invalidation a hit must not look at the text again.
        let other = Rope::from_str("let x = 1;\n");
        let hit = cache.line_spans(&a, "Rust", theme, &other, 0).unwrap();
        assert_eq!(ranges_of(hit, SpanKind::Comment), vec![0..7]);
        // A different theme is a different key and rebuilds the cache.
        let rebuilt = cache
            .line_spans(&a, "Rust", "InspiredGitHub", &other, 0)
            .unwrap();
        assert!(ranges_of(rebuilt, SpanKind::Comment).is_empty());
    }

    /// Returns the comment ranges of every line of `rope` parsed by `cache`.
    fn comments(
        cache: &mut HighlightCache,
        a: &HighlightAssets,
        rope: &Rope,
    ) -> Vec<Vec<Range<usize>>> {
        (0..rope.len_lines())
            .map(|i| {
                let spans = cache.line_spans(a, "Rust", "base16-ocean.dark", rope, i);
                ranges_of(spans.unwrap(), SpanKind::Comment)
            })
            .collect()
    }

    #[test]
    fn checkpoints_resume_parsing_like_a_fresh_cache() {
        let a = assets();
        let mut lines: Vec<String> = (0..200).map(|i| format!("let v{i} = {i};")).collect();
        lines[3] = "/* open".to_string();
        lines[150] = "close */".to_string();
        let rope = Rope::from_str(&lines.join("\n"));
        let mut cache = HighlightCache::new();
        comments(&mut cache, &a, &rope);
        assert!(cache.checkpoints.len() <= 200 / CHECKPOINT_LINES + 1);
        for edit in [70, 64, 199, 3, 40, 0] {
            lines[edit] = format!("/* edited {edit}");
            let rope = Rope::from_str(&lines.join("\n"));
            cache.invalidate_from(edit);
            assert_eq!(
                comments(&mut cache, &a, &rope),
                comments(&mut HighlightCache::new(), &a, &rope),
                "edit at {edit}"
            );
            assert_eq!(
                cache.checkpoints.len(),
                cache.spans.len() / CHECKPOINT_LINES + 1
            );
        }
    }

    #[test]
    fn parse_budget_defers_lines_and_keeps_progress() {
        let a = assets();
        let rope = Rope::from_str(&"let x = 1;\n".repeat(100));
        let mut cache = HighlightCache::new();
        cache.begin_frame(Some(Instant::now()));
        assert!(
            cache
                .line_spans(&a, "Rust", "base16-ocean.dark", &rope, 50)
                .is_none()
        );
        assert!(cache.is_starved());
        cache.begin_frame(None);
        assert!(!cache.is_starved());
        assert!(
            cache
                .line_spans(&a, "Rust", "base16-ocean.dark", &rope, 50)
                .is_some()
        );
        assert_eq!(cache.spans.len(), 51);
    }

    #[test]
    fn span_kind_matches_scope_prefixes() {
        let stack = |s: &str| s.parse::<ScopeStack>().expect("valid scope stack");
        let kind = |s: &str| span_kind(&stack(s));
        assert_eq!(kind("source.rust"), SpanKind::Other);
        assert_eq!(kind("source.rust string.quoted.double"), SpanKind::String);
        assert_eq!(
            kind("source.rust string.quoted comment.line"),
            SpanKind::Comment
        );
        assert_eq!(
            kind("source.rust comment.block string.quoted"),
            SpanKind::Comment
        );
        assert_eq!(kind("source.rust stringy.other"), SpanKind::Other);
    }

    #[test]
    fn syntax_for_path_extension() {
        let a = assets();
        assert_eq!(syntax_for_path(&a, Some(Path::new("x.rs"))), "Rust");
        assert_eq!(syntax_for_path(&a, Some(Path::new("x.md"))), "Markdown");
        assert_eq!(syntax_for_path(&a, Some(Path::new("x.zzzz"))), "Plain Text");
        assert_eq!(syntax_for_path(&a, None), "Plain Text");
    }

    #[test]
    fn spans_cover_multibyte_line() {
        let a = assets();
        let rope = Rope::from_str("let s = \"città è\"; // né\n");
        let mut cache = HighlightCache::new();
        let spans = cache
            .line_spans(&a, "Rust", "base16-ocean.dark", &rope, 0)
            .unwrap();
        let len = rope.line(0).to_string().trim_end().chars().count();
        assert_eq!(spans.first().map(|s| s.range.start), Some(0));
        assert_eq!(spans.last().map(|s| s.range.end), Some(len));
        assert!(spans.windows(2).all(|w| w[0].range.end == w[1].range.start));
    }

    #[test]
    fn ensure_warm_turns_ready_after_background_parse() {
        let a = Arc::new(assets());
        let ctx = egui::Context::default();
        let rope = Rope::from_str("const x: number = 1; // note\n");
        assert!(!a.ensure_warm("TypeScript", &rope, &ctx));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !a.ensure_warm("TypeScript", &rope, &ctx) {
            assert!(
                std::time::Instant::now() < deadline,
                "warm-up never finished"
            );
            std::thread::yield_now();
        }
        assert!(a.ensure_warm("TypeScript", &rope, &ctx));
    }

    #[test]
    fn decorator_is_plain_when_disabled() {
        let mut cache = HighlightCache::new();
        let mut deco = HighlightDecorator::new(&mut cache, None, "Rust", "x", true);
        deco.prepare(&Buffer::from_text("let x = 1;"));
        let base = TextFormat::default();
        let out = deco.decorate(0, "let x = 1;", &base);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 0..10);
    }
}
