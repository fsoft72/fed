//! Custom editor widget: renders a `Tab` and translates egui input into `EditCommand`s.

use std::ops::Range;
use std::sync::Arc;

use eframe::egui::{
    self, Align2, CursorIcon, EventFilter, FontId, Galley, Key, Modifiers, Pos2, Rect, Sense,
    TextFormat, Ui, epaint::text::cursor::CCursor, pos2, text::LayoutJob, vec2,
};
use ropey::Rope;

use crate::buffer::{Buffer, DirtyLines};
use crate::editing::{EditCommand, EditSettings};
use crate::tab::Tab;
use crate::text_nav::{self, CharClass};
use crate::vim::{Mode, VimKey};

/// Horizontal padding on each side of the line numbers.
const GUTTER_PAD: f32 = 8.0;
/// Gap between the gutter and the text, and after the text.
const TEXT_PAD: f32 = 4.0;
/// Minimum number of digits reserved for line numbers.
const MIN_GUTTER_DIGITS: usize = 3;
/// Caret width in points.
const CARET_WIDTH: f32 = 2.0;
/// Opacity of the vim block caret, so the char under it stays readable.
const BLOCK_CARET_ALPHA: f32 = 0.6;
/// Smallest width used for wrapping text.
const MIN_WRAP_WIDTH: f32 = 20.0;
/// Extra horizontal room (in chars) kept right of the caret and the longest line.
const SCROLL_MARGIN_CHARS: f32 = 2.0;

/// Options controlling how the editor widget renders and reacts to input.
#[derive(Clone, Debug)]
pub struct ViewOptions {
    /// Font used for text and line numbers.
    pub font: FontId,
    /// Show the line number gutter.
    pub line_numbers: bool,
    /// Wrap long lines at the view width (no horizontal scrolling).
    pub word_wrap: bool,
    /// Indentation settings for editing commands.
    pub edit: EditSettings,
    /// Process keyboard events (false while a modal dialog owns the keyboard).
    pub keys_enabled: bool,
    /// Vim mode is on: draw the Visual selection and a block caret outside Insert mode.
    pub vim: bool,
}

/// Formatting of one line: char ranges within the line with their text format.
#[derive(Clone, Debug, Default)]
pub struct LineDecor {
    /// Non-overlapping char ranges (within the line) and their formats.
    pub spans: Vec<(Range<usize>, TextFormat)>,
}

/// Supplies per-line formatting (syntax highlighting, spell underlines, find matches).
///
/// Timing: `show_editor` applies this frame's keyboard edits first, then calls `prepare` with
/// the updated buffer, then calls `decorate` for each line it lays out. Decorators that cache
/// per-buffer state should refresh it in `prepare` so no stale spans are painted in the frame
/// of an edit. When the buffer changed, `show_editor` also requests a repaint so state
/// refreshed asynchronously is shown promptly.
pub trait Decorator {
    /// Called once per frame after input handling and before any `decorate` call.
    fn prepare(&mut self, _buffer: &Buffer) {}

    /// Returns formatted char ranges for `line` (without its newline). Ranges are char
    /// indices within the line; gaps are drawn with `base`.
    fn decorate(
        &mut self,
        line_idx: usize,
        line: &str,
        base: &TextFormat,
    ) -> Vec<(Range<usize>, TextFormat)>;
}

/// Applies `apply` to the parts of `spans` covered by `ranges`, splitting spans at the range
/// borders. `ranges` are sorted, non-overlapping char ranges in the same coordinates as the
/// spans. Order and non-overlap of the spans are preserved.
pub fn overlay_format(
    spans: Vec<(Range<usize>, TextFormat)>,
    ranges: &[Range<usize>],
    apply: impl Fn(&mut TextFormat),
) -> Vec<(Range<usize>, TextFormat)> {
    if ranges.is_empty() {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len() + ranges.len() * 2);
    for (span, format) in spans {
        let mut pos = span.start;
        for r in ranges
            .iter()
            .filter(|r| r.end > span.start && r.start < span.end)
        {
            let (a, b) = (r.start.max(span.start), r.end.min(span.end));
            if a > pos {
                out.push((pos..a, format.clone()));
            }
            let mut changed = format.clone();
            apply(&mut changed);
            out.push((a..b, changed));
            pos = b;
        }
        if pos < span.end || span.is_empty() {
            out.push((pos..span.end, format));
        }
    }
    out
}

/// Decorator drawing every line with the base format.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlainDecorator;

impl Decorator for PlainDecorator {
    /// Returns one span covering the whole line with `base`.
    fn decorate(
        &mut self,
        _line_idx: usize,
        line: &str,
        base: &TextFormat,
    ) -> Vec<(Range<usize>, TextFormat)> {
        vec![(0..line.chars().count(), base.clone())]
    }
}

/// Keyboard event hook for `show_editor` (`&mut dyn FnMut(&egui::Event, &mut Tab) -> bool`);
/// returning true means the event was handled and the widget skips it.
pub type KeyHandler<'a> = dyn FnMut(&egui::Event, &mut Tab) -> bool + 'a;

/// What happened in the editor widget during one frame.
#[derive(Clone, Debug, Default)]
pub struct ViewOutput {
    /// Text to put on the clipboard (from Copy or Cut).
    pub clipboard: Option<String>,
    /// Word under a right click: char range in the buffer and its text.
    pub context_word: Option<(Range<usize>, String)>,
    /// True when the buffer content changed this frame.
    pub changed: bool,
    /// True when a mouse press, drag or multi-click set the selection this frame.
    pub mouse_selected: bool,
}

/// Per-tab state kept by the widget between frames.
#[derive(Default)]
pub struct ViewState {
    /// The widget held keyboard focus since the last `request_focus` (or since creation).
    focus_acquired: bool,
    /// Char index hit by the last primary press (before any vim clamping of the caret).
    press_pos: Option<usize>,
    /// The pointer left the press position since the last press.
    dragged: bool,
    /// Longest line display width in columns, updated from the edited lines.
    longest: Option<LongestLine>,
    /// Wrapped line heights (word wrap only).
    wrap: WrapCache,
    /// Distance of the caret top from the viewport top in the last frame.
    caret_offset: Option<f32>,
    /// Set by PageUp/PageDown: the next frame scrolls so the caret sits this far from the
    /// viewport top, so the view moves by the same page as the caret.
    page_anchor: Option<f32>,
}

impl ViewState {
    /// Makes the widget take keyboard focus the next time it is shown.
    pub(crate) fn request_focus(&mut self) {
        self.focus_acquired = false;
    }
}

/// The longest line of a buffer revision (no word wrap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LongestLine {
    /// Buffer revision it was computed for.
    revision: u64,
    /// Tab width used to measure.
    tab_width: usize,
    /// Display width in columns (tabs expanded).
    cols: usize,
    /// Index of the longest line.
    line: usize,
}

/// Cached line geometry for word wrap mode.
#[derive(Default)]
struct WrapCache {
    /// Font, wrap width and tab width the heights were measured with.
    params: Option<(FontId, f32, usize)>,
    /// Buffer revision `heights` and `tops` were computed for.
    revision: Option<u64>,
    /// Wrapped height of every line (one entry per buffer line).
    heights: Vec<f32>,
    /// Top of every line, plus the total height as the last element.
    tops: Vec<f32>,
}

