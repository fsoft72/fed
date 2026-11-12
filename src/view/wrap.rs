//! Line geometry of the editor: vertical placement of lines (`RowMap`), wrapped line heights
//! measured lazily within a frame budget (`WrapCache`) and the longest line of a buffer
//! (`LongestLine`), all kept up to date incrementally from `Buffer::dirty_lines_since`.

use std::ops::Range;
use std::time::{Duration, Instant};

use eframe::egui::{self, FontId, Rect, Ui};
use ropey::Rope;

use super::{Metrics, build_layout_job};
use crate::buffer::{Buffer, DirtyLines};
use crate::text_nav;

/// Edits touching at most this many lines are measured at once in word wrap mode; bigger
/// ones (and a new wrap width) start from estimates refined over the next frames.
const EAGER_WRAP_LINES: usize = 256;

/// Time a frame may spend measuring off-screen wrapped lines.
const WRAP_FRAME_BUDGET: Duration = Duration::from_millis(4);

/// Most passes made to measure the visible lines exactly (each may move the viewport lines).
const VIEWPORT_SETTLE_PASSES: usize = 3;

/// The longest line of a buffer revision (no word wrap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LongestLine {
    /// Buffer revision it was computed for.
    revision: u64,
    /// Tab width used to measure.
    tab_width: usize,
    /// Display width in columns (tabs expanded).
    pub(super) cols: usize,
    /// Index of the longest line.
    line: usize,
}

/// Cached line geometry for word wrap mode. Lines not measured yet count as one row
/// (`estimate`) until `settle` and `fill` measure them.
#[derive(Default)]
pub(super) struct WrapCache {
    /// Font, wrap width and tab width the heights were measured with.
    params: Option<(FontId, f32, usize)>,
    /// Buffer revision `heights` and `tops` were computed for.
    revision: Option<u64>,
    /// Wrapped height of every line (one entry per buffer line), estimated when unmeasured.
    heights: Vec<f32>,
    /// Whether each entry of `heights` was measured.
    measured: Vec<bool>,
    /// Number of `false` entries of `measured`.
    unmeasured: usize,
    /// Line where `fill` resumes looking for unmeasured lines.
    fill_from: usize,
    /// Top of every line, plus the total height as the last element.
    pub(super) tops: Vec<f32>,
}

/// Vertical placement of lines in content coordinates.
pub(super) enum RowMap {
    /// Every line is one row high.
    Fixed { row_height: f32, lines: usize },
    /// Variable heights: tops of lines plus the total height last.
    Wrapped(Vec<f32>),
}

impl RowMap {
    /// Top of `line`.
    pub(super) fn top(&self, line: usize) -> f32 {
        match self {
            RowMap::Fixed { row_height, .. } => line as f32 * row_height,
            RowMap::Wrapped(tops) => tops[line.min(tops.len() - 1)],
        }
    }

    /// Total content height.
    pub(super) fn total(&self) -> f32 {
        match self {
            RowMap::Fixed { row_height, lines } => *lines as f32 * row_height,
            RowMap::Wrapped(tops) => *tops.last().unwrap_or(&0.0),
        }
    }

