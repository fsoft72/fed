//! Find/replace and go-to-line bars, plus the decorator that highlights search matches.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eframe::egui::{self, Color32, Key, Modifiers, TextFormat};
use regex::Regex;
use ropey::Rope;

use crate::buffer::Buffer;
use crate::editing::Selection;
use crate::highlight::StyledSpan;
use crate::job::BackgroundJob;
use crate::search::{self, SearchQuery};
use crate::tab::Tab;
use crate::text_nav;
use crate::view::{self, Decorator};

/// Width of the search and replacement text fields in points.
const FIELD_WIDTH: f32 = 220.0;
/// Width of the go-to-line text field in points.
const GOTO_FIELD_WIDTH: f32 = 100.0;
/// Id source of the search field (also used to focus it).
const FIND_FIELD_ID: &str = "fed_find_field";
/// Id source of the replacement field.
const REPLACE_FIELD_ID: &str = "fed_replace_field";
/// Id source of the go-to-line field.
const GOTO_FIELD_ID: &str = "fed_goto_field";
/// Background drawn behind search matches (translucent, readable on light and dark themes).
const MATCH_BACKGROUND: Color32 = Color32::from_rgba_premultiplied(110, 80, 0, 110);

/// Which bar is shown above the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BarMode {
    /// No bar.
    #[default]
    Hidden,
    /// Search only.
    Find,
    /// Search and replace.
    Replace,
    /// Go to line.
    GoToLine,
}

/// Documents of at least this many bytes are searched on a background thread when the query
/// changes, so typing in the find field never waits for a whole-document regex pass.
const BACKGROUND_SEARCH_BYTES: usize = 1024 * 1024;
/// How often the bar polls a running background search.
const SEARCH_POLL_INTERVAL: Duration = Duration::from_millis(30);
/// Count label shown while a background search runs.
const SEARCHING_LABEL: &str = "searching...";

/// A whole-document search running on a background thread; dropping it cancels the search.
struct SearchJob {
    /// Tab searched.
    tab_id: u64,
    /// Buffer revision of the searched snapshot.
    revision: u64,
    /// Length in chars of that snapshot.
    chars: usize,
    /// Query searched.
    query: SearchQuery,
    /// Set to stop the worker early.
    cancel: Arc<AtomicBool>,
    /// The search; its result is `None` when it was cancelled.
    job: BackgroundJob<Option<Vec<Range<usize>>>>,
}

impl SearchJob {
    /// Starts searching a snapshot of `buffer` for `re` (compiled from `query`).
    fn spawn(tab_id: u64, buffer: &Buffer, query: &SearchQuery, re: Regex) -> SearchJob {
        let rope = buffer.rope().clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let job = BackgroundJob::spawn(None, move || {
            search::find_all_until(&rope, &re, &worker_cancel)
        });
        SearchJob {
            tab_id,
            revision: buffer.revision(),
            chars: buffer.rope().len_chars(),
            query: query.clone(),
            cancel,
            job,
        }
    }

    /// True when this job searches `query` in the tab with id `tab_id`.
    fn is_for(&self, tab_id: u64, query: &SearchQuery) -> bool {
        self.tab_id == tab_id && self.query == *query
    }
}