/// Font and layout measurements for one frame.
struct Metrics {
    /// Height of one text row.
    row_height: f32,
    /// Advance of a digit, used as the nominal char width.
    char_width: f32,
    /// Width of the line number gutter (0 when hidden).
    gutter_width: f32,
    /// Base format for text.
    base: TextFormat,
    /// Tab width in columns.
    tab_width: usize,
}

impl Metrics {
    /// Measures the font in `opts` for a buffer with `line_count` lines.
    fn new(ui: &Ui, opts: &ViewOptions, line_count: usize) -> Metrics {
        let (row_height, char_width) = ui
            .ctx()
            .fonts_mut(|f| (f.row_height(&opts.font), f.glyph_width(&opts.font, '0')));
        // Whole physical pixels keep line tops aligned (no seams between selection rects).
        let ppp = ui.ctx().pixels_per_point();
        let row_height = (row_height * ppp).ceil() / ppp;
        let digits = line_count.to_string().len().max(MIN_GUTTER_DIGITS);
        let gutter_width = if opts.line_numbers {
            digits as f32 * char_width + 2.0 * GUTTER_PAD
        } else {
            0.0
        };
        let base = TextFormat {
            font_id: opts.font.clone(),
            color: ui.visuals().text_color(),
            ..Default::default()
        };
        Metrics {
            row_height,
            char_width,
            gutter_width,
            base,
            tab_width: opts.edit.tab_width.max(1),
        }
    }

    /// X offset of the text from the content left edge.
    fn text_x(&self) -> f32 {
        self.gutter_width + TEXT_PAD
    }
}

/// Vertical placement of lines in content coordinates.
enum RowMap {
    /// Every line is one row high.
    Fixed { row_height: f32, lines: usize },
    /// Variable heights: tops of lines plus the total height last.
    Wrapped(Vec<f32>),
}

impl RowMap {
    /// Top of `line`.
    fn top(&self, line: usize) -> f32 {
        match self {
            RowMap::Fixed { row_height, .. } => line as f32 * row_height,
            RowMap::Wrapped(tops) => tops[line.min(tops.len() - 1)],
        }
    }

    /// Total content height.
    fn total(&self) -> f32 {
        match self {
            RowMap::Fixed { row_height, lines } => *lines as f32 * row_height,
            RowMap::Wrapped(tops) => *tops.last().unwrap_or(&0.0),
        }
    }

    /// Line containing content y coordinate `y`, clamped to the document.
    fn line_at(&self, y: f32) -> usize {
        match self {
            RowMap::Fixed { row_height, lines } => {
                let line = (y / row_height).floor().max(0.0) as usize;
                line.min(lines.saturating_sub(1))
            }
            RowMap::Wrapped(tops) => line_at_y(tops, y),
        }
    }
}

/// Widget id of the editor of the tab with id `tab_id` (to query keyboard focus).
pub fn editor_id(tab_id: u64) -> egui::Id {
    egui::Id::unique(("fed_editor", tab_id))
}

/// Shows the editor for `tab` filling the available space. `deco` formats visible lines.
/// When `key_handler` returns true for a keyboard event the widget skips it (vim hook).
pub fn show_editor(
    ui: &mut Ui,
    tab: &mut Tab,
    opts: &ViewOptions,
    deco: &mut dyn Decorator,
    mut key_handler: Option<&mut KeyHandler<'_>>,
) -> ViewOutput {
    let mut out = ViewOutput::default();
    let revision_before = tab.buffer.revision();
    let id = editor_id(tab.id);
    let metrics = Metrics::new(ui, opts, tab.buffer.rope().len_lines());
    if !tab.view.focus_acquired {
        // A click elsewhere in the same frame (a tab bar or dialog button) makes egui
        // surrender a fresh focus request, so keep asking until the focus has stuck.
        if ui.memory(|m| m.has_focus(id)) {
            tab.view.focus_acquired = true;
        } else {
            ui.memory_mut(|m| m.request_focus(id));
            ui.ctx().request_repaint();
        }
    }
    let focused = ui.memory(|m| m.has_focus(id));
    if focused {
        let filter = EventFilter {
            tab: true,
            horizontal_arrows: true,
            vertical_arrows: true,
            escape: true,
        };
        ui.memory_mut(|m| m.set_focus_lock_filter(id, filter));
        if opts.keys_enabled {
            let page = page_lines(ui.available_height(), metrics.row_height);
            handle_keyboard(ui, tab, opts, page, &mut key_handler, &mut out);
        }
    }
    deco.prepare(&tab.buffer);
    let scroll = if opts.word_wrap {
        egui::ScrollArea::vertical()
    } else {
        egui::ScrollArea::both()
    };
    scroll
        .id_salt(("fed_scroll", tab.id))
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let mut frame = Frame {
                id,
                tab,
                opts,
                deco,
                m: &metrics,
                focused,
            };
            frame.run(ui, viewport, &mut out);
        });
    out.changed = tab.buffer.revision() != revision_before;
    if out.changed {
        ui.ctx().request_repaint();
    }
    out
}

/// Translates this frame's keyboard events into commands on `tab`.
fn handle_keyboard(
    ui: &Ui,
    tab: &mut Tab,
    opts: &ViewOptions,
    page: usize,
    key_handler: &mut Option<&mut KeyHandler<'_>>,
    out: &mut ViewOutput,
) {
    let events = ui.input(|i| i.events.clone());
    for ev in &events {
        if !is_keyboard_event(ev) {
            continue;
        }
        if let Some(up) = page_key(ev) {
            tab.view.page_anchor = Some(tab.view.caret_offset.unwrap_or(0.0));
            if let Some(handler) = key_handler.as_mut() {
                // Vim mode: move the caret one line at a time so every mode clamps it.
                let arrow = if up { Key::ArrowUp } else { Key::ArrowDown };
                for _ in 0..page {
                    handler(&key_event(arrow), tab);
                }
                continue;
            }
        }
        if let Some(handler) = key_handler.as_mut()
            && handler(ev, tab)
        {
            tab.scroll_to_cursor = true;
            continue;
        }
        let Some(cmd) = event_to_command(ev, page) else {
            continue;
        };
        if let Some(text) = tab.apply(cmd, &opts.edit) {
            out.clipboard = Some(text);
        }
    }
}

/// Returns `Some(true)` for a PageUp press and `Some(false)` for a PageDown press (Shift
/// allowed, to extend the selection), `None` for any other event.
fn page_key(ev: &egui::Event) -> Option<bool> {
    let egui::Event::Key {
        key,
        pressed: true,
        modifiers,
        ..
    } = ev
    else {
        return None;
    };
    if modifiers.ctrl || modifiers.alt || modifiers.mac_cmd {
        return None;
    }
    match key {
        Key::PageUp => Some(true),
        Key::PageDown => Some(false),
        _ => None,
    }
}

/// A plain press of `key` without modifiers.
fn key_event(key: Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}