    /// Line containing content y coordinate `y`, clamped to the document.
    pub(super) fn line_at(&self, y: f32) -> usize {
        match self {
            RowMap::Fixed { row_height, lines } => {
                let line = (y / row_height).floor().max(0.0) as usize;
                line.min(lines.saturating_sub(1))
            }
            RowMap::Wrapped(tops) => line_at_y(tops, y),
        }
    }
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

/// Measures the wrapped height of `line` laid out `width` points wide.
fn measure_line(
    fonts: &mut egui::epaint::FontsView<'_>,
    rope: &Rope,
    line: usize,
    m: &Metrics,
    width: f32,
) -> f32 {
    let text = text_nav::line_text(rope, line);
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
    /// Brings line heights and tops up to date with the buffer and the viewport: lines edited
    /// since the last update are re-measured (or estimated, for big edits and a changed font,
    /// wrap width or tab width), the lines in `viewport` (content coordinates) are measured
    /// exactly, and other unmeasured lines are measured for at most `WRAP_FRAME_BUDGET`.
    /// Returns the vertical shift of the first visible line caused by lines measured above it
    /// (scroll by it to keep the view still) and whether unmeasured lines remain.
    pub(super) fn update(
        &mut self,
        ui: &Ui,
        buffer: &Buffer,
        m: &Metrics,
        width: f32,
        viewport: Rect,
    ) -> (f32, bool) {
        let params = (m.base.font_id.clone(), width, m.tab_width);
        if self.params.as_ref() != Some(&params) {
            self.params = Some(params);
            self.revision = None;
        }
        let rope = buffer.rope();
        let deadline = Instant::now() + WRAP_FRAME_BUDGET;
        ui.ctx().fonts_mut(|fonts| {
            let mut measure = |line| measure_line(fonts, rope, line, m, width);
            self.refresh(buffer, m.row_height, &mut measure);
            self.settle(viewport, &mut measure);
            let first = line_at_y(&self.tops, viewport.min.y);
            let before = self.tops[first];
            let pending = self.fill(deadline, &mut measure);
            (self.tops[first] - before, pending)
        })
    }

    /// Re-measures the lines edited since `revision` with `measure` (every line when there
    /// is no previous revision; more than `EAGER_WRAP_LINES` lines get `estimate` instead,
    /// to be measured later) and rebuilds `tops` from the first edited line.
    fn refresh(&mut self, buffer: &Buffer, estimate: f32, mut measure: impl FnMut(usize) -> f32) {
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
        let eager = dirty.end - dirty.start <= EAGER_WRAP_LINES;
        let fresh: Vec<f32> = match eager {
            true => (dirty.start..dirty.end).map(&mut measure).collect(),
            false => vec![estimate; dirty.end - dirty.start],
        };
        let dropped = self.measured[dirty.start..dirty.old_end]
            .iter()
            .filter(|m| !**m)
            .count();
        self.unmeasured -= dropped;
        if !eager {
            self.unmeasured += fresh.len();
        }
        let flags = std::iter::repeat_n(eager, fresh.len());
        self.measured.splice(dirty.start..dirty.old_end, flags);
        self.heights.splice(dirty.start..dirty.old_end, fresh);
        self.fill_from = self.fill_from.min(dirty.start);
        self.retop(dirty.start);
        self.revision = Some(buffer.revision());
    }

    /// Measures the unmeasured lines of `lines` and rebuilds `tops` when any height changed.
    /// Returns true when something changed.
    fn measure_lines(
        &mut self,
        lines: Range<usize>,
        measure: &mut impl FnMut(usize) -> f32,
    ) -> bool {
        let mut first_changed = None;
        for line in lines {
            if self.measured[line] {
                continue;
            }
            let height = measure(line);
            self.measured[line] = true;
            self.unmeasured -= 1;
            if height != self.heights[line] {
                self.heights[line] = height;
                first_changed.get_or_insert(line);
            }
        }
        let Some(first) = first_changed else {
            return false;
        };
        self.retop(first);
        true
    }

    /// Measures the lines shown in `viewport` (content coordinates) exactly. Measuring may
    /// grow lines and push others out of the viewport, so this repeats a few times.
    fn settle(&mut self, viewport: Rect, measure: &mut impl FnMut(usize) -> f32) {
        if self.heights.is_empty() {
            return;
        }
        for _ in 0..VIEWPORT_SETTLE_PASSES {
            let first = line_at_y(&self.tops, viewport.min.y);
            let last = line_at_y(&self.tops, viewport.max.y);
            if !self.measure_lines(first..last + 1, measure) {
                return;
            }
        }
    }

    /// Measures unmeasured lines, from `fill_from` on and then from the top, until
    /// `deadline`. Returns true when some are still unmeasured.
    fn fill(&mut self, deadline: Instant, measure: &mut impl FnMut(usize) -> f32) -> bool {
        let lines = self.heights.len();
        let mut first_changed = None;
        let mut line = self.fill_from.min(lines);
        let mut scanned = 0;
        while self.unmeasured > 0 && scanned < lines && Instant::now() < deadline {
            if line >= lines {
                line = 0;
            }
            if !self.measured[line] {
                let height = measure(line);
                self.measured[line] = true;
                self.unmeasured -= 1;
                if height != self.heights[line] {
                    self.heights[line] = height;
                    first_changed = Some(first_changed.map_or(line, |f: usize| f.min(line)));
                }
            }
            line += 1;
            scanned += 1;
        }
        self.fill_from = line;
        if let Some(first) = first_changed {
            self.retop(first);
        }
        self.unmeasured > 0
    }

    /// Rebuilds `tops` from line `from` on.
    fn retop(&mut self, from: usize) {
        self.tops.truncate(from);
        let mut y = match from {
            0 => 0.0,
            n => self.tops[n - 1] + self.heights[n - 1],
        };
        for h in &self.heights[from..] {
            self.tops.push(y);
            y += h;
        }
        self.tops.push(y);
    }
}

/// Returns the longest of `lines` (display columns, line index), the first one on ties;
/// `(0, lines.start)` for an empty range.
fn longest_in(rope: &Rope, lines: Range<usize>, tab_width: usize) -> (usize, usize) {
    let first = lines.start;
    lines
        .map(|i| (text_nav::text_width(rope.line(i).chars(), tab_width), i))
        .fold((0, first), |best, cur| match cur.0 > best.0 {
            true => cur,
            false => best,
        })
}

/// Returns the longest line of `buffer`, reusing `old` when possible: only the lines edited
/// since `old` are measured, and the whole buffer is rescanned only when the old longest line
/// was edited and no edited line is at least as long.
pub(super) fn longest_line(
    buffer: &Buffer,
    old: Option<LongestLine>,
    tab_width: usize,
) -> LongestLine {
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

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    /// Builds a fully measured wrap cache for `buffer` where a line is as tall as its char
    /// count (min 1).
    fn wrap_for(buffer: &Buffer) -> WrapCache {
        let mut wrap = WrapCache::default();
        wrap.refresh(buffer, 1.0, |i| height_of(buffer, i));
        let far = Instant::now() + Duration::from_secs(60);
        assert!(!wrap.fill(far, &mut |i| height_of(buffer, i)));
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
        wrap.refresh(&buffer, 1.0, |i| {
            measured.push(i);
            height_of(&buffer, i)
        });
        assert_eq!(measured, vec![1, 2, 3]);
        let fresh = wrap_for(&buffer);
        assert_eq!(wrap.heights, fresh.heights);
        assert_eq!(wrap.tops, fresh.tops);
        // Deleting lines shrinks the cache too.
        buffer.replace(0..8, "");
        wrap.refresh(&buffer, 1.0, |i| height_of(&buffer, i));
        let fresh = wrap_for(&buffer);
        assert_eq!(wrap.heights, fresh.heights);
        assert_eq!(wrap.tops, fresh.tops);
        // Unchanged revision: nothing is measured.
        let mut calls = 0;
        wrap.refresh(&buffer, 1.0, |_| {
            calls += 1;
            1.0
        });
        assert_eq!(calls, 0);
    }

    #[test]
    fn big_wrap_updates_measure_the_viewport_first_then_fill() {
        let text: String = (0..1000)
            .map(|i| format!("{}\n", "x".repeat(i % 7)))
            .collect();
        let buffer = Buffer::from_text(&text);
        let lines = buffer.rope().len_lines();
        let mut wrap = WrapCache::default();
        let mut calls = 0;
        wrap.refresh(&buffer, 1.0, |_| {
            calls += 1;
            1.0
        });
        assert_eq!(calls, 0, "a whole-buffer refresh only estimates");
        assert_eq!(wrap.unmeasured, lines);
        // Rows 10..20 of the content are visible.
        let viewport = Rect::from_min_max(pos2(0.0, 10.0), pos2(100.0, 20.0));
        let mut measured = Vec::new();
        wrap.settle(viewport, &mut |i| {
            measured.push(i);
            height_of(&buffer, i)
        });
        assert!(
            measured.contains(&10) && measured.len() < 40,
            "{measured:?}"
        );
        let far = Instant::now() + Duration::from_secs(60);
        assert!(!wrap.fill(far, &mut |i| height_of(&buffer, i)));
        assert_eq!(wrap.unmeasured, 0);
        let exact = wrap_for(&buffer);
        assert_eq!(wrap.heights, exact.heights);
        assert_eq!(wrap.tops, exact.tops);
        // An expired budget measures nothing.
        let mut wrap = WrapCache::default();
        wrap.refresh(&buffer, 1.0, |_| 1.0);
        assert!(wrap.fill(Instant::now(), &mut |_| 2.0));
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
    fn line_at_y_uses_tops() {
        let tops = [0.0, 10.0, 30.0, 40.0];
        assert_eq!(line_at_y(&tops, -5.0), 0);
        assert_eq!(line_at_y(&tops, 9.9), 0);
        assert_eq!(line_at_y(&tops, 10.0), 1);
        assert_eq!(line_at_y(&tops, 35.0), 2);
        assert_eq!(line_at_y(&tops, 100.0), 2);
    }
}
