//! Spell checking: dictionaries, checker and background service.

pub mod dict;
pub mod prose;
pub mod words;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use eframe::egui::{self, Color32, Stroke, TextFormat};
use ropey::Rope;

use crate::buffer::Buffer;
use crate::config::ConfigPaths;
use crate::highlight::HighlightDecorator;
use crate::view::{self, Decorator};

pub use prose::ProseRanges;

/// Word results kept by `SpellChecker::is_correct` before the cache is emptied.
const MAX_WORD_CACHE: usize = 50_000;
/// Line results kept by `SpellCache` before it is emptied.
const MAX_LINE_CACHE: usize = 4_096;
/// Maximum number of suggestions returned for a word.
const MAX_SUGGESTIONS: usize = 8;
/// Name of the Markdown syntax.
const MARKDOWN_SYNTAX: &str = "Markdown";
/// Name of the plain text syntax.
const PLAIN_TEXT_SYNTAX: &str = "Plain Text";
/// Color of the underline under misspelled words.
const MISSPELLED_COLOR: Color32 = Color32::from_rgb(220, 50, 47);
/// Width of the underline under misspelled words.
const MISSPELLED_WIDTH: f32 = 1.0;

/// Checks words against Hunspell dictionaries and a personal word list.
pub struct SpellChecker {
    /// Loaded dictionaries (a word is correct if any accepts it).
    dicts: Vec<spellbook::Dictionary>,
    /// Words added by the user.
    personal: HashSet<String>,
    /// Results of `is_correct`, cleared when the personal list changes or it grows past
    /// `MAX_WORD_CACHE` entries.
    cache: HashMap<String, bool>,
    /// Incremented whenever a dictionary or personal word is added, so cached line results
    /// computed with an older word list can be discarded.
    generation: u64,
}

impl SpellChecker {
    /// Creates a checker from parsed dictionaries and the personal word list.
    pub fn new(dicts: Vec<spellbook::Dictionary>, personal: HashSet<String>) -> SpellChecker {
        SpellChecker {
            dicts,
            personal,
            cache: HashMap::new(),
            generation: 0,
        }
    }

    /// Returns a counter that changes whenever the accepted words change.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns true if `word` is accepted by a dictionary or the personal list. A capitalized
    /// word is also tried in lowercase.
    pub fn check(&self, word: &str) -> bool {
        let word = word.replace('\u{2019}', "'");
        let word = word.as_str();
        let lower = word.to_lowercase();
        let capitalized = word.chars().next().is_some_and(char::is_uppercase);
        let known = |w: &str| self.personal.contains(w) || self.dicts.iter().any(|d| d.check(w));
        known(word) || (capitalized && known(&lower))
    }

    /// Like `check`, remembering the result for repeated lookups of the same word.
    pub fn is_correct(&mut self, word: &str) -> bool {
        if let Some(&known) = self.cache.get(word) {
            return known;
        }
        let known = self.check(word);
        if self.cache.len() >= MAX_WORD_CACHE {
            self.cache.clear();
        }
        self.cache.insert(word.to_string(), known);
        known
    }