impl Drop for SearchJob {
    /// Stops the worker: its result is no longer wanted.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Matches of a query at one buffer revision: `(revision, chars at that revision, matches)`.
type MatchBase = (u64, usize, Rc<Vec<Range<usize>>>);

/// Cached match list for one (tab, buffer revision, query) combination.
struct MatchCache {
    /// Tab the matches belong to.
    tab_id: u64,
    /// Buffer revision the matches were computed for.
    revision: u64,
    /// Length in chars of the buffer at `revision` (to shift matches after an edit).
    chars: usize,
    /// Query the matches were computed for.
    query: SearchQuery,
    /// Matches in document order.
    matches: Rc<Vec<Range<usize>>>,
}

/// What the user did in the bar during one frame.
#[derive(Default)]
struct BarInput {
    /// The query text or an option changed.
    changed: bool,
    /// Go to the next (`true`) or previous (`false`) match.
    step: Option<bool>,
    /// Replace the current match.
    replace: bool,
    /// Replace every match.
    replace_all: bool,
    /// Close the bar.
    close: bool,
}

/// Enter and Escape pressed in a single-line bar field this frame.
struct FieldKeys {
    /// Enter was pressed in the field (it keeps the keyboard focus).
    submitted: bool,
    /// Escape was pressed while the field had the focus.
    escaped: bool,
}

/// Reads Enter and Escape for `field`; after Enter the field asks for the focus back, so the
/// user can press Enter again (next match, replace again).
fn field_keys(ui: &egui::Ui, field: &egui::Response) -> FieldKeys {
    let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    if submitted {
        field.request_focus();
    }
    let escape = ui.input(|i| i.key_pressed(Key::Escape));
    FieldKeys {
        submitted,
        escaped: escape && (field.has_focus() || field.lost_focus()),
    }
}

/// State of the find/replace and go-to-line bars (shared by all tabs).
pub struct FindBar {
    /// Bar currently shown.
    pub mode: BarMode,
    /// Current search query.
    pub query: SearchQuery,
    /// Replacement text.
    pub replacement: String,
    /// Go-to-line input text.
    pub goto: String,
    /// Error to show in the bar (invalid regex or go-to-line input).
    pub error: Option<String>,
    /// Give keyboard focus to the main field on the next frame.
    focus_field: bool,
    /// Match list cache.
    cache: RefCell<Option<MatchCache>>,
    /// Last compiled query and its result (the error message when it does not compile), so
    /// neither a valid nor an invalid pattern is compiled again every frame.
    compiled: RefCell<Option<(SearchQuery, Result<Regex, String>)>>,
    /// Background search of a changed query in a big document, while it runs.
    job: RefCell<Option<SearchJob>>,
    /// The incremental selection of a changed query waits for the background search.
    select_pending: bool,
    /// Number of regex compilations (tests check the cache with it).
    #[cfg(test)]
    compile_count: std::cell::Cell<usize>,
}

impl Default for FindBar {
    /// Creates a hidden bar with an empty case-insensitive literal query.
    fn default() -> FindBar {
        FindBar::new()
    }
}

impl FindBar {
    /// Creates a hidden bar with an empty case-insensitive literal query.
    pub fn new() -> FindBar {
        FindBar {
            mode: BarMode::Hidden,
            query: SearchQuery {
                pattern: String::new(),
                regex: false,
                case_sensitive: false,
            },
            replacement: String::new(),
            goto: String::new(),
            error: None,
            focus_field: false,
            cache: RefCell::new(None),
            compiled: RefCell::new(None),
            job: RefCell::new(None),
            select_pending: false,
            #[cfg(test)]
            compile_count: std::cell::Cell::new(0),
        }
    }

    /// Returns the current query compiled, or its error message, compiling only when the
    /// query changed since the last call.
    fn regex(&self) -> Result<Regex, String> {
        if let Some((query, result)) = &*self.compiled.borrow()
            && *query == self.query
        {
            return result.clone();
        }
        #[cfg(test)]
        self.compile_count.set(self.compile_count.get() + 1);
        let result = self.query.compile().map_err(|e| e.to_string());
        *self.compiled.borrow_mut() = Some((self.query.clone(), result.clone()));
        result
    }

    /// Shows the bar in `mode` and focuses its field. Find and Replace pre-fill the query
    /// with the selection of `tab` when it is a single line.
    pub fn open(&mut self, mode: BarMode, tab: &Tab) {
        self.mode = mode;
        self.focus_field = true;
        self.error = None;
        if matches!(mode, BarMode::Find | BarMode::Replace)
            && let Some(text) = single_line_selection(tab)
        {
            self.query.pattern = if self.query.regex {
                regex::escape(&text)
            } else {
                text
            };
        }
    }

    /// Hides the bar and gives the editor of `tab` keyboard focus.
    pub fn close(&mut self, tab: &mut Tab) {
        self.mode = BarMode::Hidden;
        self.error = None;
        self.job.borrow_mut().take();
        self.select_pending = false;
        tab.focus_editor();
    }

    /// Returns the matches of the current query in `tab` (see `matches_in`).
    pub fn matches(&self, tab: &Tab) -> Rc<Vec<Range<usize>>> {
        self.matches_in(tab.id, &tab.buffer)
    }

    /// Returns the matches of the current query in `buffer` of the tab with id `tab_id`
    /// (empty while no search bar is shown, the pattern is empty or invalid). Cached by tab,
    /// buffer revision and query; after an edit only the changed lines are searched again.
    pub fn matches_in(&self, tab_id: u64, buffer: &Buffer) -> Rc<Vec<Range<usize>>> {
        let empty = || Rc::new(Vec::new());
        if !matches!(self.mode, BarMode::Find | BarMode::Replace) || self.query.pattern.is_empty() {
            return empty();
        }
        let Ok(re) = self.regex() else {
            return empty();
        };
        let revision = buffer.revision();
        let rope = buffer.rope();
        let previous = self
            .cache
            .borrow()
            .as_ref()
            .filter(|c| c.tab_id == tab_id && c.query == self.query)
            .map(|c| (c.revision, c.chars, c.matches.clone()));
        let base = match previous {
            Some(base) => base,
            None => match self.search_whole(tab_id, buffer, &re) {
                Some(base) => base,
                None => return empty(),
            },
        };
        let matches = match base {
            (rev, _, matches) if rev == revision => matches,
            (rev, chars, old) => match buffer.dirty_lines_since(rev) {
                Some(dirty) => Rc::new(search::rescan_lines(&old, chars, rope, &re, dirty)),
                None => old,
            },
        };
        *self.cache.borrow_mut() = Some(MatchCache {
            tab_id,
            revision,
            chars: rope.len_chars(),
            query: self.query.clone(),
            matches: matches.clone(),
        });
        matches
    }

