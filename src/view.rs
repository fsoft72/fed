//! Custom editor widget: renders a `Tab` and translates egui input into `EditCommand`s.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use eframe::egui::{
    self, Align2, CursorIcon, EventFilter, FontId, Galley, Key, Modifiers, Pos2, Rect, Sense,
    TextFormat, Ui, epaint::text::cursor::CCursor, pos2, text::LayoutJob, vec2,
};
use ropey::Rope;

use crate::buffer::Buffer;
use crate::editing::{EditCommand, EditSettings};
use crate::tab::Tab;
use crate::text_nav::{self, CharClass};

/// Horizontal padding on each side of the line numbers.
const GUTTER_PAD: f32 = 8.0;
/// Gap between the gutter and the text, and after the text.
const TEXT_PAD: f32 = 4.0;
/// Minimum number of digits reserved for line numbers.
const MIN_GUTTER_DIGITS: usize = 3;
/// Caret width in points.
const CARET_WIDTH: f32 = 2.0;
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
}

/// Per-tab state kept by the widget between frames.
#[derive(Default)]
pub struct ViewState {
    /// The widget held keyboard focus since the last `request_focus` (or since creation).
    focus_acquired: bool,
    /// Longest line display width in columns, with the revision and tab width it was
    /// computed for.
    max_line_cols: Option<(u64, usize, usize)>,
    /// Wrapped line heights (word wrap only).
    wrap: WrapCache,
}

impl ViewState {
    /// Makes the widget take keyboard focus the next time it is shown.
    pub(crate) fn request_focus(&mut self) {
        self.focus_acquired = false;
    }
}

/// Cached line geometry for word wrap mode.
#[derive(Default)]
struct WrapCache {
    /// Font, wrap width and tab width the heights were measured with.
    params: Option<(FontId, f32, usize)>,
    /// Wrapped height keyed by a hash of the line text.
    heights: HashMap<u64, f32>,
    /// Buffer revision `tops` was computed for.
    revision: Option<u64>,
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
        if let Some(handler) = key_handler.as_mut()
            && handler(ev, tab)
        {
            tab.end_typing();
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

/// Display width in columns of `chars` (a line, newline excluded from the count); tabs take
/// `tab_width` columns.
fn display_width(chars: impl Iterator<Item = char>, tab_width: usize) -> usize {
    chars
        .filter(|&c| c != '\n')
        .map(|c| if c == '\t' { tab_width } else { 1 })
        .sum()
}

/// Converts a char column of `line` to a display column (tabs take `tab_width` columns).
fn display_col(line: &str, col: usize, tab_width: usize) -> usize {
    line.chars()
        .take(col)
        .map(|c| if c == '\t' { tab_width } else { 1 })
        .sum()
}

/// Converts a display column to the nearest char column of `line`.
fn buffer_col(line: &str, dcol: usize, tab_width: usize) -> usize {
    let mut x = 0;
    for (i, c) in line.chars().enumerate() {
        let w = if c == '\t' { tab_width } else { 1 };
        if dcol < x + w {
            return if dcol - x <= w / 2 { i } else { i + 1 };
        }
        x += w;
    }
    line.chars().count()
}

/// Returns `line` with tabs replaced by `tab_width` spaces.
fn expand_tabs(text: &str, tab_width: usize) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    let spaces = " ".repeat(tab_width);
    text.replace('\t', &spaces)
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
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut append = |range: Range<usize>, format: &TextFormat| {
        let text: String = chars[range].iter().collect();
        job.append(&expand_tabs(&text, tab_width), 0.0, format.clone());
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

/// Hash of a line's text, used as the wrap height cache key.
fn text_hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

impl WrapCache {
    /// Recomputes line tops when the buffer revision or the layout parameters changed.
    fn update(&mut self, ui: &Ui, rope: &Rope, revision: u64, m: &Metrics, width: f32) {
        let params = (m.base.font_id.clone(), width, m.tab_width);
        if self.params.as_ref() != Some(&params) {
            self.heights.clear();
            self.params = Some(params);
            self.revision = None;
        }
        if self.revision == Some(revision) {
            return;
        }
        self.tops.clear();
        let mut y = 0.0;
        let heights = &mut self.heights;
        let tops = &mut self.tops;
        ui.ctx().fonts_mut(|fonts| {
            for line in rope.lines() {
                tops.push(y);
                let mut text = line.to_string();
                if text.ends_with('\n') {
                    text.pop();
                }
                let height = if text.is_empty() {
                    m.row_height
                } else {
                    *heights.entry(text_hash(&text)).or_insert_with(|| {
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
                    })
                };
                y += height;
            }
        });
        self.tops.push(y);
        self.revision = Some(revision);
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
        let revision = self.tab.buffer.revision();
        let rows = match wrap_width {
            Some(width) => {
                let rope = self.tab.buffer.rope().clone();
                self.tab
                    .view
                    .wrap
                    .update(ui, &rope, revision, self.m, width);
                RowMap::Wrapped(self.tab.view.wrap.tops.clone())
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
        if self.tab.scroll_to_cursor {
            self.tab.scroll_to_cursor = false;
            let margin = SCROLL_MARGIN_CHARS * self.m.char_width;
            let target = Rect::from_min_max(
                pos2(caret.min.x - text_x, caret.min.y),
                pos2(caret.max.x + margin, caret.max.y),
            );
            ui.scroll_to_rect(target, None);
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
    }

    /// Display width in columns (tabs expanded) of the longest line, cached per revision.
    fn max_line_cols(&mut self) -> usize {
        let revision = self.tab.buffer.revision();
        let tab_width = self.m.tab_width;
        if let Some((rev, tw, cols)) = self.tab.view.max_line_cols
            && rev == revision
            && tw == tab_width
        {
            return cols;
        }
        let cols = self
            .tab
            .buffer
            .rope()
            .lines()
            .map(|l| display_width(l.chars(), tab_width))
            .max()
            .unwrap_or(0);
        self.tab.view.max_line_cols = Some((revision, tab_width, cols));
        cols
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
        if response.hovered() && pressed {
            response.request_focus();
            // The pointer may already have moved within this frame: anchor at the press.
            let pos = self.hit_test(ui, press_origin.unwrap_or(pointer), pl);
            self.tab
                .apply(EditCommand::SetCaret { pos, select: shift }, &edit);
        }
        if down && response.is_pointer_button_down_on() {
            let pos = self.hit_test(ui, pointer, pl);
            if pos != self.tab.editor.selection.head {
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
        let sel = self.tab.editor.selection.range();
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
        let rect = Rect::from_min_size(caret.min, vec2(CARET_WIDTH, caret.height()));
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().text_cursor.stroke.color);
    }
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
        assert_eq!(display_col(line, 2, 4), 5);
        assert_eq!(display_col(line, 3, 4), 6);
        assert_eq!(buffer_col(line, 5, 4), 2);
        assert_eq!(buffer_col(line, 2, 4), 1);
        assert_eq!(buffer_col(line, 4, 4), 2);
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
        assert_eq!(job.text, "a  bcd");
        assert_eq!(job.sections.len(), 3);
        assert_eq!(job.sections[1].format, red);
        let r = &job.sections[1].byte_range;
        assert_eq!(&job.text[r.start.0..r.end.0], "bc");
    }

    #[test]
    fn layout_job_for_empty_line_has_a_section() {
        let job = build_layout_job("", &[], &TextFormat::default(), 4, None, 10.0);
        assert_eq!(job.sections.len(), 1);
    }
}
