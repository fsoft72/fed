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

use crate::buffer::{Buffer, DirtyLines};
use crate::sync;
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
    /// The warm-up panicked: the syntax stays plain (parsing it on the UI thread could
    /// panic the same way).
    Failed,
}

/// Syntax definitions and themes, loaded once off the UI thread.
pub struct HighlightAssets {
    /// Syntax definitions (newline-aware).
    pub syntaxes: SyntaxSet,
    /// Color themes.
    pub themes: ThemeSet,
    /// Warm-up progress by syntax name (see `ensure_warm`).
    warm: Mutex<HashMap<String, Warmth>>,
    /// Messages about failed warm-ups not yet reported (see `take_warm_failures`).
    warm_failures: Mutex<Vec<String>>,
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
            warm_failures: Mutex::new(Vec::new()),
            theme_names,
        }
    }

    /// Returns true when `syntax` is ready to be highlighted on the UI thread.
    ///
    /// syntect compiles a syntax's regexes lazily, the first time the parser meets them, and
    /// big grammars (TypeScript) take hundreds of milliseconds to compile. The first call for
    /// a syntax returns false and parses `sample` on a background thread instead: the compiled
    /// regexes are cached in the shared `SyntaxSet`, and `ctx` is repainted when done. A
    /// warm-up that panics leaves the syntax plain for good and is reported once through
    /// `take_warm_failures`.
    pub fn ensure_warm(self: &Arc<Self>, syntax: &str, sample: &Rope, ctx: &egui::Context) -> bool {
        let mut warm = sync::lock(&self.warm);
        match warm.get(syntax) {
            Some(Warmth::Ready) => return true,
            Some(Warmth::Pending | Warmth::Failed) => return false,
            None => {}
        }
        warm.insert(syntax.to_string(), Warmth::Pending);
        drop(warm);
        let assets = Arc::clone(self);
        let syntax = syntax.to_string();
        let sample = sample.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            assets.warm_guarded(syntax.clone(), || assets.warm_up(&syntax, &sample));
            ctx.request_repaint();
        });
        false
    }

    /// Runs the warm-up `work` of `syntax` and records how it ended: `Ready`, or `Failed` with
    /// a message for `take_warm_failures` when it panicked.
    fn warm_guarded(&self, syntax: String, work: impl FnOnce()) {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_ok();
        if !ok {
            let message = format!("syntax highlighting of {syntax} failed: it stays plain text");
            sync::lock(&self.warm_failures).push(message);
        }
        let state = if ok { Warmth::Ready } else { Warmth::Failed };
        sync::lock(&self.warm).insert(syntax, state);
    }

    /// Takes the messages about warm-ups that failed since the last call (to show them once).
    pub fn take_warm_failures(&self) -> Vec<String> {
        std::mem::take(&mut *sync::lock(&self.warm_failures))
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
            *sync::lock(&thread_slot) = Some(assets);
            ctx.request_repaint();
        });
        AssetsHandle { slot }
    }

    /// Returns the assets once loaded, without blocking on the loader.
    pub fn get(&self) -> Option<Arc<HighlightAssets>> {
        sync::lock(&self.slot).clone()
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
#[derive(Clone, PartialEq, Eq)]
struct LineState {
    /// Syntax parser state.
    parse: ParseState,
    /// Theme highlighter state.
    highlight: HighlightState,
}

/// Results of the lines after an edit, kept until parsing reaches them again: when the parser
/// state at one of their checkpoints equals the old one, the rest is reused as is.
struct Tail {
    /// Line (current numbering) of the first entry of `spans`.
    first_line: usize,
    /// Spans of the lines from `first_line` on.
    spans: Vec<Vec<StyledSpan>>,
    /// Checkpoints within `spans` (current numbering), sorted by line.
    checkpoints: Vec<(usize, LineState)>,
    /// State at the start of the line after the last entry of `spans`.
    end_state: LineState,
}

/// Per-line highlight cache: spans of every parsed line, plus parser states at checkpoints
/// (about every `CHECKPOINT_LINES` lines) and at the parse frontier, so parsing can resume
/// after an edit without keeping a state per line. After an edit the old results of the lines
/// below it are kept aside and reused as soon as the parser state converges.
#[derive(Default)]
pub struct HighlightCache {
    /// `(syntax, theme)` the cache was built for.
    key: Option<(String, String)>,
    /// States at the start of some parsed lines, sorted by line, at most `CHECKPOINT_LINES`
    /// apart; the first one is line 0.
    checkpoints: Vec<(usize, LineState)>,
    /// State at the start of line `spans.len()`; `None` when that line is the last checkpoint.
    next_state: Option<LineState>,
    /// Spans of every line parsed so far.
    spans: Vec<Vec<StyledSpan>>,
    /// Old results of the lines after the last edit, waiting to be reused.
    tail: Option<Tail>,
    /// Buffer revision the spans were last checked against (see `sync`).
    revision: Option<u64>,
    /// Parsing stops (until the next frame) once this instant is passed; `None`: no limit.
    deadline: Option<Instant>,
    /// True when a request was left unanswered because the deadline passed.
    starved: bool,
    /// Number of lines parsed (tests check the reuse with it).
    #[cfg(test)]
    parsed: usize,
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
        self.tail = None;
        if line >= self.spans.len() {
            return;
        }
        let keep = self.checkpoints.partition_point(|(l, _)| *l <= line).max(1);
        self.checkpoints.truncate(keep);
        self.spans
            .truncate(self.checkpoints.last().map_or(0, |(l, _)| *l));
        self.next_state = None;
    }

    /// Forgets the lines changed by `dirty` (see `Buffer::dirty_lines_since`) and the lines
    /// after them, keeping the results of the unchanged lines below the edit as a `Tail` that
    /// parsing reuses once it converges. A previous unused tail is dropped.
    pub fn invalidate(&mut self, dirty: DirtyLines) {
        let tail = self.take_tail(dirty);
        self.invalidate_from(dirty.start);
        self.tail = tail;
    }

    /// Splits off the results of the lines after `dirty` (renumbered for the current text),
    /// or `None` when there is nothing to reuse.
    fn take_tail(&mut self, dirty: DirtyLines) -> Option<Tail> {
        if dirty.old_end == usize::MAX || dirty.start >= self.spans.len() {
            return None;
        }
        if dirty.old_end > self.spans.len() {
            return None;
        }
        let end_state = self.frontier_state()?;
        let renumber = |l: usize| l - dirty.old_end + dirty.end;
        let first = self
            .checkpoints
            .partition_point(|(l, _)| *l < dirty.old_end);
        let checkpoints: Vec<(usize, LineState)> = self
            .checkpoints
            .drain(first..)
            .map(|(l, state)| (renumber(l), state))
            .collect();
        if checkpoints.is_empty() {
            return None;
        }
        Some(Tail {
            first_line: dirty.end,
            spans: self.spans.split_off(dirty.old_end),
            checkpoints,
            end_state,
        })
    }

    /// State at the start of line `spans.len()`, if known.
    fn frontier_state(&self) -> Option<LineState> {
        if let Some(state) = &self.next_state {
            return Some(state.clone());
        }
        self.checkpoints
            .last()
            .filter(|(l, _)| *l == self.spans.len())
            .map(|(_, state)| state.clone())
    }

    /// Brings the cache up to `buffer`: forgets the lines edited since the last call
    /// (everything when the edits are no longer known), keeping the lines below for reuse.
    pub fn sync(&mut self, buffer: &Buffer) {
        match self.revision {
            Some(rev) => {
                if let Some(dirty) = buffer.dirty_lines_since(rev) {
                    self.invalidate(dirty);
                }
            }
            None => self.invalidate_from(0),
        }
        self.revision = Some(buffer.revision());
    }

    /// Drops the whole cache (theme or syntax change).
    pub fn reset(&mut self) {
        self.key = None;
        self.checkpoints.clear();
        self.next_state = None;
        self.spans.clear();
        self.tail = None;
    }

    /// True when the cache was built for `syntax` and `theme`.
    fn has_key(&self, syntax: &str, theme: &str) -> bool {
        self.key
            .as_ref()
            .is_some_and(|(s, t)| s == syntax && t == theme)
    }

    /// Reuses the tail when line `line` (the parse frontier, whose start state is `state`) is
    /// one of its checkpoints with the same state: appends its spans and checkpoints and
    /// returns the state after them. Drops the tail once parsing has passed all of them.
    fn splice_tail(&mut self, line: usize, state: &LineState) -> Option<LineState> {
        let tail = self.tail.as_ref()?;
        if tail.checkpoints.last().is_none_or(|(l, _)| *l < line) {
            self.tail = None;
            return None;
        }
        let idx = tail.checkpoints.iter().position(|(l, _)| *l == line)?;
        if tail.checkpoints[idx].1 != *state {
            return None;
        }
        let mut tail = self.tail.take()?;
        self.spans
            .extend(tail.spans.drain(line - tail.first_line..));
        let known = self.checkpoints.last().is_some_and(|(l, _)| *l == line);
        let from = if known { idx + 1 } else { idx };
        self.checkpoints.extend(tail.checkpoints.drain(from..));
        Some(tail.end_state)
    }

    /// Returns the styled spans of `line`, parsing forward from the parse frontier. Spans
    /// cover the line text without its newline. A line already in the cache is returned as
    /// is: callers must `sync` (or `invalidate_from`) after edits. Returns `None` when the
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
            self.spans.clear();
            self.checkpoints.push((
                0,
                LineState {
                    parse: ParseState::new(syntax),
                    highlight: HighlightState::new(&highlighter, ScopeStack::new()),
                },
            ));
        }
        let mut state = match self.frontier_state() {
            Some(state) => state,
            None => {
                // The frontier state is always kept; should it be missing, resume from the
                // last checkpoint instead of failing.
                let (at, state) = self.checkpoints.last().cloned().expect("line 0 checkpoint");
                self.spans.truncate(at);
                state
            }
        };
        self.next_state = None;
        while self.spans.len() <= line {
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                self.starved = true;
                self.next_state = Some(state);
                return None;
            }
            let current = self.spans.len();
            if let Some(after) = self.splice_tail(current, &state) {
                state = after;
                continue;
            }
            let text = rope.line(current).to_string();
            let spans = parse_line(assets, &highlighter, &mut state, &text);
            #[cfg(test)]
            {
                self.parsed += 1;
            }
            self.spans.push(spans);
            let last = self.checkpoints.last().map_or(0, |(l, _)| *l);
            if self.spans.len() - last >= CHECKPOINT_LINES {
                self.checkpoints.push((self.spans.len(), state.clone()));
            }
        }
        if self
            .checkpoints
            .last()
            .is_none_or(|(l, _)| *l != self.spans.len())
        {
            self.next_state = Some(state);
        }
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

    /// Rust source of `lines` lines with a block comment in the middle.
    fn rust_source(lines: usize) -> String {
        (0..lines)
            .map(|i| match i {
                50 => "/* a block\n".to_string(),
                52 => "   comment */\n".to_string(),
                _ => format!("let v{i} = \"s{i}\"; // n{i}\n"),
            })
            .collect()
    }

    /// Parses every line of `buf` with `cache` and returns the spans of each line.
    fn all_spans(cache: &mut HighlightCache, a: &HighlightAssets, buf: &Buffer) -> Vec<String> {
        cache.sync(buf);
        let rope = buf.rope();
        (0..rope.len_lines())
            .map(|i| {
                let spans = cache
                    .line_spans(a, "Rust", "base16-ocean.dark", rope, i)
                    .unwrap();
                format!("{spans:?}")
            })
            .collect()
    }

    #[test]
    fn edits_reuse_the_lines_below_once_the_parser_converges() {
        let a = assets();
        let mut buf = Buffer::from_text(&rust_source(400));
        let mut cache = HighlightCache::new();
        all_spans(&mut cache, &a, &buf);
        let edits: [(usize, &str); 5] = [
            (buf.rope().line_to_char(10) + 4, "xy"),
            (
                buf.rope().line_to_char(100),
                "let added = 1;\nlet more = 2;\n",
            ),
            (buf.rope().line_to_char(48), "/* "),
            (buf.rope().line_to_char(48), " */"),
            (buf.rope().line_to_char(300), "\n"),
        ];
        for (at, text) in edits {
            buf.replace(at..at, text);
            cache.parsed = 0;
            let got = all_spans(&mut cache, &a, &buf);
            let fresh = all_spans(&mut HighlightCache::new(), &a, &buf);
            assert_eq!(got, fresh, "after inserting {text:?}");
            if !text.contains("/*") && !text.contains("*/") {
                assert!(
                    cache.parsed <= 2 * CHECKPOINT_LINES,
                    "reparsed {}",
                    cache.parsed
                );
            }
        }
        // Deleting lines (across a checkpoint) also reuses the rest.
        let (from, to) = (buf.rope().line_to_char(60), buf.rope().line_to_char(140));
        buf.replace(from..to, "");
        cache.parsed = 0;
        let got = all_spans(&mut cache, &a, &buf);
        assert_eq!(got, all_spans(&mut HighlightCache::new(), &a, &buf));
        assert!(
            cache.parsed <= 2 * CHECKPOINT_LINES,
            "reparsed {}",
            cache.parsed
        );
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
    fn a_panicking_warm_up_leaves_the_syntax_plain_and_is_reported_once() {
        let a = Arc::new(assets());
        let ctx = egui::Context::default();
        let rope = Rope::from_str("x\n");
        a.warm_guarded("Rust".to_string(), || panic!("regex compiler crashed"));
        assert!(
            !a.ensure_warm("Rust", &rope, &ctx),
            "no new warm-up, stays plain"
        );
        let failures = a.take_warm_failures();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("Rust"));
        assert!(a.take_warm_failures().is_empty());
        a.warm_guarded("Python".to_string(), || {});
        assert!(a.ensure_warm("Python", &rope, &ctx));
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