/// Translates a keyboard event into vim keys: text gives one `Char` per char; Escape, Enter,
/// Backspace, Tab and the arrows map to their keys; Ctrl plus a letter gives `Ctrl`. Other
/// events give nothing.
pub fn egui_event_to_vim_keys(ev: &egui::Event) -> Vec<VimKey> {
    match ev {
        egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => text
            .chars()
            .filter(|c| !c.is_control())
            .map(VimKey::Char)
            .collect(),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => key_to_vim_key(*key, *modifiers).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Translates a keyboard event that stands for exactly one vim key (see
/// `egui_event_to_vim_keys`); `None` for other events and for multi-char text.
pub fn egui_event_to_vim_key(ev: &egui::Event) -> Option<VimKey> {
    let mut keys = egui_event_to_vim_keys(ev);
    if keys.len() != 1 {
        return None;
    }
    keys.pop()
}

/// Maps a key press to a vim key; printable keys without Ctrl arrive as text events instead.
fn key_to_vim_key(key: Key, mods: Modifiers) -> Option<VimKey> {
    if mods.ctrl {
        let name = key.name();
        let mut chars = name.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => {
                Some(VimKey::Ctrl(c.to_ascii_lowercase()))
            }
            _ => None,
        };
    }
    let vim_key = match key {
        Key::Escape => VimKey::Esc,
        Key::Enter => VimKey::Enter,
        Key::Backspace => VimKey::Backspace,
        Key::Tab if !mods.shift => VimKey::Tab,
        Key::ArrowLeft => VimKey::Left,
        Key::ArrowRight => VimKey::Right,
        Key::ArrowUp => VimKey::Up,
        Key::ArrowDown => VimKey::Down,
        _ => return None,
    };
    Some(vim_key)
}

/// True for events the editor (or its key handler) consumes from the keyboard.
fn is_keyboard_event(ev: &egui::Event) -> bool {
    matches!(
        ev,
        egui::Event::Text(_)
            | egui::Event::Ime(egui::ImeEvent::Commit(_))
            | egui::Event::Copy
            | egui::Event::Cut
            | egui::Event::Paste(_)
            | egui::Event::Key { pressed: true, .. }
    )
}

/// Maps one keyboard event to an editing command; `page` is the PageUp/PageDown size.
fn event_to_command(ev: &egui::Event, page: usize) -> Option<EditCommand> {
    match ev {
        egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
            let text: String = text.chars().filter(|c| !c.is_control()).collect();
            (!text.is_empty()).then_some(EditCommand::InsertText(text))
        }
        egui::Event::Copy => Some(EditCommand::Copy),
        egui::Event::Cut => Some(EditCommand::Cut),
        egui::Event::Paste(text) => (!text.is_empty()).then(|| EditCommand::Paste(text.clone())),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => key_to_command(*key, *modifiers, page),
        _ => None,
    }
}

/// Maps a key press to an editing command using the platform conventions
/// (`Cmd`/`Alt` arrows on macOS, `Ctrl` arrows elsewhere). `page` is the page size in lines.
pub fn key_to_command(key: Key, mods: Modifiers, page: usize) -> Option<EditCommand> {
    key_to_command_for(key, mods, page, cfg!(target_os = "macos"))
}

/// `key_to_command` with an explicit platform flag (testable on every OS).
fn key_to_command_for(key: Key, mods: Modifiers, page: usize, mac: bool) -> Option<EditCommand> {
    let select = mods.shift;
    let word = if mac { mods.alt } else { mods.ctrl };
    let cmd_arrow = mac && mods.mac_cmd;
    let doc_jump = if mac { mods.mac_cmd } else { mods.ctrl };
    let plain = !mods.ctrl && !mods.alt && !mods.mac_cmd;
    let cmd = match key {
        Key::ArrowLeft if cmd_arrow => EditCommand::Home { select },
        Key::ArrowLeft if word => EditCommand::WordLeft { select },
        Key::ArrowLeft => EditCommand::Left { select },
        Key::ArrowRight if cmd_arrow => EditCommand::End { select },
        Key::ArrowRight if word => EditCommand::WordRight { select },
        Key::ArrowRight => EditCommand::Right { select },
        Key::ArrowUp if cmd_arrow => EditCommand::DocStart { select },
        Key::ArrowUp => EditCommand::Up { select },
        Key::ArrowDown if cmd_arrow => EditCommand::DocEnd { select },
        Key::ArrowDown => EditCommand::Down { select },
        Key::Home if doc_jump => EditCommand::DocStart { select },
        Key::Home => EditCommand::Home { select },
        Key::End if doc_jump => EditCommand::DocEnd { select },
        Key::End => EditCommand::End { select },
        Key::PageUp => EditCommand::PageUp {
            select,
            lines: page,
        },
        Key::PageDown => EditCommand::PageDown {
            select,
            lines: page,
        },
        Key::Enter if plain => EditCommand::Newline,
        Key::Backspace => EditCommand::Backspace,
        Key::Delete => EditCommand::Delete,
        Key::Tab if plain && mods.shift => EditCommand::Outdent,
        Key::Tab if plain => EditCommand::Indent,
        _ => return None,
    };
    Some(cmd)
}

/// Number of lines moved by PageUp/PageDown: visible rows minus one, at least one.
fn page_lines(view_height: f32, row_height: f32) -> usize {
    if row_height <= 0.0 {
        return 1;
    }
    ((view_height / row_height).floor() as usize)
        .saturating_sub(1)
        .max(1)
}

/// Index of the line containing `y` given line tops with the total height last.
fn line_at_y(tops: &[f32], y: f32) -> usize {
    let lines = tops.len().saturating_sub(1);
    if lines == 0 {
        return 0;
    }
    tops[..lines]
        .partition_point(|&top| top <= y)
        .saturating_sub(1)
        .min(lines - 1)
}

/// Display width in columns of `chars` (a line, newline excluded from the count); tabs
/// advance to the next multiple of `tab_width`.
fn display_width(chars: impl Iterator<Item = char>, tab_width: usize) -> usize {
    chars
        .filter(|&c| c != '\n')
        .fold(0, |col, c| col + text_nav::char_advance(c, col, tab_width))
}

/// Converts a char column of `line` to a display column (tabs advance to the next tab stop).
fn display_col(line: &str, col: usize, tab_width: usize) -> usize {
    display_width(line.chars().take(col), tab_width)
}

/// Converts a display column to the nearest char column of `line`.
fn buffer_col(line: &str, dcol: usize, tab_width: usize) -> usize {
    let mut x = 0;
    for (i, c) in line.chars().enumerate() {
        let w = text_nav::char_advance(c, x, tab_width);
        if dcol < x + w {
            return if dcol - x <= w / 2 { i } else { i + 1 };
        }
        x += w;
    }
    line.chars().count()
}

/// Returns `line` with every tab replaced by the spaces up to the next tab stop, plus the
/// byte offset in the result of every char of `line` (and of its end, as the last entry).
fn expand_tabs(line: &str, tab_width: usize) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(line.len());
    let mut offsets = Vec::with_capacity(line.len() + 1);
    let mut col = 0;
    for c in line.chars() {
        offsets.push(out.len());
        let w = text_nav::char_advance(c, col, tab_width);
        match c {
            '\t' => out.extend(std::iter::repeat_n(' ', w)),
            _ => out.push(c),
        }
        col += w;
    }
    offsets.push(out.len());
    (out, offsets)
}