    /// Searches the whole of `buffer` for `re` (a query without cached matches). Small
    /// documents are searched at once; big ones on a background thread, returning `None`
    /// until its result is in (a job for another query or tab is cancelled first).
    fn search_whole(&self, tab_id: u64, buffer: &Buffer, re: &Regex) -> Option<MatchBase> {
        let rope = buffer.rope();
        let here = || {
            let found = Rc::new(search::find_all(rope, re));
            (buffer.revision(), rope.len_chars(), found)
        };
        let mut job = self.job.borrow_mut();
        if rope.len_bytes() < BACKGROUND_SEARCH_BYTES {
            *job = None;
            return Some(here());
        }
        let current = job.as_ref().filter(|j| j.is_for(tab_id, &self.query));
        let Some(running) = current else {
            *job = Some(SearchJob::spawn(tab_id, buffer, &self.query, re.clone()));
            return None;
        };
        let base = match running.job.poll()? {
            Ok(Some(found)) => (running.revision, running.chars, Rc::new(found)),
            // The worker crashed (or was cancelled, which only happens on drop): search here.
            Ok(None) | Err(_) => here(),
        };
        *job = None;
        Some(base)
    }

    /// True while a background search of the current query runs.
    pub fn is_searching(&self) -> bool {
        self.job.borrow().is_some()
    }

    /// Draws the active bar and applies the user's actions to `tab`. Returns a status bar
    /// message when there is one (for example the replace-all count).
    pub fn show(&mut self, ui: &mut egui::Ui, tab: &mut Tab) -> Option<String> {
        match self.mode {
            BarMode::Hidden => None,
            BarMode::GoToLine => {
                self.show_goto(ui, tab);
                None
            }
            BarMode::Find | BarMode::Replace => self.show_search(ui, tab),
        }
    }