    /// Returns up to `MAX_SUGGESTIONS` distinct suggestions merged from all dictionaries.
    pub fn suggest(&self, word: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for dict in &self.dicts {
            let mut found = Vec::new();
            dict.suggest(word, &mut found);
            for s in found {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
        out.truncate(MAX_SUGGESTIONS);
        out
    }

    /// Adds a dictionary loaded after the checker was created and clears the result cache.
    pub fn add_dictionary(&mut self, dict: spellbook::Dictionary) {
        self.dicts.push(dict);
        self.cache.clear();
        self.generation += 1;
    }

    /// Adds `word` to the in-memory personal list and clears the result cache.
    pub fn add_personal(&mut self, word: &str) {
        self.personal.insert(word.to_string());
        self.cache.clear();
        self.generation += 1;
    }
}

/// Loads the personal word list (one word per line); a missing file is an empty list.
pub fn load_personal(path: &Path) -> HashSet<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashSet::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Appends `word` as a new line of the personal word list, creating the file and its
/// directory when needed.
pub fn append_personal(path: &Path, word: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{word}")
}

/// State of the background dictionary loading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpellStatus {
    /// Spell checking is switched off.
    Disabled,
    /// Dictionaries are being read from disk.
    Loading,
    /// Missing dictionaries are being downloaded.
    Downloading,
    /// The checker is available.
    Ready,
    /// Loading failed (the message says why); the checker may still hold partial data.
    Failed(String),
}

/// State shared between the service handle and its loader thread.
struct Shared {
    /// Current loading status.
    status: Mutex<SpellStatus>,
    /// The checker, once at least one dictionary is loaded.
    checker: Mutex<Option<Arc<RwLock<SpellChecker>>>>,
}

/// Loads dictionaries on a background thread and hands out the checker.
pub struct SpellService {
    /// State shared with the loader thread.
    shared: Arc<Shared>,
    /// Context used to request a repaint when loading progresses.
    ctx: egui::Context,
    /// Config locations (dictionaries directory, personal word list).
    paths: ConfigPaths,
    /// Whether missing dictionaries may be downloaded.
    allow_download: bool,
}

/// Reads and parses one dictionary from its `.aff` and `.dic` files.
fn parse_dictionary(aff: &Path, dic: &Path) -> Result<spellbook::Dictionary, String> {
    let read = |p: &Path| fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
    spellbook::Dictionary::new(&read(aff)?, &read(dic)?)
        .map_err(|e| format!("{}: {e}", aff.display()))
}

/// Updates the shared status and wakes the UI.
fn set_status(shared: &Shared, ctx: &egui::Context, status: SpellStatus) {
    // Tolerate poisoning: a crashed loader must still be able to report its failure.
    *shared.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
    ctx.request_repaint();
}

/// Makes `dict` available: creates and publishes the checker for the first dictionary, adds
/// later ones to it, so a language works as soon as it is parsed.
fn publish_dictionary(
    shared: &Shared,
    checker: &mut Option<Arc<RwLock<SpellChecker>>>,
    dict: spellbook::Dictionary,
    personal_path: &Path,
) {
    match checker {
        Some(c) => c
            .write()
            .expect("spell checker poisoned")
            .add_dictionary(dict),
        None => {
            let c = Arc::new(RwLock::new(SpellChecker::new(
                vec![dict],
                load_personal(personal_path),
            )));
            *shared.checker.lock().expect("spell checker slot poisoned") = Some(Arc::clone(&c));
            *checker = Some(c);
        }
    }
}

/// Loader thread body: locates (or downloads) and parses every language.
fn load_all(shared: &Shared, ctx: &egui::Context, paths: &ConfigPaths, allow_download: bool) {
    let dict_dir = paths.dictionaries_dir();
    let system = dict::system_dirs();
    let mut errors = Vec::new();
    let mut checker: Option<Arc<RwLock<SpellChecker>>> = None;
    for lang in dict::LANGS {
        let files = match dict::locate(lang, &dict_dir, &system) {
            Some(files) => Ok(files),
            None if allow_download => {
                set_status(shared, ctx, SpellStatus::Downloading);
                dict::download(lang, &dict_dir)
            }
            None => Err(format!("{lang}: dictionary not found")),
        };
        match files.and_then(|(aff, dic)| parse_dictionary(&aff, &dic)) {
            Ok(d) => publish_dictionary(shared, &mut checker, d, &paths.personal_dictionary()),
            Err(e) => errors.push(e),
        }
        set_status(shared, ctx, SpellStatus::Loading);
    }
    let status = if errors.is_empty() {
        SpellStatus::Ready
    } else {
        SpellStatus::Failed(format!("spell check: {}", errors.join("; ")))
    };
    set_status(shared, ctx, status);
}

/// Status message set when the loader thread panics.
const LOADER_CRASHED: &str = "dictionary loader crashed";

/// Runs the loader body `load` and turns a panic into `SpellStatus::Failed`, so the status
/// never stays `Loading` forever and a retry is offered.
fn run_guarded(shared: &Shared, ctx: &egui::Context, load: impl FnOnce()) {
    let Err(_) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(load)) else {
        return;
    };
    set_status(shared, ctx, SpellStatus::Failed(LOADER_CRASHED.to_string()));
}

impl SpellService {
    /// Starts loading the dictionaries in a thread and requests a repaint as it progresses.
    pub fn spawn(ctx: egui::Context, paths: ConfigPaths, allow_download: bool) -> SpellService {
        let shared = Arc::new(Shared {
            status: Mutex::new(SpellStatus::Loading),
            checker: Mutex::new(None),
        });
        let service = SpellService {
            shared,
            ctx,
            paths,
            allow_download,
        };
        service.start_thread();
        service
    }