/// Builds the layout job for `line`: `spans` (char ranges) on top of `base`, tabs expanded.
fn build_layout_job(
    line: &str,
    spans: &[(Range<usize>, TextFormat)],
    base: &TextFormat,
    tab_width: usize,
    wrap_width: Option<f32>,
    row_height: f32,
) -> LayoutJob {
    let mut job = LayoutJob {
        first_row_min_height: row_height,
        ..Default::default()
    };
    job.wrap.max_width = wrap_width.unwrap_or(f32::INFINITY);
    let (expanded, offsets) = expand_tabs(line, tab_width);
    let len = offsets.len() - 1;
    let mut append = |range: Range<usize>, format: &TextFormat| {
        let text = &expanded[offsets[range.start]..offsets[range.end]];
        job.append(text, 0.0, format.clone());
    };
    let mut sorted: Vec<&(Range<usize>, TextFormat)> = spans.iter().collect();
    sorted.sort_by_key(|(r, _)| r.start);
    let mut pos = 0;
    for (range, format) in sorted {
        let start = range.start.max(pos).min(len);
        let end = range.end.min(len);
        if start >= end {
            continue;
        }
        if start > pos {
            append(pos..start, base);
        }
        append(start..end, format);
        pos = end;
    }
    if pos < len || len == 0 {
        append(pos..len, base);
    }
    job
}

/// Returns the text of `line` without its trailing newline.
fn line_text(rope: &Rope, line: usize) -> String {
    let mut text = rope.line(line).to_string();
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Measures the wrapped height of `line` laid out `width` points wide.
fn measure_line(
    fonts: &mut egui::epaint::FontsView<'_>,
    rope: &Rope,
    line: usize,
    m: &Metrics,
    width: f32,
) -> f32 {
    let text = line_text(rope, line);
    if text.is_empty() {
        return m.row_height;
    }
    let spans = [(0..text.chars().count(), m.base.clone())];
    let job = build_layout_job(
        &text,
        &spans,
        &m.base,
        m.tab_width,
        Some(width),
        m.row_height,
    );
    fonts.layout_job(job).size().y.max(m.row_height)
}

impl WrapCache {
    /// Brings line heights and tops up to date with the buffer. Only lines edited since the
    /// last update are measured again (all of them when the font, wrap width or tab width
    /// changed), and tops are re-summed from the first edited line.
    fn update(&mut self, ui: &Ui, buffer: &Buffer, m: &Metrics, width: f32) {
        let params = (m.base.font_id.clone(), width, m.tab_width);
        if self.params.as_ref() != Some(&params) {
            self.params = Some(params);
            self.revision = None;
        }
        if self.revision == Some(buffer.revision()) {
            return;
        }
        let rope = buffer.rope();
        ui.ctx().fonts_mut(|fonts| {
            self.refresh(buffer, |line| measure_line(fonts, rope, line, m, width));
        });
    }

    /// Re-measures the lines edited since `revision` with `measure` (every line when there
    /// is no previous revision) and rebuilds `tops` from the first edited line.
    fn refresh(&mut self, buffer: &Buffer, mut measure: impl FnMut(usize) -> f32) {
        if self.revision == Some(buffer.revision()) {
            return;
        }
        let lines = buffer.rope().len_lines();
        let everything = DirtyLines {
            start: 0,
            end: lines,
            old_end: usize::MAX,
        };
        let dirty = match self.revision {
            Some(rev) => buffer.dirty_lines_since(rev).unwrap_or(everything),
            None => everything,
        };
        let old_end = dirty.old_end.min(self.heights.len());
        let consistent = dirty.start <= old_end
            && dirty.end <= lines
            && self.heights.len() - old_end + dirty.end == lines;
        let dirty = match consistent {
            true => DirtyLines { old_end, ..dirty },
            false => DirtyLines {
                old_end: self.heights.len(),
                ..everything
            },
        };
        let fresh: Vec<f32> = (dirty.start..dirty.end).map(&mut measure).collect();
        self.heights.splice(dirty.start..dirty.old_end, fresh);
        self.tops.truncate(dirty.start);
        let mut y = match dirty.start {
            0 => 0.0,
            n => self.tops[n - 1] + self.heights[n - 1],
        };
        for h in &self.heights[dirty.start..] {
            self.tops.push(y);
            y += h;
        }
        self.tops.push(y);
        self.revision = Some(buffer.revision());
    }
}

/// Returns the longest of `lines` (display columns, line index), the first one on ties;
/// `(0, lines.start)` for an empty range.
fn longest_in(rope: &Rope, lines: Range<usize>, tab_width: usize) -> (usize, usize) {
    let first = lines.start;
    lines
        .map(|i| (display_width(rope.line(i).chars(), tab_width), i))
        .fold((0, first), |best, cur| match cur.0 > best.0 {
            true => cur,
            false => best,
        })
}

/// Returns the longest line of `buffer`, reusing `old` when possible: only the lines edited
/// since `old` are measured, and the whole buffer is rescanned only when the old longest line
/// was edited and no edited line is at least as long.
fn longest_line(buffer: &Buffer, old: Option<LongestLine>, tab_width: usize) -> LongestLine {
    let rope = buffer.rope();
    let revision = buffer.revision();
    let rescan = || {
        let (cols, line) = longest_in(rope, 0..rope.len_lines(), tab_width);
        LongestLine {
            revision,
            tab_width,
            cols,
            line,
        }
    };
    let Some(old) = old.filter(|o| o.tab_width == tab_width) else {
        return rescan();
    };
    let Some(dirty) = buffer.dirty_lines_since(old.revision) else {
        return old;
    };
    let (cols, line) = longest_in(rope, dirty.start..dirty.end, tab_width);
    let replaced = (dirty.start..dirty.old_end).contains(&old.line);
    if cols > old.cols || (replaced && cols == old.cols) {
        return LongestLine {
            revision,
            tab_width,
            cols,
            line,
        };
    }
    if replaced {
        return rescan();
    }
    let line = match old.line >= dirty.old_end {
        true => old.line - dirty.old_end + dirty.end,
        false => old.line,
    };
    LongestLine {
        revision,
        line,
        ..old
    }
}

/// Where the content sits this frame: line placement, screen origin and visible area.
struct Placement {
    /// Vertical placement of lines.
    rows: RowMap,
    /// Screen position of the content's top-left corner.
    origin: Pos2,
    /// Visible part of the content, in content coordinates.
    viewport: Rect,
    /// Wrap width when word wrap is on.
    wrap_width: Option<f32>,
}

/// Everything needed to lay out, hit-test and paint the editor content for one frame.
struct Frame<'a> {
    id: egui::Id,
    tab: &'a mut Tab,
    opts: &'a ViewOptions,
    deco: &'a mut dyn Decorator,
    m: &'a Metrics,
    focused: bool,
}