    /// Draws the find (and replace) rows and applies what the user did.
    fn show_search(&mut self, ui: &mut egui::Ui, tab: &mut Tab) -> Option<String> {
        self.finish_pending_select(tab);
        if self.is_searching() {
            ui.ctx().request_repaint_after(SEARCH_POLL_INTERVAL);
        }
        let mut input = BarInput::default();
        let count = self.count_label(tab);
        let replace_mode = self.mode == BarMode::Replace;
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query.pattern)
                    .id(egui::Id::unique(FIND_FIELD_ID))
                    .hint_text("Find")
                    .desired_width(FIELD_WIDTH),
            );
            if std::mem::take(&mut self.focus_field) {
                field.request_focus();
            }
            input.changed |= field.changed();
            let keys = field_keys(ui, &field);
            if keys.submitted {
                input.step = Some(!ui.input(|i| i.modifiers.shift));
            }
            input.changed |= ui
                .toggle_value(&mut self.query.case_sensitive, "Aa")
                .on_hover_text("Match case")
                .changed();
            input.changed |= ui
                .toggle_value(&mut self.query.regex, ".*")
                .on_hover_text("Regular expression")
                .changed();
            if ui.button("Prev").clicked() {
                input.step = Some(false);
            }
            if ui.button("Next").clicked() {
                input.step = Some(true);
            }
            if let Some(err) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            } else {
                ui.label(count);
            }
            if ui.small_button("x").on_hover_text("Close").clicked() {
                input.close = true;
            }
            input.close |= keys.escaped;
        });
        if replace_mode {
            ui.horizontal(|ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.replacement)
                        .id(egui::Id::unique(REPLACE_FIELD_ID))
                        .hint_text("Replace with")
                        .desired_width(FIELD_WIDTH),
                );
                let keys = field_keys(ui, &field);
                input.replace = keys.submitted;
                input.replace |= ui.button("Replace").clicked();
                input.replace_all |= ui.button("Replace all").clicked();
                input.close |= keys.escaped;
            });
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::F3)) {
            input.step = Some(false);
        } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F3)) {
            input.step = Some(true);
        }
        if input.close {
            // The editor takes focus in this same frame: hide the key from it.
            ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        }
        self.apply(tab, input)
    }

    /// Applies the user's actions from one frame of the search bar.
    fn apply(&mut self, tab: &mut Tab, input: BarInput) -> Option<String> {
        if input.close {
            self.close(tab);
            return None;
        }
        if input.changed {
            self.select_from_caret(tab);
        }
        if let Some(forward) = input.step {
            self.step(tab, forward);
        }
        if input.replace {
            self.replace_current(tab);
        }
        if input.replace_all {
            return self.replace_all(tab);
        }
        None
    }

    /// Compiles the query, updating `error`. Returns `None` for an empty or invalid pattern.
    fn compiled(&mut self) -> Option<Regex> {
        self.error = None;
        if self.query.pattern.is_empty() {
            return None;
        }
        match self.regex() {
            Ok(re) => Some(re),
            Err(e) => {
                self.error = Some(short_error(&e));
                None
            }
        }
    }

    /// Selects the first match at or after the caret (incremental search). While a
    /// background search runs the selection waits for its result.
    fn select_from_caret(&mut self, tab: &mut Tab) {
        self.select_pending = false;
        if self.compiled().is_none() {
            return;
        }
        let matches = self.matches(tab);
        if self.is_searching() {
            self.select_pending = true;
            return;
        }
        let from = tab.editor.selection.range().start;
        if let Some(m) = next_match(&matches, from, true) {
            select(tab, m);
        }
    }

    /// Makes the incremental selection that waited for a background search, once it is done.
    fn finish_pending_select(&mut self, tab: &mut Tab) {
        if !self.select_pending {
            return;
        }
        self.matches(tab);
        if !self.is_searching() {
            self.select_from_caret(tab);
        }
    }

    /// Selects the next or previous match relative to the selection.
    fn step(&mut self, tab: &mut Tab, forward: bool) {
        if self.compiled().is_none() {
            return;
        }
        let matches = self.matches(tab);
        let sel = tab.editor.selection.range();
        let found = if forward {
            next_match(&matches, sel.start, sel.is_empty())
        } else {
            prev_match(&matches, sel.start)
        };
        if let Some(m) = found {
            select(tab, m);
        }
    }

    /// Replaces the selected match (when the selection is one), then selects the next match.
    fn replace_current(&mut self, tab: &mut Tab) {
        let Some(re) = self.compiled() else { return };
        let sel = tab.editor.selection.range();
        let rep = effective_replacement(&self.query, &self.replacement);
        if self.matches(tab).contains(&sel)
            && let Some(new_text) = expand_match(tab.buffer.rope(), &re, &sel, &rep)
        {
            tab.end_typing();
            tab.buffer.replace(sel.clone(), &new_text);
            tab.editor.selection = Selection::caret(sel.start + new_text.chars().count());
            tab.editor.preferred_col = None;
        }
        let from = tab.editor.selection.range().end;
        if let Some(m) = next_match(&self.matches(tab), from, true) {
            select(tab, m);
        }
    }

    /// Replaces every match as one undo step and reports the count.
    fn replace_all(&mut self, tab: &mut Tab) -> Option<String> {
        let re = self.compiled()?;
        let rep = effective_replacement(&self.query, &self.replacement);
        tab.end_typing();
        let count = search::replace_all(&mut tab.buffer, &re, &rep);
        let len = tab.buffer.rope().len_chars();
        let sel = tab.editor.selection;
        tab.editor.selection = Selection {
            anchor: sel.anchor.min(len),
            head: sel.head.min(len),
        };
        tab.scroll_to_cursor = true;
        Some(match count {
            0 => "no matches".to_string(),
            1 => "replaced 1 occurrence".to_string(),
            n => format!("replaced {n} occurrences"),
        })
    }

    /// The `"3 of 12"` / `"no matches"` text (empty while there is no pattern).
    fn count_label(&self, tab: &Tab) -> String {
        if self.query.pattern.is_empty() || self.error.is_some() {
            return String::new();
        }
        let matches = self.matches(tab);
        if self.is_searching() {
            return SEARCHING_LABEL.to_string();
        }
        if matches.is_empty() {
            return "no matches".to_string();
        }
        let sel = tab.editor.selection.range();
        match match_index(&matches, &sel) {
            Some(i) => format!("{} of {}", i + 1, matches.len()),
            None => format!("{} matches", matches.len()),
        }
    }

    /// Draws the go-to-line bar.
    fn show_goto(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        let mut submit = false;
        let mut close = false;
        let line_count = tab.buffer.rope().len_lines();
        ui.horizontal(|ui| {
            ui.label("Go to line");
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.goto)
                    .id(egui::Id::unique(GOTO_FIELD_ID))
                    .hint_text(format!("1-{line_count}"))
                    .desired_width(GOTO_FIELD_WIDTH),
            );
            if std::mem::take(&mut self.focus_field) {
                field.request_focus();
            }
            if field.changed() {
                self.error = None;
            }
            let keys = field_keys(ui, &field);
            submit = keys.submitted;
            if let Some(err) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }
            if ui.small_button("x").on_hover_text("Close").clicked() {
                close = true;
            }
            close |= keys.escaped;
        });
        if close || submit {
            // The editor takes focus in this same frame: hide the key from it.
            ui.input_mut(|i| {
                i.consume_key(Modifiers::NONE, Key::Enter);
                i.consume_key(Modifiers::NONE, Key::Escape);
            });
        }
        if close {
            self.close(tab);
        } else if submit {
            self.go_to_line(tab, line_count);
        }
    }

    /// Moves the caret of `tab` to the first non-blank of the typed line and closes the bar.
    fn go_to_line(&mut self, tab: &mut Tab, line_count: usize) {
        let line = match parse_goto(&self.goto, line_count) {
            Ok(line) => line,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let rope = tab.buffer.rope();
        let pos = text_nav::first_non_blank(rope, rope.line_to_char(line));
        tab.end_typing();
        tab.editor.selection = Selection::caret(pos);
        tab.editor.preferred_col = None;
        tab.scroll_to_cursor = true;
        self.close(tab);
    }
}