    /// Spawns the loader thread for the current shared state.
    fn start_thread(&self) {
        let shared = Arc::clone(&self.shared);
        let ctx = self.ctx.clone();
        let paths = self.paths.clone();
        let allow_download = self.allow_download;
        std::thread::spawn(move || {
            run_guarded(&shared, &ctx, || {
                load_all(&shared, &ctx, &paths, allow_download)
            })
        });
    }

    /// Returns the current loading status.
    pub fn status(&self) -> SpellStatus {
        self.shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Returns the checker once at least one dictionary is loaded.
    pub fn checker(&self) -> Option<Arc<RwLock<SpellChecker>>> {
        self.shared
            .checker
            .lock()
            .expect("spell checker slot poisoned")
            .clone()
    }

    /// Adds `word` to the personal dictionary, in memory and on disk.
    pub fn add_word(&self, word: &str) -> std::io::Result<()> {
        if let Some(checker) = self.checker() {
            checker
                .write()
                .expect("spell checker poisoned")
                .add_personal(word);
        }
        append_personal(&self.paths.personal_dictionary(), word)
    }

    /// Starts loading again after a failure, unless a load is in progress.
    pub fn retry(&mut self) {
        if !matches!(self.status(), SpellStatus::Failed(_)) {
            return;
        }
        set_status(&self.shared, &self.ctx, SpellStatus::Loading);
        self.start_thread();
    }
}

/// Which parts of a document are spell checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpellMode {
    /// Source code: only comments and strings.
    Code,
    /// Markdown: prose only (no code, URLs or front matter).
    Markdown,
    /// Plain text: everything.
    Plain,
}

impl SpellMode {
    /// Picks the mode for a syntax name.
    pub fn for_syntax(syntax: &str) -> SpellMode {
        match syntax {
            MARKDOWN_SYNTAX => SpellMode::Markdown,
            PLAIN_TEXT_SYNTAX => SpellMode::Plain,
            _ => SpellMode::Code,
        }
    }
}

/// Per-tab spell checking state kept between frames.
#[derive(Default)]
pub struct SpellCache {
    /// Markdown prose ranges, parsed in the background.
    pub prose: ProseRanges,
    /// Buffer char ranges of the misspelled words painted in the last frame.
    pub marks: HashSet<Range<usize>>,
    /// Misspelled ranges (line coordinates) keyed by a hash of the line text and of its
    /// checkable ranges, valid for checker generation `generation`.
    lines: HashMap<u64, Vec<Range<usize>>>,
    /// Checker generation the cached line results were computed with.
    generation: Option<u64>,
}

impl SpellCache {
    /// Returns the cached result for `key`, if any.
    fn line(&self, key: u64) -> Option<&Vec<Range<usize>>> {
        self.lines.get(&key)
    }

    /// Stores the result for `key`, emptying the cache first when it is full.
    fn store_line(&mut self, key: u64, misspelled: Vec<Range<usize>>) {
        if self.lines.len() >= MAX_LINE_CACHE {
            self.lines.clear();
        }
        self.lines.insert(key, misspelled);
    }

    /// Drops the line results when the checker word list changed.
    fn sync_generation(&mut self, generation: u64) {
        if self.generation == Some(generation) {
            return;
        }
        self.lines.clear();
        self.generation = Some(generation);
    }
}

/// Hash of a line's text and checkable ranges: the inputs of its spell check result.
fn line_key(line: &str, ranges: &[Range<usize>]) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    line.hash(&mut h);
    ranges.hash(&mut h);
    h.finish()
}

/// Decorator that underlines misspelled words on top of the highlight decorator. Without a
/// checker it only passes the highlight spans through.
pub struct SpellDecorator<'a, 'b> {
    /// Highlight decorator providing the base spans and the token kinds for code.
    inner: &'a mut HighlightDecorator<'b>,
    /// Shared checker; `None` when spell checking is off.
    checker: Option<Arc<RwLock<SpellChecker>>>,
    /// Which text is checked.
    mode: SpellMode,
    /// Spell state of the tab (Markdown ranges, painted marks, line results).
    cache: &'a mut SpellCache,
    /// Context repainted when a background Markdown parse completes.
    ctx: egui::Context,
    /// Snapshot of the buffer text taken in `prepare`.
    rope: Option<Rope>,
}