impl Frame<'_> {
    /// Lays out the visible lines, handles the mouse and paints. `viewport` is the visible
    /// part of the content in content coordinates.
    fn run(&mut self, ui: &mut Ui, viewport: Rect, out: &mut ViewOutput) {
        let origin = ui.max_rect().min;
        let text_x = self.m.text_x();
        let wrap_width = self
            .opts
            .word_wrap
            .then(|| (viewport.width() - text_x - TEXT_PAD).max(MIN_WRAP_WIDTH));
        let rows = match wrap_width {
            Some(width) => {
                self.tab
                    .view
                    .wrap
                    .update(ui, &self.tab.buffer, self.m, width);
                // Borrowed for the frame (moved back at the end), never cloned.
                RowMap::Wrapped(std::mem::take(&mut self.tab.view.wrap.tops))
            }
            None => RowMap::Fixed {
                row_height: self.m.row_height,
                lines: self.tab.buffer.rope().len_lines(),
            },
        };

        // Content size and interaction.
        let width = match wrap_width {
            Some(_) => viewport.width(),
            None => {
                let cols = self.max_line_cols() as f32 * self.m.char_width;
                (text_x + cols + SCROLL_MARGIN_CHARS * self.m.char_width).max(viewport.width())
            }
        };
        let height = rows.total().max(viewport.height());
        let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let response = ui.interact(rect, self.id, Sense::click_and_drag());
        let pl = Placement {
            rows,
            origin,
            viewport,
            wrap_width,
        };
        self.handle_mouse(ui, &response, &pl, out);

        // Visible lines (after the mouse so the frame shows the new selection).
        let first = pl.rows.line_at(viewport.min.y);
        let last = pl.rows.line_at(viewport.max.y);
        let galleys: Vec<(usize, Arc<Galley>)> = (first..=last)
            .map(|i| (i, self.layout_line(ui, i, wrap_width)))
            .collect();
        if wrap_width.is_none() {
            // Wide glyphs can make a visible line wider than its column estimate.
            let widest = galleys.iter().map(|(_, g)| g.size().x).fold(0.0, f32::max);
            ui.set_min_width(text_x + widest + SCROLL_MARGIN_CHARS * self.m.char_width);
        }
        self.paint(ui, &galleys, &pl);

        let caret = self.caret_rect(ui, &galleys, &pl);
        let caret_offset = caret.min.y - origin.y - viewport.min.y;
        self.tab.view.caret_offset = Some(caret_offset);
        if let Some(anchor) = self.tab.view.page_anchor.take() {
            // Keep the caret on the same screen row (fully visible): the view follows the page.
            self.tab.scroll_to_cursor = false;
            let anchor = anchor.clamp(0.0, (viewport.height() - caret.height()).max(0.0));
            // A positive scroll delta moves the content down, i.e. scrolls the view up.
            ui.scroll_with_delta_animation(
                vec2(0.0, anchor - caret_offset),
                egui::style::ScrollAnimation::none(),
            );
        }
        if self.tab.scroll_to_cursor {
            self.tab.scroll_to_cursor = false;
            let margin = SCROLL_MARGIN_CHARS * self.m.char_width;
            let target = Rect::from_min_max(
                pos2(caret.min.x - text_x, caret.min.y),
                pos2(caret.max.x + margin, caret.max.y),
            );
            // Caret jumps (`G`, `:N`, find next, ...) land instantly, never with a smooth scroll.
            ui.scroll_to_rect_animation(target, None, egui::style::ScrollAnimation::none());
        }
        if self.focused {
            self.paint_caret(ui, caret);
            let to_global = ui
                .ctx()
                .layer_transform_to_global(ui.layer_id())
                .unwrap_or_default();
            let clip = ui.clip_rect();
            ui.output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect: to_global * clip,
                    cursor_rect: to_global * caret,
                    should_interrupt_composition: false,
                });
            });
        }
        if let RowMap::Wrapped(tops) = pl.rows {
            self.tab.view.wrap.tops = tops;
        }
    }

    /// Display width in columns (tabs expanded) of the longest line, updated incrementally
    /// from the lines edited since the last frame.
    fn max_line_cols(&mut self) -> usize {
        let longest = longest_line(&self.tab.buffer, self.tab.view.longest, self.m.tab_width);
        self.tab.view.longest = Some(longest);
        longest.cols
    }

    /// Lays out `line` with the decorator formats.
    fn layout_line(&mut self, ui: &Ui, line: usize, wrap_width: Option<f32>) -> Arc<Galley> {
        let text = line_text(self.tab.buffer.rope(), line);
        let spans = self.deco.decorate(line, &text, &self.m.base);
        let job = build_layout_job(
            &text,
            &spans,
            &self.m.base,
            self.m.tab_width,
            wrap_width,
            self.m.row_height,
        );
        ui.painter().layout_job(job)
    }

    /// Returns the galley of `line`, reusing an already laid out visible one.
    fn galley_for(
        &mut self,
        ui: &Ui,
        galleys: &[(usize, Arc<Galley>)],
        line: usize,
        wrap_width: Option<f32>,
    ) -> Arc<Galley> {
        match galleys.iter().find(|(i, _)| *i == line) {
            Some((_, g)) => g.clone(),
            None => self.layout_line(ui, line, wrap_width),
        }
    }

    /// Maps a screen position to a char index in the buffer.
    fn hit_test(&mut self, ui: &Ui, pos: Pos2, pl: &Placement) -> usize {
        let local = pos - pl.origin;
        let line = pl.rows.line_at(local.y);
        let galley = self.layout_line(ui, line, pl.wrap_width);
        // Text hidden under the gutter is not clickable.
        let x = local.x.max(pl.viewport.min.x + self.m.gutter_width) - self.m.text_x();
        let cursor = galley.cursor_from_pos(vec2(x, local.y - pl.rows.top(line)));
        let rope = self.tab.buffer.rope();
        let col = buffer_col(&line_text(rope, line), cursor.index.0, self.m.tab_width);
        rope.line_to_char(line) + col
    }

    /// Handles clicks, drags, double/triple clicks and right clicks.
    fn handle_mouse(
        &mut self,
        ui: &Ui,
        response: &egui::Response,
        pl: &Placement,
        out: &mut ViewOutput,
    ) {
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Text);
        }
        let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        let (pressed, down, shift, press_origin) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.modifiers.shift,
                i.pointer.press_origin(),
            )
        });
        let edit = self.opts.edit;
        let before = self.tab.editor.selection;
        if response.hovered() && pressed {
            response.request_focus();
            // The pointer may already have moved within this frame: anchor at the press.
            let pos = self.hit_test(ui, press_origin.unwrap_or(pointer), pl);
            self.tab.view.press_pos = Some(pos);
            self.tab.view.dragged = false;
            self.tab
                .apply(EditCommand::SetCaret { pos, select: shift }, &edit);
        }
        if down && response.is_pointer_button_down_on() {
            let pos = self.hit_test(ui, pointer, pl);
            let view = &mut self.tab.view;
            view.dragged |= view.press_pos != Some(pos);
            let head = self.tab.editor.selection.head;
            if drag_moves_head(pos, head, self.tab.view.dragged) {
                self.tab
                    .apply(EditCommand::SetCaret { pos, select: true }, &edit);
            }
        }
        if response.triple_clicked() {
            let pos = self.hit_test(ui, pointer, pl);
            self.tab.apply(EditCommand::SelectLineAt(pos), &edit);
        } else if response.double_clicked() {
            let pos = self.hit_test(ui, pointer, pl);
            self.tab.apply(EditCommand::SelectWordAt(pos), &edit);
        }
        let clicked = response.hovered() && pressed;
        if clicked || response.double_clicked() || self.tab.editor.selection != before {
            out.mouse_selected = true;
        }
        if response.secondary_clicked() {
            let pos = self.hit_test(ui, pointer, pl);
            out.context_word = self.word_at(pos);
        }
    }

    /// Returns the word (alphanumeric run) at `pos` with its char range.
    fn word_at(&self, pos: usize) -> Option<(Range<usize>, String)> {
        let rope = self.tab.buffer.rope();
        let range = text_nav::word_range_at(rope, pos);
        if range.is_empty()
            || text_nav::char_class(rope.char(range.start), false) != CharClass::Word
        {
            return None;
        }
        Some((range.clone(), rope.slice(range).to_string()))
    }

    /// Paints selection, text and the line number gutter for the visible lines.
    fn paint(&self, ui: &Ui, galleys: &[(usize, Arc<Galley>)], pl: &Placement) {
        let (rows, origin, viewport) = (&pl.rows, pl.origin, pl.viewport);
        let painter = ui.painter();
        let rope = self.tab.buffer.rope();
        let sel = self.selection_to_paint();
        let sel_color = ui.visuals().selection.bg_fill;
        let text_color = ui.visuals().text_color();
        for (line, galley) in galleys {
            let pos = origin + vec2(self.m.text_x(), rows.top(*line));
            let ls = rope.line_to_char(*line);
            let text = line_text(rope, *line);
            let le = ls + text.chars().count();
            if !sel.is_empty() && sel.start <= le && sel.end > ls {
                let a = display_col(&text, sel.start.max(ls) - ls, self.m.tab_width);
                let b = display_col(&text, sel.end.min(le) - ls, self.m.tab_width);
                let extra = if sel.end > le { self.m.char_width } else { 0.0 };
                for r in selection_rects(galley, a, b, extra) {
                    painter.rect_filled(r.translate(pos.to_vec2()), 0.0, sel_color);
                }
            }
            painter.galley(pos, galley.clone(), text_color);
        }
        if !self.opts.line_numbers {
            return;
        }
        let gutter = Rect::from_min_size(
            pos2(origin.x + viewport.min.x, origin.y + viewport.min.y),
            vec2(self.m.gutter_width, viewport.height()),
        );
        painter.rect_filled(gutter, 0.0, ui.visuals().panel_fill);
        let caret_line = rope.char_to_line(self.tab.editor.selection.head);
        let right = gutter.max.x - GUTTER_PAD;
        for (line, _) in galleys {
            let color = if *line == caret_line {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().weak_text_color()
            };
            let y = origin.y + rows.top(*line);
            painter.text(
                pos2(right, y),
                Align2::RIGHT_TOP,
                (line + 1).to_string(),
                self.m.base.font_id.clone(),
                color,
            );
        }
    }

    /// Char range painted as selected: the Visual selection in vim mode, else the selection.
    fn selection_to_paint(&self) -> Range<usize> {
        if !self.opts.vim {
            return self.tab.editor.selection.range();
        }
        let tab = &*self.tab;
        tab.vim
            .visual_range(&tab.buffer, &tab.editor)
            .unwrap_or_default()
    }

    /// True when the caret is drawn as a block (vim outside Insert mode).
    fn block_caret(&self) -> bool {
        self.opts.vim && self.tab.vim.mode() != Mode::Insert
    }

    /// Screen rect of the caret (zero width).
    fn caret_rect(&mut self, ui: &Ui, galleys: &[(usize, Arc<Galley>)], pl: &Placement) -> Rect {
        let rope = self.tab.buffer.rope();
        let (line, col) = text_nav::line_col(rope, self.tab.editor.selection.head);
        let dcol = display_col(&line_text(rope, line), col, self.m.tab_width);
        let galley = self.galley_for(ui, galleys, line, pl.wrap_width);
        let offset = pl.origin + vec2(self.m.text_x(), pl.rows.top(line));
        galley
            .pos_from_cursor(CCursor::new(dcol))
            .translate(offset.to_vec2())
    }

    /// Paints the caret at `caret` unless it is hidden under the gutter.
    fn paint_caret(&self, ui: &Ui, caret: Rect) {
        let gutter_right = ui.clip_rect().min.x + self.m.gutter_width;
        if caret.min.x < gutter_right {
            return;
        }
        let color = ui.visuals().text_cursor.stroke.color;
        if self.block_caret() {
            let rect = Rect::from_min_size(caret.min, vec2(self.block_width(ui), caret.height()));
            ui.painter()
                .rect_filled(rect, 0.0, color.gamma_multiply(BLOCK_CARET_ALPHA));
            return;
        }
        let rect = Rect::from_min_size(caret.min, vec2(CARET_WIDTH, caret.height()));
        ui.painter().rect_filled(rect, 0.0, color);
    }

    /// Width of the block caret: the char under the caret (tabs expanded), at least one column.
    fn block_width(&self, ui: &Ui) -> f32 {
        let rope = self.tab.buffer.rope();
        let head = self.tab.editor.selection.head;
        let c = (head < rope.len_chars()).then(|| rope.char(head));
        match c {
            Some('\t') => {
                let col = text_nav::display_col(rope, head, self.m.tab_width);
                self.m.char_width * text_nav::tab_advance(col, self.m.tab_width) as f32
            }
            Some(c) if c != '\n' => {
                let font = &self.m.base.font_id;
                ui.ctx()
                    .fonts_mut(|f| f.glyph_width(font, c))
                    .max(self.m.char_width)
            }
            _ => self.m.char_width,
        }
    }
}