/// Parses go-to-line input: a 1-based line number. Returns the 0-based line clamped to the
/// document (`line_count` lines), or an error message for anything that is not a number.
pub fn parse_goto(input: &str, line_count: usize) -> Result<usize, String> {
    let text = input.trim();
    let number: usize = text
        .parse()
        .map_err(|_| format!("'{text}' is not a line number"))?;
    Ok(number.saturating_sub(1).min(line_count.saturating_sub(1)))
}

/// Reduces a multi-line regex error to its last line (the reason) so it fits in the bar.
fn short_error(message: &str) -> String {
    let reason = message.lines().last().unwrap_or(message).trim();
    format!("invalid regex: {}", reason.trim_start_matches("error: "))
}

/// The selected text of `tab` when it is non-empty and on a single line.
fn single_line_selection(tab: &Tab) -> Option<String> {
    let sel = tab.editor.selection;
    if sel.is_empty() {
        return None;
    }
    let range = sel.range();
    let len = tab.buffer.rope().len_chars();
    let text = tab
        .buffer
        .rope()
        .slice(range.start.min(len)..range.end.min(len))
        .to_string();
    (!text.contains('\n')).then_some(text)
}

/// Selects `range` in `tab` and scrolls it into view.
fn select(tab: &mut Tab, range: Range<usize>) {
    tab.end_typing();
    tab.editor.selection = Selection {
        anchor: range.start,
        head: range.end,
    };
    tab.editor.preferred_col = None;
    tab.scroll_to_cursor = true;
}

/// Index of the match equal to `sel` in `matches` (sorted, non-overlapping), found by a binary
/// search; `None` when the selection is not a match.
fn match_index(matches: &[Range<usize>], sel: &Range<usize>) -> Option<usize> {
    let idx = matches.partition_point(|m| m.start < sel.start);
    (matches.get(idx) == Some(sel)).then_some(idx)
}

/// First match starting after `from` (or at `from` when `inclusive`), wrapping to the first.
/// `matches` must be sorted by start.
fn next_match(matches: &[Range<usize>], from: usize, inclusive: bool) -> Option<Range<usize>> {
    let idx = if inclusive {
        matches.partition_point(|m| m.start < from)
    } else {
        matches.partition_point(|m| m.start <= from)
    };
    matches.get(idx).or(matches.first()).cloned()
}

/// Last match starting before `from`, wrapping to the last. `matches` must be sorted by start.
fn prev_match(matches: &[Range<usize>], from: usize) -> Option<Range<usize>> {
    let idx = matches.partition_point(|m| m.start < from);
    let prev = if idx == 0 { None } else { matches.get(idx - 1) };
    prev.or(matches.last()).cloned()
}

/// The replacement string to pass to the regex engine: literal queries must not expand `$`.
fn effective_replacement(query: &SearchQuery, replacement: &str) -> String {
    if query.regex {
        replacement.to_string()
    } else {
        replacement.replace('$', "$$")
    }
}

/// Expands `replacement` for the match at char `range` (which must lie on one line),
/// resolving capture groups. Returns `None` when no match of `re` has exactly that range.
fn expand_match(
    rope: &Rope,
    re: &Regex,
    range: &Range<usize>,
    replacement: &str,
) -> Option<String> {
    if range.start >= rope.len_chars() {
        return None;
    }
    let line = rope.char_to_line(range.start);
    let line_start = rope.line_to_char(line);
    let text = text_nav::line_text(rope, line);
    for caps in re.captures_iter(&text) {
        let m = caps.get(0)?;
        let start = line_start + text[..m.start()].chars().count();
        let end = start + text[m.start()..m.end()].chars().count();
        if start == range.start && end == range.end {
            let mut out = String::new();
            caps.expand(replacement, &mut out);
            return Some(out);
        }
    }
    None
}

/// Overlays `background` on the parts of `spans` covered by `ranges` (see
/// `view::overlay_format`).
pub fn overlay_background(
    spans: Vec<(Range<usize>, TextFormat)>,
    ranges: &[Range<usize>],
    background: Color32,
) -> Vec<(Range<usize>, TextFormat)> {
    view::overlay_format(spans, ranges, |format| format.background = background)
}

/// Decorator that paints a translucent background behind search matches on top of the
/// spans produced by a wrapped decorator.
pub struct MatchDecorator<'a> {
    /// Decorator providing the base spans (syntax highlighting, plain text, ...).
    inner: &'a mut dyn Decorator,
    /// Find bar and tab id the matches are read from in `prepare`; `None` keeps `matches`.
    source: Option<(&'a FindBar, u64)>,
    /// Matches as char ranges in the buffer, sorted by start.
    matches: Rc<Vec<Range<usize>>>,
    /// Snapshot of the buffer text taken in `prepare`, used to map lines to char offsets.
    rope: Option<Rope>,
}