impl<'a, 'b> SpellDecorator<'a, 'b> {
    /// Creates a decorator over `inner`; `cache` is the tab's spell state and `ctx` is
    /// repainted when the Markdown ranges are refreshed in the background. With no `checker`
    /// nothing is underlined.
    pub fn new(
        inner: &'a mut HighlightDecorator<'b>,
        checker: Option<Arc<RwLock<SpellChecker>>>,
        mode: SpellMode,
        cache: &'a mut SpellCache,
        ctx: &egui::Context,
    ) -> SpellDecorator<'a, 'b> {
        SpellDecorator {
            inner,
            checker,
            mode,
            cache,
            ctx: ctx.clone(),
            rope: None,
        }
    }

    /// Returns the misspelled ranges (line coordinates) among `ranges` of `line`, from the
    /// line cache when the same text and ranges were checked before.
    fn misspelled(&mut self, line: &str, ranges: &[Range<usize>]) -> Vec<Range<usize>> {
        let key = line_key(line, ranges);
        if let Some(hit) = self.cache.line(key) {
            return hit.clone();
        }
        let Some(checker) = &self.checker else {
            return Vec::new();
        };
        let tokens = words::tokens_in_ranges(line, ranges);
        let mut bad = Vec::new();
        if !tokens.is_empty() {
            let chars: Vec<char> = line.chars().collect();
            let text = |r: &Range<usize>| chars[r.clone()].iter().collect::<String>();
            let mut checker = checker.write().expect("spell checker poisoned");
            for token in tokens {
                if let Some(whole) = &token.whole
                    && checker.is_correct(&text(whole))
                {
                    continue;
                }
                bad.extend(
                    token
                        .parts
                        .into_iter()
                        .filter(|p| !checker.is_correct(&text(p))),
                );
            }
        }
        self.cache.store_line(key, bad.clone());
        bad
    }

    /// Returns the checkable char ranges of line `line_idx` (line coordinates).
    fn checkable_ranges(&mut self, line_idx: usize, len: usize) -> Vec<Range<usize>> {
        match self.mode {
            SpellMode::Plain => std::iter::once(0..len).collect(),
            SpellMode::Code => self
                .inner
                .spans(line_idx)
                .map(words::code_checkable)
                .unwrap_or_default(),
            SpellMode::Markdown => {
                let Some(rope) = &self.rope else {
                    return Vec::new();
                };
                let ranges = self.cache.prose.ranges();
                let start = rope.line_to_char(line_idx);
                let end = start + len;
                let first = ranges.partition_point(|r| r.end <= start);
                ranges[first..]
                    .iter()
                    .take_while(|r| r.start < end)
                    .map(|r| r.start.max(start) - start..r.end.min(end) - start)
                    .collect()
            }
        }
    }
}

impl Decorator for SpellDecorator<'_, '_> {
    /// Prepares the inner decorator, clears the marks and refreshes the Markdown ranges (the
    /// parse itself runs on a worker thread, never here).
    fn prepare(&mut self, buffer: &Buffer) {
        self.inner.prepare(buffer);
        self.rope = Some(buffer.rope().clone());
        self.cache.marks.clear();
        let Some(checker) = &self.checker else {
            return;
        };
        let generation = checker.read().expect("spell checker poisoned").generation();
        self.cache.sync_generation(generation);
        if self.mode == SpellMode::Markdown {
            self.cache.prose.refresh(buffer, &self.ctx);
        }
    }