/// True when a held primary button at char `pos` must move the selection head. Nothing moves
/// until the pointer has left the press position (`dragged`), so a caret that vim clamped
/// after the press (a click past the line end) does not turn the click into a selection.
fn drag_moves_head(pos: usize, head: usize, dragged: bool) -> bool {
    dragged && pos != head
}

/// Selection rectangles (galley coordinates) for display columns `a..b`, with `extra`
/// width appended after the last row when the selection includes the newline.
fn selection_rects(galley: &Galley, a: usize, b: usize, extra: f32) -> Vec<Rect> {
    let mut rects = Vec::new();
    let last = galley.rows.len().saturating_sub(1);
    let mut start = 0;
    for (ri, row) in galley.rows.iter().enumerate() {
        let r0 = start;
        let r1 = start + row.char_count_excluding_newline().0;
        start += row.char_count_including_newline().0;
        let s = a.max(r0);
        let e = b.min(r1);
        let tail = if ri == last { extra } else { 0.0 };
        if s > e || (s == e && tail == 0.0) {
            continue;
        }
        let x0 = row.pos.x + row.x_offset((s - r0).into());
        let x1 = row.pos.x + row.x_offset((e - r0).into()) + tail;
        rects.push(Rect::from_min_max(
            pos2(x0, row.min_y()),
            pos2(x1, row.max_y()),
        ));
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;
    const SHIFT: Modifiers = Modifiers::SHIFT;
    const CTRL: Modifiers = Modifiers::CTRL;

    /// Builds a wrap cache for `buffer` where a line is as tall as its char count (min 1).
    fn wrap_for(buffer: &Buffer) -> WrapCache {
        let mut wrap = WrapCache::default();
        wrap.refresh(buffer, |i| height_of(buffer, i));
        wrap
    }

    /// Fake wrapped height: the line length, at least 1.
    fn height_of(buffer: &Buffer, line: usize) -> f32 {
        buffer.rope().line(line).len_chars().max(1) as f32
    }

    #[test]
    fn wrap_cache_remeasures_only_edited_lines() {
        let mut buffer = Buffer::from_text("a\nbb\nccc\ndddd\neeeee");
        let mut wrap = wrap_for(&buffer);
        buffer.replace(3..3, "XX\nYYYYYYY\n");
        let mut measured = Vec::new();
        wrap.refresh(&buffer, |i| {
            measured.push(i);
            height_of(&buffer, i)
        });
        assert_eq!(measured, vec![1, 2, 3]);
        let fresh = wrap_for(&buffer);
        assert_eq!(wrap.heights, fresh.heights);
        assert_eq!(wrap.tops, fresh.tops);
        // Deleting lines shrinks the cache too.
        buffer.replace(0..8, "");
        wrap.refresh(&buffer, |i| height_of(&buffer, i));
        let fresh = wrap_for(&buffer);
        assert_eq!(wrap.heights, fresh.heights);
        assert_eq!(wrap.tops, fresh.tops);
        // Unchanged revision: nothing is measured.
        let mut calls = 0;
        wrap.refresh(&buffer, |_| {
            calls += 1;
            1.0
        });
        assert_eq!(calls, 0);
    }

    #[test]
    fn longest_line_updates_incrementally() {
        let mut buffer = Buffer::from_text("aaaa\nbb\naaaaaa\nc");
        let l = longest_line(&buffer, None, 4);
        assert_eq!((l.cols, l.line), (6, 2));
        // Grow a short line past the longest.
        buffer.replace(5..5, "bbbbbbbb");
        let l = longest_line(&buffer, Some(l), 4);
        assert_eq!((l.cols, l.line), (10, 1));
        // Insert lines before: the longest line index moves.
        buffer.replace(0..0, "x\ny\n");
        let l = longest_line(&buffer, Some(l), 4);
        assert_eq!((l.cols, l.line), (10, 3));
        // Shrink the longest line: rescan finds the next one.
        let start = buffer.rope().line_to_char(3);
        buffer.replace(start..start + 9, "");
        let l = longest_line(&buffer, Some(l), 4);
        assert_eq!((l.cols, l.line), (6, 4));
        // A tab width change rescans with the new width.
        buffer.replace(0..0, "\t\t");
        let l = longest_line(&buffer, Some(l), 8);
        assert_eq!((l.cols, l.line), (17, 0));
    }

    #[test]
    fn arrows_map_to_moves() {
        assert_eq!(
            key_to_command_for(Key::ArrowLeft, NONE, 10, false),
            Some(EditCommand::Left { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowRight, SHIFT, 10, false),
            Some(EditCommand::Right { select: true })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowLeft, CTRL, 10, false),
            Some(EditCommand::WordLeft { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowRight, CTRL | SHIFT, 10, false),
            Some(EditCommand::WordRight { select: true })
        );
    }

    #[test]
    fn mac_uses_alt_for_words_and_cmd_for_lines() {
        assert_eq!(
            key_to_command_for(Key::ArrowLeft, Modifiers::ALT, 10, true),
            Some(EditCommand::WordLeft { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowRight, Modifiers::MAC_CMD, 10, true),
            Some(EditCommand::End { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowUp, Modifiers::MAC_CMD, 10, true),
            Some(EditCommand::DocStart { select: false })
        );
    }

    #[test]
    fn home_end_and_doc_jumps() {
        assert_eq!(
            key_to_command_for(Key::Home, NONE, 10, false),
            Some(EditCommand::Home { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::End, CTRL | SHIFT, 10, false),
            Some(EditCommand::DocEnd { select: true })
        );
    }

    #[test]
    fn page_keys_carry_page_size() {
        assert_eq!(
            key_to_command_for(Key::PageDown, SHIFT, 7, false),
            Some(EditCommand::PageDown {
                select: true,
                lines: 7
            })
        );
    }

    #[test]
    fn editing_keys() {
        assert_eq!(
            key_to_command_for(Key::Enter, NONE, 1, false),
            Some(EditCommand::Newline)
        );
        assert_eq!(key_to_command_for(Key::Enter, CTRL, 1, false), None);
        assert_eq!(
            key_to_command_for(Key::Tab, NONE, 1, false),
            Some(EditCommand::Indent)
        );
        assert_eq!(
            key_to_command_for(Key::Tab, SHIFT, 1, false),
            Some(EditCommand::Outdent)
        );
        assert_eq!(key_to_command_for(Key::Tab, CTRL, 1, false), None);
        assert_eq!(
            key_to_command_for(Key::Backspace, NONE, 1, false),
            Some(EditCommand::Backspace)
        );
        assert_eq!(key_to_command_for(Key::A, CTRL, 1, false), None);
    }

    #[test]
    fn text_events_become_inserts() {
        let ev = egui::Event::Text("é".into());
        assert_eq!(
            event_to_command(&ev, 1),
            Some(EditCommand::InsertText("é".into()))
        );
        let ime = egui::Event::Ime(egui::ImeEvent::Commit("ö".into()));
        assert_eq!(
            event_to_command(&ime, 1),
            Some(EditCommand::InsertText("ö".into()))
        );
        assert_eq!(
            event_to_command(&egui::Event::Text("\u{8}".into()), 1),
            None
        );
        assert_eq!(
            event_to_command(&egui::Event::Paste("x".into()), 1),
            Some(EditCommand::Paste("x".into()))
        );
    }

    #[test]
    fn held_click_past_clamped_caret_does_not_drag() {
        // Press at 3 (past the line end), vim clamped the caret to 2: no selection.
        assert!(!drag_moves_head(3, 2, false));
        // The pointer moved away from the press position: the drag extends the selection.
        assert!(drag_moves_head(5, 2, true));
        // Back on the press position after a real drag: the head follows.
        assert!(drag_moves_head(3, 5, true));
        assert!(!drag_moves_head(5, 5, true));
    }

    #[test]
    fn events_map_to_vim_keys() {
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        assert_eq!(
            egui_event_to_vim_key(&egui::Event::Text("n".into())),
            Some(VimKey::Char('n'))
        );
        assert_eq!(
            egui_event_to_vim_keys(&egui::Event::Text("ab".into())),
            vec![VimKey::Char('a'), VimKey::Char('b')]
        );
        assert_eq!(egui_event_to_vim_key(&egui::Event::Text("ab".into())), None);
        assert_eq!(
            egui_event_to_vim_key(&key(Key::Escape, NONE)),
            Some(VimKey::Esc)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::Enter, NONE)),
            Some(VimKey::Enter)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::Backspace, NONE)),
            Some(VimKey::Backspace)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::Tab, NONE)),
            Some(VimKey::Tab)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::ArrowUp, NONE)),
            Some(VimKey::Up)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::R, CTRL)),
            Some(VimKey::Ctrl('r'))
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::N, NONE)),
            None,
            "text event follows"
        );
        assert_eq!(egui_event_to_vim_key(&egui::Event::Copy), None);
    }

    #[test]
    fn page_lines_is_visible_rows_minus_one() {
        assert_eq!(page_lines(200.0, 20.0), 9);
        assert_eq!(page_lines(10.0, 20.0), 1);
    }

    #[test]
    fn line_at_y_uses_tops() {
        let tops = [0.0, 10.0, 30.0, 40.0];
        assert_eq!(line_at_y(&tops, -5.0), 0);
        assert_eq!(line_at_y(&tops, 9.9), 0);
        assert_eq!(line_at_y(&tops, 10.0), 1);
        assert_eq!(line_at_y(&tops, 35.0), 2);
        assert_eq!(line_at_y(&tops, 100.0), 2);
    }

    #[test]
    fn display_width_expands_tabs() {
        assert_eq!(display_width("\t\tab\n".chars(), 4), 10);
        // Tabs stop at multiples of the tab width.
        assert_eq!(display_width("ab\tx".chars(), 4), 5);
        assert_eq!(display_width("abcd\t".chars(), 4), 8);
        assert_eq!(display_width("abc".chars(), 4), 3);
        assert_eq!(display_width("".chars(), 4), 0);
    }

    #[test]
    fn decorator_prepare_defaults_to_noop() {
        let mut deco = PlainDecorator;
        deco.prepare(&Buffer::from_text("x"));
        let spans = deco.decorate(0, "abc", &TextFormat::default());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, 0..3);
    }

    #[test]
    fn tab_columns_round_trip() {
        let line = "a\tb";
        assert_eq!(display_col(line, 0, 4), 0);
        assert_eq!(display_col(line, 2, 4), 4);
        assert_eq!(display_col(line, 3, 4), 5);
        assert_eq!(buffer_col(line, 4, 4), 2);
        assert_eq!(buffer_col(line, 2, 4), 1);
        assert_eq!(buffer_col(line, 3, 4), 2);
        assert_eq!(buffer_col(line, 99, 4), 3);
        assert_eq!(buffer_col("abc", 1, 4), 1);
    }

    #[test]
    fn layout_job_fills_gaps_and_expands_tabs() {
        let base = TextFormat::default();
        let red = TextFormat {
            color: egui::Color32::RED,
            ..Default::default()
        };
        let job = build_layout_job("a\tbcd", &[(2..4, red.clone())], &base, 2, None, 10.0);
        assert_eq!(job.text, "a bcd");
        assert_eq!(job.sections.len(), 3);
        assert_eq!(job.sections[1].format, red);
        let r = &job.sections[1].byte_range;
        assert_eq!(&job.text[r.start.0..r.end.0], "bc");
    }

    #[test]
    fn view_columns_match_editing_tab_stops() {
        let rope = Rope::from_str("ab\tx\n\t\ty");
        for pos in [0, 2, 3, 4, 5, 6, 7] {
            let line = rope.char_to_line(pos);
            let col = pos - rope.line_to_char(line);
            let text = line_text(&rope, line);
            assert_eq!(
                display_col(&text, col, 4),
                text_nav::display_col(&rope, pos, 4),
                "pos {pos}"
            );
        }
        let (expanded, offsets) = expand_tabs("ab\tx", 4);
        assert_eq!(expanded, "ab  x");
        assert_eq!(offsets, vec![0, 1, 2, 4, 5]);
    }

    #[test]
    fn layout_job_for_empty_line_has_a_section() {
        let job = build_layout_job("", &[], &TextFormat::default(), 4, None, 10.0);
        assert_eq!(job.sections.len(), 1);
    }

    /// Decorator that leaves every line plain.
    struct Plain;

    impl Decorator for Plain {
        fn decorate(
            &mut self,
            _: usize,
            _: &str,
            _: &TextFormat,
        ) -> Vec<(Range<usize>, TextFormat)> {
            Vec::new()
        }
    }

    /// Indentation settings for the headless editor tests.
    const EDIT: EditSettings = EditSettings {
        tab_width: 4,
        insert_spaces: true,
    };

    /// Runs one frame of the editor for `tab` in a 400x300 window, feeding `events`.
    fn editor_frame(ctx: &egui::Context, tab: &mut Tab, vim: bool, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
            events,
            ..Default::default()
        };
        let opts = ViewOptions {
            font: FontId::monospace(14.0),
            line_numbers: true,
            word_wrap: false,
            edit: EDIT,
            keys_enabled: true,
            vim,
        };
        let mut output = ctx.run_ui(input, |ui| {
            let mut route = |ev: &egui::Event, tab: &mut Tab| {
                for key in egui_event_to_vim_keys(ev) {
                    tab.vim
                        .handle_key(key, &mut tab.buffer, &mut tab.editor, &EDIT);
                }
                true
            };
            let handler: Option<&mut KeyHandler<'_>> = vim.then_some(&mut route as _);
            show_editor(ui, tab, &opts, &mut Plain, handler);
        });
        output.textures_delta.clear();
    }

    /// Opens a 500 line buffer, moves the caret a few lines down, presses PageDown then
    /// PageUp and checks the caret moved by a page while keeping its screen row.
    fn check_page_keys_scroll_the_view(vim: bool) {
        let ctx = egui::Context::default();
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let mut tab = Tab::new(Buffer::from_text(&text));
        tab.editor.selection = crate::editing::Selection::caret(tab.buffer.rope().line_to_char(5));
        for _ in 0..3 {
            editor_frame(&ctx, &mut tab, vim, Vec::new());
        }
        let line = |tab: &Tab| text_nav::line_col(tab.buffer.rope(), tab.editor.selection.head).0;
        let offset_before = tab.view.caret_offset.expect("caret drawn");

        editor_frame(&ctx, &mut tab, vim, vec![key_event(Key::PageDown)]);
        for _ in 0..2 {
            editor_frame(&ctx, &mut tab, vim, Vec::new());
        }
        let paged = line(&tab);
        assert!(paged > 5 + 5, "caret moved by a page, now on line {paged}");
        let offset = tab.view.caret_offset.expect("caret drawn");
        assert!(
            (offset - offset_before).abs() < 1.0,
            "{offset} vs {offset_before}"
        );

        editor_frame(&ctx, &mut tab, vim, vec![key_event(Key::PageUp)]);
        for _ in 0..2 {
            editor_frame(&ctx, &mut tab, vim, Vec::new());
        }
        assert_eq!(line(&tab), 5);
        let offset = tab.view.caret_offset.expect("caret drawn");
        assert!(
            (offset - offset_before).abs() < 1.0,
            "{offset} vs {offset_before}"
        );
    }

    #[test]
    fn page_keys_scroll_the_view() {
        check_page_keys_scroll_the_view(false);
    }

    #[test]
    fn page_keys_scroll_the_view_in_vim_mode() {
        check_page_keys_scroll_the_view(true);
    }

    #[test]
    fn page_key_ignores_modified_presses() {
        assert_eq!(page_key(&key_event(Key::PageUp)), Some(true));
        assert_eq!(page_key(&key_event(Key::PageDown)), Some(false));
        let ctrl = egui::Event::Key {
            key: Key::PageDown,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: CTRL,
        };
        assert_eq!(page_key(&ctrl), None);
        assert_eq!(page_key(&key_event(Key::ArrowDown)), None);
    }
}