impl<'a> MatchDecorator<'a> {
    /// Wraps `inner`, highlighting the matches of `bar` in the tab with id `tab_id`. They are
    /// read in `prepare`, after the frame's edits, so they always fit the drawn text.
    pub fn new(inner: &'a mut dyn Decorator, bar: &'a FindBar, tab_id: u64) -> MatchDecorator<'a> {
        MatchDecorator {
            inner,
            source: Some((bar, tab_id)),
            matches: Rc::new(Vec::new()),
            rope: None,
        }
    }

    /// Wraps `inner`, highlighting the fixed `matches` (sorted char ranges in the buffer).
    pub fn with_matches(
        inner: &'a mut dyn Decorator,
        matches: Vec<Range<usize>>,
    ) -> MatchDecorator<'a> {
        MatchDecorator {
            inner,
            source: None,
            matches: Rc::new(matches),
            rope: None,
        }
    }
}

impl Decorator for MatchDecorator<'_> {
    /// The syntax spans of the inner decorator.
    fn syntax_spans(&mut self, line_idx: usize) -> Option<&[StyledSpan]> {
        self.inner.syntax_spans(line_idx)
    }

    /// Prepares the inner decorator, reads the matches of the edited buffer and snapshots
    /// its text.
    fn prepare(&mut self, buffer: &Buffer) {
        self.inner.prepare(buffer);
        if let Some((bar, tab_id)) = self.source {
            self.matches = bar.matches_in(tab_id, buffer);
        }
        self.rope = Some(buffer.rope().clone());
    }

    /// Returns the inner spans with match backgrounds overlaid.
    fn decorate(
        &mut self,
        line_idx: usize,
        line: &str,
        base: &TextFormat,
    ) -> Vec<(Range<usize>, TextFormat)> {
        let spans = self.inner.decorate(line_idx, line, base);
        let Some(rope) = &self.rope else { return spans };
        if line_idx >= rope.len_lines() {
            return spans;
        }
        let start = rope.line_to_char(line_idx);
        let end = start + line.chars().count();
        let first = self.matches.partition_point(|m| m.end <= start);
        let local: Vec<Range<usize>> = self.matches[first..]
            .iter()
            .take_while(|m| m.start < end)
            .map(|m| m.start.max(start) - start..m.end.min(end) - start)
            .collect();
        overlay_background(spans, &local, MATCH_BACKGROUND)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::PlainDecorator;

    /// Builds a query with the given options.
    fn query(pattern: &str, regex: bool, case_sensitive: bool) -> SearchQuery {
        SearchQuery {
            pattern: pattern.to_string(),
            regex,
            case_sensitive,
        }
    }

    /// A format with a distinct text color to tell spans apart.
    fn colored(color: Color32) -> TextFormat {
        TextFormat {
            color,
            ..TextFormat::default()
        }
    }

    #[test]
    fn parse_goto_valid() {
        assert_eq!(parse_goto("1", 10), Ok(0));
        assert_eq!(parse_goto(" 5 ", 10), Ok(4));
        assert_eq!(parse_goto("10", 10), Ok(9));
    }

    #[test]
    fn parse_goto_clamps() {
        assert_eq!(parse_goto("0", 10), Ok(0));
        assert_eq!(parse_goto("999", 10), Ok(9));
        assert_eq!(parse_goto("5", 0), Ok(0));
    }

    #[test]
    fn parse_goto_rejects_text() {
        assert!(parse_goto("abc", 10).is_err());
        assert!(parse_goto("", 10).is_err());
        assert!(parse_goto("-3", 10).is_err());
        assert!(parse_goto("1.5", 10).is_err());
    }

    #[test]
    fn short_error_keeps_only_the_reason() {
        let long = "invalid pattern: regex parse error:\n    (fo+\n    ^\nerror: unclosed group";
        assert_eq!(short_error(long), "invalid regex: unclosed group");
    }

    #[test]
    fn next_and_prev_wrap() {
        let m = vec![2..4, 10..12, 20..22];
        assert_eq!(next_match(&m, 0, true), Some(2..4));
        assert_eq!(next_match(&m, 2, true), Some(2..4));
        assert_eq!(next_match(&m, 2, false), Some(10..12));
        assert_eq!(next_match(&m, 21, false), Some(2..4));
        assert_eq!(prev_match(&m, 10), Some(2..4));
        assert_eq!(prev_match(&m, 2), Some(20..22));
        assert_eq!(next_match(&[], 0, true), None);
        assert_eq!(prev_match(&[], 0), None);
    }

    #[test]
    fn literal_replacement_does_not_expand_dollar() {
        let rope = Rope::from_str("a.b and a.b");
        let q = query("a.b", false, true);
        let re = q.compile().unwrap();
        let rep = effective_replacement(&q, "$1x");
        assert_eq!(expand_match(&rope, &re, &(0..3), &rep), Some("$1x".into()));
    }

    #[test]
    fn regex_replacement_expands_groups_on_the_right_match() {
        let rope = Rope::from_str("x\nfoo=1 foo=2\n");
        let q = query(r"(\w+)=(\d)", true, true);
        let re = q.compile().unwrap();
        let rep = effective_replacement(&q, "$2:$1");
        assert_eq!(
            expand_match(&rope, &re, &(8..13), &rep),
            Some("2:foo".into())
        );
        assert_eq!(expand_match(&rope, &re, &(3..5), &rep), None);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn overlay_splits_spans_at_range_borders() {
        let red = colored(Color32::RED);
        let blue = colored(Color32::BLUE);
        let spans = vec![(0..5, red.clone()), (5..10, blue.clone())];
        let bg = Color32::YELLOW;
        let out = overlay_background(spans, &[3..7], bg);
        let ranges: Vec<_> = out.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..3, 3..5, 5..7, 7..10]);
        assert_eq!(out[0].1, red);
        assert_eq!(out[1].1.background, bg);
        assert_eq!(out[1].1.color, Color32::RED);
        assert_eq!(out[2].1.background, bg);
        assert_eq!(out[2].1.color, Color32::BLUE);
        assert_eq!(out[3].1, blue);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn overlay_without_overlap_keeps_spans() {
        let base = TextFormat::default();
        let spans = vec![(0..4, base.clone())];
        assert_eq!(
            overlay_background(spans.clone(), &[], Color32::RED).len(),
            1
        );
        let out = overlay_background(spans, &[4..6], Color32::RED);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, base);
    }

    #[test]
    fn overlay_handles_range_covering_whole_span_and_multiple_ranges() {
        let spans = vec![(0..8, TextFormat::default())];
        let out = overlay_background(spans, &[0..2, 4..8], Color32::RED);
        let ranges: Vec<_> = out.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..2, 2..4, 4..8]);
        assert_eq!(out[0].1.background, Color32::RED);
        assert_ne!(out[1].1.background, Color32::RED);
        assert_eq!(out[2].1.background, Color32::RED);
    }

    #[test]
    fn match_decorator_maps_buffer_matches_to_lines() {
        let buf = Buffer::from_text("héllo\nfoo bar foo\n");
        let mut plain = PlainDecorator;
        let mut deco = MatchDecorator::with_matches(&mut plain, vec![6..9, 14..17]);
        deco.prepare(&buf);
        let base = TextFormat::default();
        let line0 = deco.decorate(0, "héllo", &base);
        assert_eq!(line0.len(), 1);
        let line1 = deco.decorate(1, "foo bar foo", &base);
        let ranges: Vec<_> = line1.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..3, 3..8, 8..11]);
        assert_eq!(line1[0].1.background, MATCH_BACKGROUND);
        assert_eq!(line1[2].1.background, MATCH_BACKGROUND);
    }

    #[test]
    fn match_decorator_reads_matches_after_the_edit() {
        let mut tab = Tab::new(Buffer::from_text("ab"));
        let mut bar = FindBar::new();
        bar.open(BarMode::Find, &tab);
        bar.query.pattern = "ab".to_string();
        assert_eq!(*bar.matches(&tab), vec![0..2]);
        tab.buffer.replace(0..0, "xx");
        let mut plain = PlainDecorator;
        let mut deco = MatchDecorator::new(&mut plain, &bar, tab.id);
        deco.prepare(&tab.buffer);
        let spans = deco.decorate(0, "xxab", &TextFormat::default());
        let ranges: Vec<_> = spans.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..2, 2..4]);
        assert_eq!(spans[1].1.background, MATCH_BACKGROUND);
    }

    #[test]
    fn regex_is_compiled_once_per_query_even_when_invalid() {
        let mut bar = FindBar::new();
        bar.mode = BarMode::Find;
        bar.query.regex = true;
        bar.query.pattern = "(".to_string();
        let mut tab = Tab::new(Buffer::from_text("a(b"));
        for _ in 0..5 {
            assert!(bar.matches(&tab).is_empty());
        }
        assert!(bar.compiled().is_none());
        assert!(bar.error.is_some());
        assert_eq!(bar.compile_count.get(), 1);
        bar.query.pattern = "b".to_string();
        bar.replacement = "b".to_string();
        bar.step(&mut tab, true);
        bar.replace_current(&mut tab);
        assert_eq!(bar.matches(&tab).len(), 1);
        assert_eq!(bar.compile_count.get(), 2);
    }

    #[test]
    fn matches_are_cached_and_follow_edits() {
        let mut tab = Tab::new(Buffer::from_text("ab ab"));
        let mut bar = FindBar::new();
        bar.open(BarMode::Find, &tab);
        bar.query.pattern = "ab".to_string();
        let first = bar.matches(&tab);
        assert_eq!(*first, vec![0..2, 3..5]);
        assert!(Rc::ptr_eq(&first, &bar.matches(&tab)));
        tab.buffer.replace(0..0, "ab ");
        assert_eq!(*bar.matches(&tab), vec![0..2, 3..5, 6..8]);
        tab.buffer.replace(4..8, "\nab");
        assert_eq!(*bar.matches(&tab), vec![0..2, 5..7]);
        bar.mode = BarMode::Hidden;
        assert!(bar.matches(&tab).is_empty());
    }

    /// Polls `bar` until its background search is done (at most a few seconds).
    fn wait_for_search(bar: &FindBar, tab: &Tab) -> Rc<Vec<Range<usize>>> {
        for _ in 0..500 {
            let found = bar.matches(tab);
            if !bar.is_searching() {
                return found;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the background search did not finish");
    }

    /// A buffer of `lines` lines of 32 bytes, every one containing "needle" once.
    fn big_tab(lines: usize) -> Tab {
        let text: String = (0..lines).map(|i| format!("{i:>20} needle\n")).collect();
        Tab::new(Buffer::from_text(&text))
    }

    #[test]
    fn a_new_query_on_a_big_document_is_searched_in_the_background() {
        let mut tab = big_tab(BACKGROUND_SEARCH_BYTES / 28 + 1);
        let lines = tab.buffer.rope().len_lines() - 1;
        let mut bar = FindBar::new();
        bar.open(BarMode::Find, &tab);
        bar.query.pattern = "needle".to_string();
        assert!(
            bar.matches(&tab).is_empty(),
            "nothing until the search is done"
        );
        assert!(bar.is_searching());
        assert_eq!(bar.count_label(&tab), SEARCHING_LABEL);
        // An edit while the worker runs is applied to its result incrementally.
        tab.buffer.replace(0..0, "needle ");
        let found = wait_for_search(&bar, &tab);
        assert_eq!(found.len(), lines + 1);
        assert_eq!(found[0], 0..6);
    }

    #[test]
    fn a_changed_query_cancels_the_running_search() {
        let tab = big_tab(BACKGROUND_SEARCH_BYTES / 28 + 1);
        let mut bar = FindBar::new();
        bar.open(BarMode::Find, &tab);
        bar.query.pattern = "needle".to_string();
        bar.matches(&tab);
        let first = Arc::clone(&bar.job.borrow().as_ref().unwrap().cancel);
        bar.query.pattern = "needl".to_string();
        bar.matches(&tab);
        assert!(
            first.load(Ordering::Relaxed),
            "the old worker is told to stop"
        );
        assert_eq!(
            wait_for_search(&bar, &tab).len(),
            tab.buffer.rope().len_lines() - 1
        );
    }

    #[test]
    fn match_index_finds_only_exact_matches() {
        let matches = vec![0..2, 5..7, 9..12];
        assert_eq!(match_index(&matches, &(5..7)), Some(1));
        assert_eq!(match_index(&matches, &(9..12)), Some(2));
        assert_eq!(match_index(&matches, &(5..6)), None);
        assert_eq!(match_index(&matches, &(3..3)), None);
        assert_eq!(match_index(&[], &(0..1)), None);
    }

    #[test]
    fn open_prefills_single_line_selection_only() {
        let mut tab = Tab::new(Buffer::from_text("héllo\nworld"));
        let mut bar = FindBar::new();
        tab.editor.selection = Selection { anchor: 0, head: 5 };
        bar.open(BarMode::Find, &tab);
        assert_eq!(bar.query.pattern, "héllo");
        tab.editor.selection = Selection { anchor: 0, head: 8 };
        bar.query.pattern = "keep".into();
        bar.open(BarMode::Find, &tab);
        assert_eq!(bar.query.pattern, "keep");
    }

    #[test]
    fn replace_current_then_next_and_replace_all_with_accents() {
        let mut tab = Tab::new(Buffer::from_text("città città\ncittà"));
        let mut bar = FindBar::new();
        bar.open(BarMode::Replace, &tab);
        bar.query.pattern = "CITTÀ".into();
        bar.replacement = "x".into();
        bar.select_from_caret(&mut tab);
        assert_eq!(tab.editor.selection.range(), 0..5);
        bar.replace_current(&mut tab);
        assert_eq!(tab.buffer.text(), "x città\ncittà");
        assert_eq!(tab.editor.selection.range(), 2..7);
        assert_eq!(
            bar.replace_all(&mut tab),
            Some("replaced 2 occurrences".into())
        );
        assert_eq!(tab.buffer.text(), "x x\nx");
        assert!(tab.buffer.undo().is_some());
        assert_eq!(tab.buffer.text(), "x città\ncittà");
    }

    #[test]
    fn invalid_regex_sets_error_and_go_to_line_moves_caret() {
        let mut tab = Tab::new(Buffer::from_text("a\n   b\nc"));
        let mut bar = FindBar::new();
        bar.open(BarMode::Find, &tab);
        bar.query.regex = true;
        bar.query.pattern = "(".into();
        bar.select_from_caret(&mut tab);
        assert!(bar.error.is_some());
        bar.open(BarMode::GoToLine, &tab);
        bar.goto = "2".into();
        bar.go_to_line(&mut tab, 3);
        assert_eq!(tab.editor.selection, Selection::caret(5));
        assert_eq!(bar.mode, BarMode::Hidden);
    }
}
