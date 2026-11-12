//! Markdown prose ranges computed off the UI thread.
//!
//! Parsing a whole Markdown document with pulldown-cmark on every keystroke is too slow for
//! the UI thread, and a Markdown parse cannot be safely restarted in the middle of a document
//! (fences, lists and front matter change the meaning of later lines). `ProseRanges` hands a
//! snapshot of the rope to a worker thread instead, at most one at a time per tab, and while a
//! parse is running it shows the last known ranges shifted by the edits made since.

use std::ops::Range;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui;

use crate::buffer::Buffer;

use super::words;

/// A parse running on a worker thread.
struct Pending {
    /// Buffer revision the worker is parsing.
    revision: u64,
    /// Char length of the buffer at `revision`.
    len_chars: usize,
    /// Receives the prose ranges when the parse is done.
    rx: Receiver<Vec<Range<usize>>>,
}

/// Last completed parse.
struct Parsed {
    /// Buffer revision the ranges belong to.
    revision: u64,
    /// Char length of the buffer at `revision`.
    len_chars: usize,
    /// Prose char ranges of the whole document at `revision`.
    ranges: Vec<Range<usize>>,
}

/// Prose char ranges of a Markdown buffer, kept up to date by a background parse.
#[derive(Default)]
pub struct ProseRanges {
    /// Last completed parse, if any.
    parsed: Option<Parsed>,
    /// Parse in flight, if any.
    pending: Option<Pending>,
    /// Ranges shown for `shown_revision` (the parsed ones, shifted when stale).
    shown: Vec<Range<usize>>,
    /// Buffer revision `shown` was computed for.
    shown_revision: Option<u64>,
}

impl ProseRanges {
    /// Creates an empty cache; the first `refresh` starts a parse.
    pub fn new() -> ProseRanges {
        ProseRanges::default()
    }

    /// Collects a finished parse, starts a new one when the buffer moved on, and updates the
    /// ranges returned by `ranges` for the current buffer revision. `ctx` is repainted when a
    /// background parse completes.
    pub fn refresh(&mut self, buffer: &Buffer, ctx: &egui::Context) {
        self.poll();
        let revision = buffer.revision();
        let parsed_current = self.parsed.as_ref().is_some_and(|p| p.revision == revision);
        if !parsed_current && self.pending.is_none() {
            self.spawn(buffer, ctx);
        }
        if self.shown_revision == Some(revision) {
            return;
        }
        self.shown = match &self.parsed {
            Some(p) if p.revision == revision => p.ranges.clone(),
            Some(p) => shift_stale(p, buffer),
            None => Vec::new(),
        };
        self.shown_revision = Some(revision);
    }

    /// Returns the prose ranges for the buffer revision of the last `refresh`.
    pub fn ranges(&self) -> &[Range<usize>] {
        &self.shown
    }

    /// Returns true while a background parse is running.
    pub fn is_parsing(&self) -> bool {
        self.pending.is_some()
    }

    /// Takes the result of the running parse, if it is done.
    fn poll(&mut self) {
        let Some(pending) = &self.pending else { return };
        match pending.rx.try_recv() {
            Ok(ranges) => {
                self.parsed = Some(Parsed {
                    revision: pending.revision,
                    len_chars: pending.len_chars,
                    ranges,
                });
                // Force `shown` to be rebuilt from the new parse.
                self.shown_revision = None;
                self.pending = None;
            }
            // The worker died (a parser panic): try again on the next refresh.
            Err(TryRecvError::Disconnected) => self.pending = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Starts parsing a snapshot of `buffer` on a worker thread.
    fn spawn(&mut self, buffer: &Buffer, ctx: &egui::Context) {
        let rope = buffer.rope().clone();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let ranges = words::markdown_checkable(&rope.to_string());
            // The tab may be gone: nobody listening is fine.
            let _ = tx.send(ranges);
            ctx.request_repaint();
        });
        self.pending = Some(Pending {
            revision: buffer.revision(),
            len_chars: buffer.rope().len_chars(),
            rx,
        });
    }
}

/// Maps stale parsed ranges onto the current buffer: text before the first line edited since
/// the parse is unchanged, so ranges there are kept; later ranges move by the change in
/// length. This is exact for a single edit site and close enough until the new parse lands.
fn shift_stale(parsed: &Parsed, buffer: &Buffer) -> Vec<Range<usize>> {
    let rope = buffer.rope();
    let line = buffer
        .first_dirty_line_since(parsed.revision)
        .unwrap_or(0)
        .min(rope.len_lines().saturating_sub(1));
    let pivot = rope.line_to_char(line);
    let delta = rope.len_chars() as isize - parsed.len_chars as isize;
    shift_ranges(&parsed.ranges, pivot, delta, rope.len_chars())
}

/// Shifts the parts of `ranges` at or after char `pivot` by `delta` chars, clamping to
/// `pivot..len`; ranges that become empty are dropped.
pub fn shift_ranges(
    ranges: &[Range<usize>],
    pivot: usize,
    delta: isize,
    len: usize,
) -> Vec<Range<usize>> {
    let shift = |x: usize| match x < pivot {
        true => x,
        false => (x as isize + delta).clamp(pivot as isize, len as isize) as usize,
    };
    ranges
        .iter()
        .map(|r| shift(r.start)..shift(r.end))
        .filter(|r| !r.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Refreshes `prose` until no parse is running (or a timeout expires).
    fn settle(prose: &mut ProseRanges, buffer: &Buffer, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(5);
        prose.refresh(buffer, ctx);
        while prose.is_parsing() {
            assert!(Instant::now() < deadline, "parse did not finish");
            std::thread::sleep(Duration::from_millis(1));
            prose.refresh(buffer, ctx);
        }
    }

    #[test]
    fn background_parse_matches_direct_parse() {
        let ctx = egui::Context::default();
        let text = "Hello `code` world\n\n```\nfenced\n```\n\nbye\n";
        let buffer = Buffer::from_text(text);
        let mut prose = ProseRanges::new();
        settle(&mut prose, &buffer, &ctx);
        assert_eq!(prose.ranges(), words::markdown_checkable(text).as_slice());
    }

    #[test]
    fn stale_ranges_are_shifted_until_the_new_parse_lands() {
        let ctx = egui::Context::default();
        let mut buffer = Buffer::from_text("alpha\n\nbeta\n");
        let mut prose = ProseRanges::new();
        settle(&mut prose, &buffer, &ctx);
        let before = prose.ranges().to_vec();
        buffer.replace(0..0, "xx");
        // Simulate the frame right after the edit, with the old parse still current.
        prose.shown_revision = None;
        let parsed = prose.parsed.as_ref().unwrap();
        let shifted = shift_stale(parsed, &buffer);
        assert_eq!(shifted.len(), before.len());
        assert_eq!(shifted[1], before[1].start + 2..before[1].end + 2);
        settle(&mut prose, &buffer, &ctx);
        assert_eq!(
            prose.ranges(),
            words::markdown_checkable(&buffer.text()).as_slice()
        );
    }

    #[test]
    fn shift_ranges_keeps_before_pivot_and_clamps() {
        let ranges = [0..3, 5..9, 12..15];
        assert_eq!(shift_ranges(&ranges, 6, 2, 20), vec![0..3, 5..11, 14..17]);
        // A deletion swallowing a whole range drops it.
        assert_eq!(shift_ranges(&ranges, 10, -5, 10), vec![0..3, 5..9]);
    }
}