    /// Returns the inner spans with misspelled words underlined.
    fn decorate(
        &mut self,
        line_idx: usize,
        line: &str,
        base: &TextFormat,
    ) -> Vec<(Range<usize>, TextFormat)> {
        let spans = self.inner.decorate(line_idx, line, base);
        if self.checker.is_none() {
            return spans;
        }
        let ranges = self.checkable_ranges(line_idx, line.chars().count());
        if ranges.is_empty() {
            return spans;
        }
        let misspelled = self.misspelled(line, &ranges);
        if misspelled.is_empty() {
            return spans;
        }
        if let Some(rope) = &self.rope
            && line_idx < rope.len_lines()
        {
            let start = rope.line_to_char(line_idx);
            for w in &misspelled {
                self.cache.marks.insert(start + w.start..start + w.end);
            }
        }
        view::overlay_format(spans, &misspelled, |format| {
            format.underline = Stroke::new(MISSPELLED_WIDTH, MISSPELLED_COLOR);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a checker with a tiny dictionary.
    fn checker() -> SpellChecker {
        let dict = spellbook::Dictionary::new("SET UTF-8\n", "2\nciao\nhello\n").unwrap();
        SpellChecker::new(vec![dict], HashSet::new())
    }

    #[test]
    fn personal_roundtrip() {
        let dir = std::env::temp_dir().join(format!("fed_personal_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("personal.dic");
        assert!(load_personal(&path).is_empty());
        append_personal(&path, "fabio").unwrap();
        append_personal(&path, "wox").unwrap();
        let got = load_personal(&path);
        assert_eq!(got, HashSet::from(["fabio".to_string(), "wox".to_string()]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checker_uses_personal_and_capitalization() {
        let mut c = checker();
        assert!(c.check("ciao"));
        assert!(c.check("Ciao"));
        assert!(!c.check("xyz"));
        assert!(!c.is_correct("xyz"));
        c.add_personal("xyz");
        assert!(c.check("xyz"));
        assert!(c.is_correct("xyz"));
    }

    #[test]
    fn suggestions_are_limited_and_deduplicated() {
        let dict_a = spellbook::Dictionary::new("SET UTF-8\n", "2\nhello\nhallo\n").unwrap();
        let dict_b = spellbook::Dictionary::new("SET UTF-8\n", "2\nhello\nhallo\n").unwrap();
        let c = SpellChecker::new(vec![dict_a, dict_b], HashSet::new());
        let s = c.suggest("helo");
        assert!(s.len() <= MAX_SUGGESTIONS);
        assert_eq!(s.iter().collect::<HashSet<_>>().len(), s.len());
        assert!(s.contains(&"hello".to_string()), "{s:?}");
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn decorator_underlines_misspelled_words_in_plain_text() {
        let checker = Arc::new(RwLock::new(checker()));
        let mut cache = crate::highlight::HighlightCache::new();
        let mut inner = HighlightDecorator::new(&mut cache, None, "Plain Text", "x", true);
        let mut cache = SpellCache::default();
        let buffer = Buffer::from_text("ciao xyzzy\nhello");
        let ctx = egui::Context::default();
        let mut deco = SpellDecorator::new(
            &mut inner,
            Some(checker),
            SpellMode::Plain,
            &mut cache,
            &ctx,
        );
        deco.prepare(&buffer);
        let base = TextFormat::default();
        let spans = deco.decorate(0, "ciao xyzzy", &base);
        let underlined: Vec<_> = spans
            .iter()
            .filter(|(_, f)| f.underline != Stroke::NONE)
            .map(|(r, _)| r.clone())
            .collect();
        assert_eq!(underlined, vec![5..10]);
        assert!(
            deco.decorate(1, "hello", &base)
                .iter()
                .all(|(_, f)| f.underline == Stroke::NONE)
        );
        drop(deco);
        assert_eq!(cache.marks, HashSet::from([5..10]));
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn contractions_are_checked_whole_before_splitting() {
        let en = spellbook::Dictionary::new("SET UTF-8\n", "2\ndon't\nhello\n").unwrap();
        let it = spellbook::Dictionary::new("SET UTF-8\n", "1\nalbero\n").unwrap();
        let checker = Arc::new(RwLock::new(SpellChecker::new(vec![en, it], HashSet::new())));
        let mut cache = crate::highlight::HighlightCache::new();
        let mut inner = HighlightDecorator::new(&mut cache, None, "Plain Text", "x", true);
        let mut cache = SpellCache::default();
        let text = "don't l\u{2019}albero don\u{2019}t doesn't";
        let buffer = Buffer::from_text(text);
        let ctx = egui::Context::default();
        let mut deco = SpellDecorator::new(
            &mut inner,
            Some(checker),
            SpellMode::Plain,
            &mut cache,
            &ctx,
        );
        deco.prepare(&buffer);
        deco.decorate(0, text, &TextFormat::default());
        drop(deco);
        // "doesn't" is unknown: its split part "doesn" is flagged, "t" is too short.
        assert_eq!(cache.marks, HashSet::from_iter([21..26]));
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn line_results_are_cached_until_the_word_list_changes() {
        let checker = Arc::new(RwLock::new(checker()));
        let mut hl = crate::highlight::HighlightCache::new();
        let mut inner = HighlightDecorator::new(&mut hl, None, "Plain Text", "x", true);
        let mut cache = SpellCache::default();
        let buffer = Buffer::from_text("ciao xyzzy");
        let ctx = egui::Context::default();
        let base = TextFormat::default();
        let underlined = |spans: Vec<(Range<usize>, TextFormat)>| -> Vec<Range<usize>> {
            spans
                .into_iter()
                .filter(|(_, f)| f.underline != Stroke::NONE)
                .map(|(r, _)| r)
                .collect()
        };
        {
            let mut deco = SpellDecorator::new(
                &mut inner,
                Some(Arc::clone(&checker)),
                SpellMode::Plain,
                &mut cache,
                &ctx,
            );
            deco.prepare(&buffer);
            assert_eq!(
                underlined(deco.decorate(0, "ciao xyzzy", &base)),
                vec![5..10]
            );
        }
        assert_eq!(cache.lines.len(), 1);
        // A cache hit still reports the marks, without touching the word cache.
        checker.write().unwrap().cache.clear();
        {
            let mut deco = SpellDecorator::new(
                &mut inner,
                Some(Arc::clone(&checker)),
                SpellMode::Plain,
                &mut cache,
                &ctx,
            );
            deco.prepare(&buffer);
            assert_eq!(
                underlined(deco.decorate(0, "ciao xyzzy", &base)),
                vec![5..10]
            );
        }
        assert!(checker.read().unwrap().cache.is_empty());
        assert_eq!(cache.marks, HashSet::from([5..10]));
        // Adding the word bumps the generation and drops the cached line.
        checker.write().unwrap().add_personal("xyzzy");
        let mut deco = SpellDecorator::new(
            &mut inner,
            Some(checker),
            SpellMode::Plain,
            &mut cache,
            &ctx,
        );
        deco.prepare(&buffer);
        assert!(underlined(deco.decorate(0, "ciao xyzzy", &base)).is_empty());
    }

    #[test]
    fn decorator_without_checker_passes_spans_through() {
        let mut hl = crate::highlight::HighlightCache::new();
        let mut inner = HighlightDecorator::new(&mut hl, None, "Plain Text", "x", true);
        let mut cache = SpellCache::default();
        cache.marks.insert(0..1);
        let ctx = egui::Context::default();
        let mut deco = SpellDecorator::new(&mut inner, None, SpellMode::Plain, &mut cache, &ctx);
        deco.prepare(&Buffer::from_text("xyzzy"));
        let spans = deco.decorate(0, "xyzzy", &TextFormat::default());
        assert!(spans.iter().all(|(_, f)| f.underline == Stroke::NONE));
        drop(deco);
        assert!(cache.marks.is_empty(), "stale marks are dropped");
    }

    #[test]
    fn word_cache_is_bounded() {
        let mut c = checker();
        for i in 0..MAX_WORD_CACHE + 10 {
            c.is_correct(&format!("w{i}"));
        }
        assert!(c.cache.len() <= MAX_WORD_CACHE);
    }

    #[test]
    fn checker_publishes_first_dictionary_and_accepts_more() {
        let mut c = checker();
        assert!(!c.check("albero"));
        c.add_dictionary(spellbook::Dictionary::new("SET UTF-8\n", "1\nalbero\n").unwrap());
        assert!(c.check("albero"));
    }

    #[test]
    fn loader_panic_sets_failed_status() {
        let shared = Shared {
            status: Mutex::new(SpellStatus::Loading),
            checker: Mutex::new(None),
        };
        let ctx = egui::Context::default();
        run_guarded(&shared, &ctx, || panic!("simulated loader crash"));
        assert_eq!(
            *shared.status.lock().unwrap(),
            SpellStatus::Failed(LOADER_CRASHED.to_string())
        );
        // A normal run leaves the status alone.
        let shared = Shared {
            status: Mutex::new(SpellStatus::Ready),
            checker: Mutex::new(None),
        };
        run_guarded(&shared, &ctx, || {});
        assert_eq!(*shared.status.lock().unwrap(), SpellStatus::Ready);
    }

    #[test]
    fn mode_follows_syntax_name() {
        assert_eq!(SpellMode::for_syntax("Markdown"), SpellMode::Markdown);
        assert_eq!(SpellMode::for_syntax("Plain Text"), SpellMode::Plain);
        assert_eq!(SpellMode::for_syntax("Rust"), SpellMode::Code);
    }
}
