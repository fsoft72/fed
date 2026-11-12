//! Rope-backed text buffer with undo/redo, marks, line endings and atomic saving.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ropey::{Rope, RopeBuilder};

use crate::job::BackgroundJob;

/// Files at or above this many bytes are flagged as large.
pub const LARGE_FILE_BYTES: usize = 5_000_000;
/// Files bigger than this many bytes are refused instead of opened.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Files at or above this many bytes are read on a background thread.
pub const BACKGROUND_LOAD_BYTES: u64 = 1024 * 1024;
/// Lines longer than this many chars flag the buffer as large.
pub const LONG_LINE_CHARS: usize = 10_000;
/// Most undo steps kept; older ones are dropped.
const MAX_UNDO_STEPS: usize = 1000;
/// Most text (removed plus inserted bytes) the undo history keeps; older steps are dropped.
const MAX_UNDO_BYTES: usize = 64 * 1024 * 1024;
/// Number of recent revisions (single edits or batches) remembered by `dirty_lines_since`.
const EDIT_LOG_LEN: usize = 1024;
/// Suffix of the temporary file used for atomic saves.
const TMP_SUFFIX: &str = ".fed-tmp";

/// Line ending style of a file on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEnding {
    /// Unix style `\n`.
    #[default]
    Lf,
    /// Windows style `\r\n`.
    Crlf,
}

impl LineEnding {
    /// Returns the literal line terminator string.
    pub fn as_str(&self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }
}

/// Errors produced by buffer file operations.
#[derive(thiserror::Error, Debug)]
pub enum BufferError {
    /// The file content is not valid UTF-8.
    #[error("{0}: file is not valid UTF-8")]
    NotUtf8(PathBuf),
    /// An I/O error occurred on `path`.
    #[error("{path}: {source}")]
    Io {
        /// File that caused the error.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The file is bigger than `MAX_FILE_BYTES`.
    #[error("{path}: file is too large ({size} bytes, the limit is {limit} bytes)")]
    TooLarge {
        /// The refused file.
        path: PathBuf,
        /// Its size in bytes.
        size: u64,
        /// The limit, `MAX_FILE_BYTES`.
        limit: u64,
    },
    /// The background loader thread crashed or stopped without a result.
    #[error("{0}: loading was interrupted ({1})")]
    LoadInterrupted(PathBuf, crate::job::JobError),
    /// A save was requested but the buffer is not bound to a path.
    #[error("buffer has no file path")]
    NoPath,
    /// The file was changed on disk by another program since it was read or saved.
    #[error("{0}: file changed on disk since it was read")]
    ChangedOnDisk(PathBuf),
}

/// What the file of a buffer looked like on disk when it was last read or written, to notice
/// changes made by other programs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiskStamp {
    /// Last modification time, when the file system reports one.
    modified: Option<SystemTime>,
    /// File size in bytes.
    len: u64,
}

impl DiskStamp {
    /// Stamp of the file at `path`, `None` when it does not exist (or cannot be read).
    fn of(path: &Path) -> Option<DiskStamp> {
        let meta = fs::metadata(path).ok()?;
        Some(DiskStamp {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

/// One primitive text replacement, stored so it can be reverted.
#[derive(Clone, Debug)]
struct Edit {
    /// Char index where the replacement happened.
    pos: usize,
    /// Text that was removed.
    removed: String,
    /// Text that was inserted.
    inserted: String,
}

/// Lines touched by one revision (one edit or one batch), remembered for
/// `dirty_lines_since`.
#[derive(Clone, Copy, Debug)]
struct LoggedEdit {
    /// Buffer revision right after the edit.
    revision: u64,
    /// The lines it changed.
    span: LineSpan,
}

/// Lines changed since a revision: `start..end` in the current buffer replaced
/// `start..old_end` of the buffer at that revision. Lines before `start` are unchanged and
/// line `l >= end` is old line `l - end + old_end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyLines {
    /// First changed line (same index before and after).
    pub start: usize,
    /// End (exclusive) of the changed lines in the current buffer.
    pub end: usize,
    /// End (exclusive) of the replaced lines in the old buffer. `usize::MAX` when the edits
    /// are no longer known: everything from `start` on is replaced, whatever the old length.
    pub old_end: usize,
}

/// A rope-backed text buffer.
pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    /// Canonical form of `path`, computed when it is bound or saved; `None` while the file
    /// does not exist.
    canonical: Option<PathBuf>,
    /// The bound file as last read or written by this buffer; `None` when it did not exist.
    disk: Option<DiskStamp>,
    line_ending: LineEnding,
    large: bool,
    revision: u64,
    /// Undo steps, oldest first, bounded by `MAX_UNDO_STEPS` and `MAX_UNDO_BYTES`.
    history: VecDeque<Vec<Edit>>,
    /// Text size of `history` (see `step_bytes`).
    history_bytes: usize,
    /// Number of history steps currently applied.
    position: usize,
    /// History position at the last save, if still reachable.
    saved_position: Option<usize>,
    group_depth: usize,
    group_edits: Vec<Edit>,
    marks: Vec<(char, usize)>,
    /// Recent edits, oldest first.
    edit_log: VecDeque<LoggedEdit>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// Creates an empty, unnamed buffer.
    pub fn new() -> Buffer {
        Self::from_text("")
    }

    /// Creates a buffer from `text`. The line ending used on save is the majority one
    /// (CRLF when CRLF lines outnumber bare LF lines, LF otherwise, ties go to LF); the
    /// rope always holds LF only (CRLF and lone CR become LF).
    pub fn from_text(text: &str) -> Buffer {
        let crlf = detect_line_ending(text) == LineEnding::Crlf;
        let rope = build_rope(text);
        let large = is_large_text(&rope, 0..rope.len_lines());
        Buffer {
            rope,
            path: None,
            canonical: None,
            disk: None,
            line_ending: if crlf {
                LineEnding::Crlf
            } else {
                LineEnding::Lf
            },
            large,
            revision: 0,
            history: VecDeque::new(),
            history_bytes: 0,
            position: 0,
            saved_position: Some(0),
            group_depth: 0,
            group_edits: Vec::new(),
            marks: Vec::new(),
            edit_log: VecDeque::new(),
        }
    }

    /// Loads `path`. A missing file yields an empty buffer bound to `path`; a file bigger than
    /// `MAX_FILE_BYTES` is refused before it is read. The text is held at most twice while
    /// loading (the file contents and the rope).
    pub fn load(path: &Path) -> Result<Buffer, BufferError> {
        let (text, disk) = read_file(path)?;
        let mut buf = Self::from_text(text.as_deref().unwrap_or(""));
        buf.bind(path.to_path_buf());
        buf.disk = disk;
        Ok(buf)
    }

    /// Replaces the content with the file on disk, as one undo step (so the reload can be
    /// undone), and marks the buffer as saved. The save line ending follows the file again.
    /// Callers must close any open undo group first.
    pub fn reload(&mut self) -> Result<(), BufferError> {
        debug_assert_eq!(self.group_depth, 0, "reload with an open undo group");
        let path = self.path.clone().ok_or(BufferError::NoPath)?;
        let (text, disk) = read_file(&path)?;
        let text = text.unwrap_or_default();
        self.line_ending = detect_line_ending(&text);
        let len = self.rope.len_chars();
        self.replace(0..len, &text);
        self.saved_position = Some(self.position);
        self.disk = disk;
        Ok(())
    }

    /// True when the bound file was created, changed or deleted by another program since this
    /// buffer last read or wrote it (modification time or size differ).
    pub fn changed_on_disk(&self) -> bool {
        self.path
            .as_deref()
            .is_some_and(|path| DiskStamp::of(path) != self.disk)
    }

    /// Returns the bound file path, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Binds the buffer to `path` without saving.
    pub fn set_path(&mut self, path: PathBuf) {
        self.bind(path);
    }

    /// Binds the buffer to `path` and remembers its canonical form.
    fn bind(&mut self, path: PathBuf) {
        self.canonical = fs::canonicalize(&path).ok();
        self.path = Some(path);
    }

    /// True when the buffer is bound to the file `path`, whose canonical form is `canonical`
    /// (see `same_file`). Compares the canonical path kept by the buffer, so checking every
    /// open tab costs no file system call.
    pub fn is_bound_to(&self, path: &Path, canonical: Option<&Path>) -> bool {
        self.path
            .as_deref()
            .is_some_and(|own| same_file(own, self.canonical.as_deref(), path, canonical))
    }

    /// Saves to the bound path. Refuses with `ChangedOnDisk` when another program changed the
    /// file since it was read or saved (see `save_overwrite`). Callers must close any open
    /// undo group first.
    pub fn save(&mut self) -> Result<(), BufferError> {
        let path = self.path.clone().ok_or(BufferError::NoPath)?;
        if self.changed_on_disk() {
            return Err(BufferError::ChangedOnDisk(path));
        }
        self.save_overwrite()
    }

    /// Saves to the bound path even when the file changed on disk. Callers must close any
    /// open undo group first.
    pub fn save_overwrite(&mut self) -> Result<(), BufferError> {
        debug_assert_eq!(self.group_depth, 0, "save with an open undo group");
        let path = self.path.clone().ok_or(BufferError::NoPath)?;
        self.write_copy(&path)?;
        self.saved_position = Some(self.position);
        // The file exists now, even if it did not when the buffer was bound.
        self.canonical = fs::canonicalize(&path).ok();
        self.disk = DiskStamp::of(&path);
        Ok(())
    }

    /// Saves to `path` and binds the buffer to it. On failure the buffer keeps its previous
    /// path, so a failed "Save As" does not silently retarget later saves.
    pub fn save_as(&mut self, path: PathBuf) -> Result<(), BufferError> {
        self.write_copy(&path)?;
        self.disk = DiskStamp::of(&path);
        self.bind(path);
        self.saved_position = Some(self.position);
        Ok(())
    }

    /// Writes the content to `path` atomically without changing the buffer state.
    pub fn write_copy(&self, path: &Path) -> Result<(), BufferError> {
        let io_err = |source| BufferError::Io {
            path: path.to_path_buf(),
            source,
        };
        // Write through symlinks so the link itself is preserved.
        let is_link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        let resolved = if is_link {
            fs::canonicalize(path).map_err(io_err)?
        } else {
            path.to_path_buf()
        };
        let path = resolved.as_path();
        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(TMP_SUFFIX);
        let tmp = PathBuf::from(tmp_name);
        let crlf = self.line_ending == LineEnding::Crlf;
        write_synced(&tmp, &self.rope, crlf).map_err(io_err)?;
        // Keep the original file mode (the rename creates a new inode).
        if let Ok(meta) = fs::metadata(path)
            && let Err(e) = fs::set_permissions(&tmp, meta.permissions())
        {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
        if let Err(e) = fs::rename(&tmp, path) {
            // Best effort cleanup; the rename error is the one worth reporting.
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
        // Without this a crash right after the rename may still lose the new directory entry.
        sync_dir(parent_dir(path)).map_err(io_err)
    }

    /// Returns the underlying rope (always LF-only).
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Returns the whole content as a `String`.
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Returns the line ending used when saving.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Returns true when the content differs from the last saved state, including edits
    /// collected in a still open undo group.
    pub fn is_modified(&self) -> bool {
        self.saved_position != Some(self.position) || !self.group_edits.is_empty()
    }

    /// Returns true when the buffer is big or has very long lines: computed at creation and
    /// turned on by an edit that makes it so (it never turns off again, so the editor does
    /// not flip highlighting and wrap on and off while such text is edited).
    pub fn is_large(&self) -> bool {
        self.large
    }

    /// Returns a counter incremented on every content change.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replaces the char `range` with `text` and records an undo step.
    /// Insert uses an empty range, delete uses empty text.
    pub fn replace(&mut self, range: Range<usize>, text: &str) {
        let len = self.rope.len_chars();
        let range = range.start.min(len)..range.end.min(len).max(range.start.min(len));
        if range.is_empty() && text.is_empty() {
            return;
        }
        let text = normalize_newlines(text);
        let edit = self.apply(range, &text);
        if self.group_depth > 0 {
            self.group_edits.push(edit);
            return;
        }
        self.push_step(vec![edit]);
    }

    /// Starts (or nests) an undo group; edits until the matching `end_group` form one step.
    pub fn begin_group(&mut self) {
        self.group_depth += 1;
    }

    /// Ends an undo group; the outermost end commits the merged step.
    pub fn end_group(&mut self) {
        if self.group_depth == 0 {
            return;
        }
        self.group_depth -= 1;
        if self.group_depth > 0 || self.group_edits.is_empty() {
            return;
        }
        let edits = std::mem::take(&mut self.group_edits);
        self.push_step(edits);
    }

    /// Replaces each char range of `edits` with its text, in the given order (so later
    /// ranges must not be shifted by earlier edits: pass them from the end of the text
    /// backwards), as one undo step (or as part of the open undo group). The batch is one
    /// revision and one `dirty_lines_since` entry, however many edits it has.
    pub fn replace_many(&mut self, edits: Vec<(Range<usize>, String)>) {
        let mut applied = Vec::with_capacity(edits.len());
        let mut spans = Vec::with_capacity(edits.len());
        for (range, text) in edits {
            let len = self.rope.len_chars();
            let range = range.start.min(len)..range.end.min(len).max(range.start.min(len));
            if range.is_empty() && text.is_empty() {
                continue;
            }
            let text = normalize_newlines(&text);
            let (edit, span) = self.apply_raw(range, &text);
            applied.push(edit);
            spans.push(span);
        }
        if applied.is_empty() {
            return;
        }
        self.log_batch(&spans);
        if self.group_depth > 0 {
            self.group_edits.extend(applied);
            return;
        }
        self.push_step(applied);
    }

    /// Undoes the last step; returns the char index for the cursor. Callers must close any
    /// open undo group first.
    pub fn undo(&mut self) -> Option<usize> {
        debug_assert_eq!(self.group_depth, 0, "undo with an open undo group");
        if self.position == 0 {
            return None;
        }
        self.position -= 1;
        // Moved out while applying (`apply_raw` needs `&mut self`), never cloned.
        let step = std::mem::take(&mut self.history[self.position]);
        let mut cursor = 0;
        let mut spans = Vec::with_capacity(step.len());
        for edit in step.iter().rev() {
            let end = edit.pos + edit.inserted.chars().count();
            spans.push(self.apply_raw(edit.pos..end, &edit.removed).1);
            cursor = edit.pos + edit.removed.chars().count();
        }
        self.log_batch(&spans);
        self.history[self.position] = step;
        Some(cursor)
    }

    /// Redoes the next step; returns the char index for the cursor. Callers must close any
    /// open undo group first.
    pub fn redo(&mut self) -> Option<usize> {
        debug_assert_eq!(self.group_depth, 0, "redo with an open undo group");
        if self.position >= self.history.len() {
            return None;
        }
        let step = std::mem::take(&mut self.history[self.position]);
        let mut cursor = 0;
        let mut spans = Vec::with_capacity(step.len());
        for edit in &step {
            let end = edit.pos + edit.removed.chars().count();
            spans.push(self.apply_raw(edit.pos..end, &edit.inserted).1);
            cursor = edit.pos + edit.inserted.chars().count();
        }
        self.log_batch(&spans);
        self.history[self.position] = step;
        self.position += 1;
        Some(cursor)
    }

    /// Sets mark `name` at char index `pos`.
    pub fn set_mark(&mut self, name: char, pos: usize) {
        match self.marks.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = pos,
            None => self.marks.push((name, pos)),
        }
    }

    /// Returns the char index of mark `name`.
    pub fn mark(&self, name: char) -> Option<usize> {
        self.marks.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
    }

    /// Returns the lines changed after revision `since`, merged into one range, or `None`
    /// when the buffer is still at `since`. Every caller tracks its own revision.
    ///
    /// When `since` is older than the remembered edits (or newer than the buffer, i.e. a
    /// different buffer) the whole buffer counts as replaced.
    pub fn dirty_lines_since(&self, since: u64) -> Option<DirtyLines> {
        if since == self.revision {
            return None;
        }
        let everything = DirtyLines {
            start: 0,
            end: self.rope.len_lines(),
            old_end: usize::MAX,
        };
        let covered = since < self.revision
            && self
                .edit_log
                .front()
                .is_some_and(|e| e.revision <= since + 1);
        if !covered {
            return Some(everything);
        }
        let first = self.edit_log.partition_point(|e| e.revision <= since);
        let merged = merge_spans(self.edit_log.range(first..).map(|e| e.span))?;
        Some(merged)
    }

    /// Returns the file name, or `"untitled"` when unnamed.
    pub fn display_name(&self) -> String {
        self.path
            .as_deref()
            .and_then(|p| p.file_name())
            .map_or_else(
                || "untitled".to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
    }

    /// Pushes a finished step, dropping any redo tail, then drops the oldest steps while the
    /// history is over `MAX_UNDO_STEPS` or `MAX_UNDO_BYTES` (the newest step always stays).
    fn push_step(&mut self, edits: Vec<Edit>) {
        for dropped in self.history.drain(self.position..) {
            self.history_bytes -= step_bytes(&dropped);
        }
        if self.saved_position.is_some_and(|s| s > self.position) {
            self.saved_position = None;
        }
        self.history_bytes += step_bytes(&edits);
        self.history.push_back(edits);
        self.position += 1;
        while self.history.len() > 1
            && (self.history.len() > MAX_UNDO_STEPS || self.history_bytes > MAX_UNDO_BYTES)
        {
            let oldest = self.history.pop_front().expect("history is not empty");
            self.history_bytes -= step_bytes(&oldest);
            self.position -= 1;
            // The saved state was before the dropped step: it can no longer be reached.
            self.saved_position = self.saved_position.and_then(|s| s.checked_sub(1));
        }
    }

    /// Applies a raw replacement as its own revision: updates rope, revision, edit log and
    /// marks.
    fn apply(&mut self, range: Range<usize>, text: &str) -> Edit {
        let (edit, span) = self.apply_raw(range, text);
        self.log_batch(&[span]);
        edit
    }

    /// Replaces `range` with `text` in the rope and updates the large flag and the marks,
    /// without a new revision. Returns the undo record and the lines it touched.
    fn apply_raw(&mut self, range: Range<usize>, text: &str) -> (Edit, LineSpan) {
        let removed = self.rope.slice(range.clone()).to_string();
        let new_len = text.chars().count();
        let old_len = range.len();
        let line = self.rope.char_to_line(range.start);
        let old_last = self.rope.char_to_line(range.end);
        self.rope.remove(range.clone());
        self.rope.insert(range.start, text);
        let new_last = self.rope.char_to_line(range.start + new_len);
        self.large = self.large || is_large_text(&self.rope, line..new_last + 1);
        for (_, pos) in &mut self.marks {
            if *pos >= range.end {
                *pos = *pos + new_len - old_len;
            } else if *pos > range.start {
                *pos = range.start;
            }
        }
        let edit = Edit {
            pos: range.start,
            removed,
            inserted: text.to_string(),
        };
        let span = LineSpan {
            start: line,
            old_last,
            new_last,
        };
        (edit, span)
    }

    /// Ends a batch of raw edits (`spans`, in the order applied): one new revision and one edit
    /// log entry covering all of them. Does nothing for an empty batch.
    fn log_batch(&mut self, spans: &[LineSpan]) {
        let Some(merged) = merge_spans(spans.iter().copied()) else {
            return;
        };
        self.revision += 1;
        if self.edit_log.len() == EDIT_LOG_LEN {
            self.edit_log.pop_front();
        }
        self.edit_log.push_back(LoggedEdit {
            revision: self.revision,
            span: LineSpan {
                start: merged.start,
                old_last: merged.old_end - 1,
                new_last: merged.end - 1,
            },
        });
    }
}

/// Lines touched by one primitive edit: `start..=old_last` before it became
/// `start..=new_last` after it.
#[derive(Clone, Copy, Debug)]
struct LineSpan {
    /// First line of the edit.
    start: usize,
    /// Last line the replaced text covered, before the edit.
    old_last: usize,
    /// Last line the inserted text covers, after the edit.
    new_last: usize,
}

/// Merges consecutive edits (in the order applied) into the one window of lines they changed,
/// as `dirty_lines_since` reports it; `None` without edits.
fn merge_spans(spans: impl IntoIterator<Item = LineSpan>) -> Option<DirtyLines> {
    let mut dirty: Option<(usize, usize, isize)> = None;
    for e in spans {
        let shift = e.new_last as isize - e.old_last as isize;
        let (start, end, delta) = match dirty {
            None => (e.start, e.new_last + 1, shift),
            Some((start, end, delta)) => {
                // Map the window end through this edit (after it: shifted; inside the
                // replaced lines: the end of the inserted ones; before it: unchanged),
                // then make the window cover the edit.
                let end = if end > e.old_last + 1 {
                    (end as isize + shift) as usize
                } else if end > e.start {
                    e.new_last + 1
                } else {
                    end
                };
                (start.min(e.start), end.max(e.new_last + 1), delta + shift)
            }
        };
        dirty = Some((start, end, delta));
    }
    let (start, end, delta) = dirty?;
    Some(DirtyLines {
        start,
        end,
        old_end: (end as isize - delta) as usize,
    })
}

/// Creates (or truncates) the file at `path`, streams the chunks of `rope` into it (LF
/// written as CRLF when `crlf` is set) and flushes it to the disk, so a rename over the
/// original never exposes a file whose data is not stored yet. The text is never copied
/// into one big string.
fn write_synced(path: &Path, rope: &Rope, crlf: bool) -> std::io::Result<()> {
    let mut out = BufWriter::new(fs::File::create(path)?);
    for chunk in rope.chunks() {
        match crlf {
            true => write_crlf(&mut out, chunk)?,
            false => out.write_all(chunk.as_bytes())?,
        }
    }
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()
}

/// Writes `chunk` to `out` with every `\n` written as `\r\n`.
fn write_crlf(out: &mut impl Write, chunk: &str) -> std::io::Result<()> {
    let mut pieces = chunk.split('\n');
    if let Some(first) = pieces.next() {
        out.write_all(first.as_bytes())?;
    }
    for piece in pieces {
        out.write_all(b"\r\n")?;
        out.write_all(piece.as_bytes())?;
    }
    Ok(())
}

/// Directory holding `path`: its parent, or `.` for a bare file name.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// Flushes the entries of directory `dir` (a rename into it) to the disk.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Directories cannot be opened for syncing on this platform; the rename is durable once the
/// file system commits it.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// True when paths `a` and `b` name the same file: equal paths, or equal canonical forms
/// (`a_canonical` and `b_canonical`, `None` when that file does not exist).
pub fn same_file(
    a: &Path,
    a_canonical: Option<&Path>,
    b: &Path,
    b_canonical: Option<&Path>,
) -> bool {
    a == b || (a_canonical.is_some() && a_canonical == b_canonical)
}

/// True when `rope` is at least `LARGE_FILE_BYTES` long or one of `lines` is longer than
/// `LONG_LINE_CHARS` chars.
fn is_large_text(rope: &Rope, lines: Range<usize>) -> bool {
    rope.len_bytes() >= LARGE_FILE_BYTES
        || lines
            .into_iter()
            .any(|l| rope.line(l).len_chars() > LONG_LINE_CHARS)
}

/// Text size of an undo step: the bytes it removed plus the bytes it inserted.
fn step_bytes(step: &[Edit]) -> usize {
    step.iter()
        .map(|e| e.removed.len() + e.inserted.len())
        .sum()
}

/// Returns the majority line ending of `text`: CRLF only when CRLF line breaks outnumber
/// bare LF ones. Lone CR is not counted (it is normalized to LF).
fn detect_line_ending(text: &str) -> LineEnding {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    match crlf > lf {
        true => LineEnding::Crlf,
        false => LineEnding::Lf,
    }
}

/// Reads the UTF-8 text of the file at `path` and its stamp taken before the read (so a
/// change during the read is noticed later). A missing file gives `(None, None)`; a file over
/// `MAX_FILE_BYTES` is refused before it is read.
fn read_file(path: &Path) -> Result<(Option<String>, Option<DiskStamp>), BufferError> {
    check_size(path)?;
    let disk = DiskStamp::of(path);
    match fs::read(path) {
        Ok(bytes) => {
            let text =
                String::from_utf8(bytes).map_err(|_| BufferError::NotUtf8(path.to_path_buf()))?;
            Ok((Some(text), disk))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, None)),
        Err(source) => Err(BufferError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Returns the size of the file at `path` in bytes, 0 when it cannot be read (a missing
/// file opens empty, other errors are reported by the read itself).
pub fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

/// Returns the size of the file at `path` (see `file_size`), or `TooLarge` when it is bigger
/// than `MAX_FILE_BYTES`.
pub fn check_size(path: &Path) -> Result<u64, BufferError> {
    let size = file_size(path);
    if size > MAX_FILE_BYTES {
        return Err(BufferError::TooLarge {
            path: path.to_path_buf(),
            size,
            limit: MAX_FILE_BYTES,
        });
    }
    Ok(size)
}

/// Builds the rope of `text` with `\r\n` and lone `\r` mapped to `\n`, appending the pieces
/// between carriage returns instead of building a normalized copy of the whole text first.
fn build_rope(text: &str) -> Rope {
    if !text.contains('\r') {
        return Rope::from_str(text);
    }
    let mut builder = RopeBuilder::new();
    let mut rest = text;
    while let Some(i) = rest.find('\r') {
        builder.append(&rest[..i]);
        let after = &rest[i + 1..];
        if !after.starts_with('\n') {
            builder.append("\n");
        }
        rest = after;
    }
    builder.append(rest);
    builder.finish()
}

/// A file being loaded on a background thread.
pub struct LoadJob {
    /// The file being loaded.
    path: PathBuf,
    /// Canonical form of `path` (`None` when the file does not exist).
    canonical: Option<PathBuf>,
    /// The load running in the background.
    job: BackgroundJob<Result<Buffer, BufferError>>,
}

impl LoadJob {
    /// Starts loading `path` with `Buffer::load` on a new thread.
    pub fn spawn(path: PathBuf) -> LoadJob {
        let thread_path = path.clone();
        let job = BackgroundJob::spawn(None, move || Buffer::load(&thread_path));
        let canonical = fs::canonicalize(&path).ok();
        LoadJob {
            path,
            canonical,
            job,
        }
    }

    /// The file being loaded.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when this job loads the file `path` with canonical form `canonical`.
    pub fn is_loading(&self, path: &Path, canonical: Option<&Path>) -> bool {
        same_file(&self.path, self.canonical.as_deref(), path, canonical)
    }

    /// Returns the result once the thread is done, `None` while it is still loading.
    pub fn poll(&self) -> Option<Result<Buffer, BufferError>> {
        let result = self.job.poll()?;
        Some(result.unwrap_or_else(|e| Err(BufferError::LoadInterrupted(self.path.clone(), e))))
    }
}

/// Maps `\r\n` and lone `\r` to `\n` so the rope never contains `\r`.
pub fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return text.into();
    }
    text.replace("\r\n", "\n").replace('\r', "\n").into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a fresh unique temp directory for a test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fed-buffer-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_batch_of_edits_is_one_revision_and_one_log_entry() {
        let text: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        let mut buf = Buffer::from_text(&text);
        let rev = buf.revision();
        // Every line of 0..2000 gets a suffix, from the end backwards.
        let edits: Vec<(Range<usize>, String)> = (0..2000)
            .rev()
            .map(|l| {
                let end = buf.rope().line_to_char(l + 1) - 1;
                (end..end, "!\nx".to_string())
            })
            .collect();
        buf.replace_many(edits);
        assert_eq!(buf.revision(), rev + 1);
        assert_eq!(
            buf.dirty_lines_since(rev),
            Some(DirtyLines {
                start: 0,
                end: 4000,
                old_end: 2000,
            })
        );
        assert!(buf.text().starts_with("line 0!\nx\nline 1!\nx\n"));
        let after = buf.revision();
        assert!(buf.undo().is_some());
        assert_eq!(buf.text(), text, "one undo step");
        assert_eq!(buf.revision(), after + 1);
        let dirty = buf.dirty_lines_since(after).unwrap();
        assert_eq!((dirty.start, dirty.end, dirty.old_end), (0, 2000, 4000));
        buf.redo();
        assert_eq!(buf.dirty_lines_since(rev).map(|d| d.old_end), Some(2000));
    }

    #[test]
    fn save_refuses_a_file_changed_on_disk() {
        let dir = temp_dir("changed");
        let path = dir.join("a.txt");
        fs::write(&path, "one\n").unwrap();
        let mut buf = Buffer::load(&path).unwrap();
        assert!(!buf.changed_on_disk());
        buf.replace(0..0, "x");
        fs::write(&path, "other program\n").unwrap();
        assert!(buf.changed_on_disk());
        assert!(matches!(buf.save(), Err(BufferError::ChangedOnDisk(_))));
        assert_eq!(fs::read_to_string(&path).unwrap(), "other program\n");
        buf.save_overwrite().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "xone\n");
        assert!(!buf.changed_on_disk(), "own saves do not count");
        buf.replace(0..1, "");
        buf.save().unwrap();
        fs::remove_file(&path).unwrap();
        assert!(buf.changed_on_disk(), "a deleted file counts as changed");
    }

    #[test]
    fn a_missing_file_created_by_someone_else_counts_as_changed() {
        let dir = temp_dir("created");
        let path = dir.join("new.txt");
        let buf = Buffer::load(&path).unwrap();
        assert!(!buf.changed_on_disk());
        fs::write(&path, "hi").unwrap();
        assert!(buf.changed_on_disk());
    }

    #[test]
    fn reload_takes_the_disk_text_as_one_undo_step() {
        let dir = temp_dir("reload");
        let path = dir.join("a.txt");
        fs::write(&path, "old\n").unwrap();
        let mut buf = Buffer::load(&path).unwrap();
        buf.replace(0..0, "mine ");
        fs::write(&path, "new\r\ntext\r\n").unwrap();
        buf.reload().unwrap();
        assert_eq!(buf.text(), "new\ntext\n");
        assert_eq!(buf.line_ending(), LineEnding::Crlf);
        assert!(!buf.is_modified());
        assert!(!buf.changed_on_disk());
        buf.undo();
        assert_eq!(buf.text(), "mine old\n");
    }

    #[test]
    fn buffers_compare_files_by_canonical_path() {
        let dir = temp_dir("canonical");
        let path = dir.join("a.txt");
        fs::create_dir_all(dir.join("sub")).unwrap();
        // `Path` equality ignores `.` components but not `..`.
        let dotted = dir.join("sub").join("..").join("a.txt");
        let mut buf = Buffer::load(&path).unwrap();
        let canonical = |p: &Path| fs::canonicalize(p).ok();
        assert!(buf.is_bound_to(&path, None), "the same path, file missing");
        assert!(!buf.is_bound_to(&dotted, canonical(&dotted).as_deref()));
        buf.save().unwrap();
        let c = canonical(&dotted);
        assert!(c.is_some());
        assert!(buf.is_bound_to(&dotted, c.as_deref()), "known once saved");
        assert!(!buf.is_bound_to(&dir.join("b.txt"), None));
        let loaded = Buffer::load(&dotted).unwrap();
        assert!(loaded.is_bound_to(&path, canonical(&path).as_deref()));
        let job = LoadJob::spawn(dotted.clone());
        assert!(job.is_loading(&path, canonical(&path).as_deref()));
    }

    #[test]
    fn edits_can_make_a_buffer_large() {
        let mut buf = Buffer::from_text("short\nlines\n");
        assert!(!buf.is_large());
        // "short\n" plus the padding is exactly the limit (the line break counts).
        buf.replace(0..0, &"x".repeat(LONG_LINE_CHARS - 6));
        assert!(!buf.is_large(), "exactly the limit is not long");
        buf.replace(0..0, "x");
        assert!(buf.is_large(), "an over-long line");
        buf.undo();
        assert!(buf.is_large(), "large stays on");
        let mut buf = Buffer::from_text("a");
        let block = "y".repeat(LONG_LINE_CHARS - 1) + "\n";
        buf.replace(1..1, &block.repeat(LARGE_FILE_BYTES / block.len() + 1));
        assert!(buf.is_large(), "a big paste of short lines");
    }

    #[test]
    fn parent_dir_of_bare_name_is_current_dir() {
        assert_eq!(parent_dir(Path::new("a.txt")), Path::new("."));
        assert_eq!(parent_dir(Path::new("/x/a.txt")), Path::new("/x"));
        assert_eq!(parent_dir(Path::new("d/a.txt")), Path::new("d"));
    }

    #[test]
    fn synced_write_stores_content_and_dir_syncs() {
        let dir = temp_dir("synced");
        let path = dir.join("a.txt");
        write_synced(&path, &Rope::from_str("one"), false).unwrap();
        write_synced(&path, &Rope::from_str("t\nwo\n"), true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "t\r\nwo\r\n");
        sync_dir(&dir).unwrap();
        assert!(sync_dir(&dir.join("missing")).is_err());
    }

    #[test]
    fn crlf_conversion_handles_chunk_edges() {
        for (input, want) in [
            ("", ""),
            ("\n", "\r\n"),
            ("a\nb", "a\r\nb"),
            ("\n\na\n", "\r\n\r\na\r\n"),
        ] {
            let mut out = Vec::new();
            write_crlf(&mut out, input).unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), want, "{input:?}");
        }
    }

    #[test]
    fn multi_chunk_crlf_save_matches_text() {
        let line = "abcdefghij".repeat(10);
        let text: String = (0..2000).map(|i| format!("{i} {line}\n")).collect();
        let buf = Buffer::from_text(&text.replace('\n', "\r\n"));
        assert!(buf.rope().chunks().count() > 1);
        let path = temp_dir("chunks").join("a.txt");
        buf.write_copy(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            text.replace('\n', "\r\n")
        );
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let dir = temp_dir("no-tmp");
        let path = dir.join("a.txt");
        let mut buf = Buffer::from_text("hi\n");
        buf.save_as(path.clone()).unwrap();
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("a.txt")]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi\n");
    }

    #[test]
    fn crlf_roundtrip_preserves_endings() {
        let path = temp_dir("crlf").join("a.txt");
        fs::write(&path, "a\r\nb\r\n").unwrap();
        let mut buf = Buffer::load(&path).unwrap();
        assert_eq!(buf.rope().to_string(), "a\nb\n");
        assert_eq!(buf.line_ending(), LineEnding::Crlf);
        buf.replace(0..1, "x");
        buf.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"x\r\nb\r\n");
    }

    #[test]
    fn mostly_lf_file_with_one_crlf_saves_as_lf() {
        let path = temp_dir("mixed-lf").join("a.txt");
        fs::write(&path, "a\nb\r\nc\nd\n").unwrap();
        let mut buf = Buffer::load(&path).unwrap();
        assert_eq!(buf.line_ending(), LineEnding::Lf);
        buf.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\nb\nc\nd\n");
    }

    #[test]
    fn mostly_crlf_file_saves_as_crlf() {
        let path = temp_dir("mixed-crlf").join("a.txt");
        fs::write(&path, "a\r\nb\nc\r\nd\r\n").unwrap();
        let mut buf = Buffer::load(&path).unwrap();
        assert_eq!(buf.line_ending(), LineEnding::Crlf);
        buf.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\r\nc\r\nd\r\n");
    }

    #[test]
    fn line_ending_tie_and_lone_cr_go_to_lf() {
        assert_eq!(detect_line_ending("a\r\nb\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\rb\rc\r\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending(""), LineEnding::Lf);
        let buf = Buffer::from_text("a\rb");
        assert_eq!(buf.text(), "a\nb");
    }

    #[test]
    fn failed_save_as_keeps_previous_path() {
        let dir = temp_dir("saveas-fail");
        let old = dir.join("old.txt");
        let mut buf = Buffer::load(&old).unwrap();
        buf.replace(0..0, "x");
        let bad = dir.join("missing-dir").join("new.txt");
        assert!(buf.save_as(bad).is_err());
        assert_eq!(buf.path(), Some(old.as_path()));
        assert!(buf.is_modified());
    }

    #[test]
    fn form_feed_and_unicode_separators_are_not_line_breaks() {
        let buf = Buffer::from_text("a\x0cb\nc");
        assert_eq!(buf.rope().len_lines(), 2);
        for sep in ['\u{0b}', '\u{85}', '\u{2028}', '\u{2029}'] {
            let buf = Buffer::from_text(&format!("a{sep}b"));
            assert_eq!(buf.rope().len_lines(), 1, "{sep:?}");
        }
    }

    #[test]
    fn carriage_returns_become_line_feeds_in_the_rope() {
        for (text, want) in [
            ("a\r\nb\r\n", "a\nb\n"),
            ("a\rb\r", "a\nb\n"),
            ("\r\r\n\n", "\n\n\n"),
            ("plain\n", "plain\n"),
            ("è\r\nò", "è\nò"),
        ] {
            assert_eq!(build_rope(text).to_string(), want, "{text:?}");
        }
    }

    #[test]
    fn files_over_the_limit_are_refused_unread() {
        let path = temp_dir("too-large").join("big.bin");
        let file = fs::File::create(&path).unwrap();
        // Sparse: no data is written, only the size is set.
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        let err = Buffer::load(&path).err().expect("refused");
        assert!(matches!(err, BufferError::TooLarge { size, .. } if size == MAX_FILE_BYTES + 1));
        assert!(err.to_string().contains("too large"), "{err}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_job_delivers_the_buffer() {
        let path = temp_dir("load-job").join("a.txt");
        fs::write(&path, "x\r\ny\r\n").unwrap();
        let job = LoadJob::spawn(path.clone());
        assert_eq!(job.path(), path.as_path());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let buf = loop {
            if let Some(result) = job.poll() {
                break result.unwrap();
            }
            assert!(std::time::Instant::now() < deadline, "load never finished");
            std::thread::yield_now();
        };
        assert_eq!(buf.text(), "x\ny\n");
        assert_eq!(buf.path(), Some(path.as_path()));
    }

    #[test]
    fn non_utf8_is_error() {
        let path = temp_dir("utf8").join("bin");
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(Buffer::load(&path), Err(BufferError::NotUtf8(_))));
    }

    #[test]
    fn missing_file_opens_empty_with_path() {
        let path = temp_dir("missing").join("nope.txt");
        let buf = Buffer::load(&path).unwrap();
        assert_eq!(buf.text(), "");
        assert_eq!(buf.path(), Some(path.as_path()));
        assert!(!buf.is_modified());
    }

    #[test]
    fn undo_redo_restores_text_and_modified_flag() {
        let mut buf = Buffer::from_text("abc");
        buf.replace(3..3, "d");
        assert_eq!(buf.undo(), Some(3));
        assert_eq!(buf.text(), "abc");
        assert!(!buf.is_modified());
        assert_eq!(buf.redo(), Some(4));
        assert_eq!(buf.text(), "abcd");
        assert!(buf.is_modified());
    }

    #[test]
    #[should_panic(expected = "open undo group")]
    #[cfg(debug_assertions)]
    fn undo_inside_an_open_group_is_a_bug() {
        let mut buf = Buffer::from_text("abc");
        buf.begin_group();
        buf.replace(0..0, "x");
        buf.undo();
    }

    #[test]
    fn undo_history_keeps_the_newest_steps() {
        let mut buf = Buffer::from_text("");
        for _ in 0..MAX_UNDO_STEPS + 10 {
            buf.replace(0..0, "x");
        }
        assert_eq!(buf.history.len(), MAX_UNDO_STEPS);
        let mut undone = 0;
        while buf.undo().is_some() {
            undone += 1;
        }
        assert_eq!(undone, MAX_UNDO_STEPS);
        assert_eq!(buf.text(), "x".repeat(10));
        // The empty saved state was dropped with the oldest steps.
        assert!(buf.is_modified());
        while buf.redo().is_some() {}
        assert_eq!(buf.text().len(), MAX_UNDO_STEPS + 10);
    }

    #[test]
    fn undo_history_is_bounded_by_size() {
        let mut buf = Buffer::from_text("");
        let big = "y".repeat(MAX_UNDO_BYTES / 3);
        for _ in 0..4 {
            let len = buf.rope().len_chars();
            buf.replace(len..len, &big);
        }
        assert!(buf.history_bytes <= MAX_UNDO_BYTES);
        assert_eq!(buf.history.len(), 3);
        assert_eq!(buf.history_bytes, 3 * big.len());
        // Undo and redo leave the steps in place.
        buf.undo();
        buf.redo();
        assert_eq!(buf.history_bytes, 3 * big.len());
    }

    #[test]
    fn group_undoes_as_one_step() {
        let mut buf = Buffer::from_text("abc");
        buf.begin_group();
        buf.replace(0..0, "x");
        buf.replace(4..4, "y");
        buf.end_group();
        assert_eq!(buf.text(), "xabcy");
        buf.undo();
        assert_eq!(buf.text(), "abc");
        assert_eq!(buf.undo(), None);
    }

    #[test]
    fn marks_shift_on_edit() {
        let mut buf = Buffer::from_text("hello world");
        buf.set_mark('a', 6);
        buf.replace(0..0, "XX");
        assert_eq!(buf.mark('a'), Some(8));
        buf.replace(0..9, "");
        assert_eq!(buf.mark('a'), Some(0));
    }

    #[test]
    fn dirty_lines_are_per_caller() {
        let mut buf = Buffer::from_text("one\ntwo\nthree");
        let first = |buf: &Buffer, since| buf.dirty_lines_since(since).map(|d| d.start);
        let start = buf.revision();
        assert_eq!(first(&buf, start), None);
        buf.replace(8..8, "x");
        let mid = buf.revision();
        buf.replace(4..4, "y");
        assert_eq!(first(&buf, start), Some(1));
        assert_eq!(first(&buf, mid), Some(1));
        assert_eq!(first(&buf, buf.revision()), None);
        // A revision newer than the buffer means another buffer: everything is dirty.
        assert_eq!(first(&buf, buf.revision() + 5), Some(0));
    }

    #[test]
    fn dirty_lines_fall_back_to_everything_past_the_log() {
        let mut buf = Buffer::from_text("a\nb\nc");
        for _ in 0..EDIT_LOG_LEN + 1 {
            buf.replace(4..4, "x");
        }
        assert_eq!(
            buf.dirty_lines_since(0).map(|d| d.old_end),
            Some(usize::MAX)
        );
        assert_eq!(
            buf.dirty_lines_since(buf.revision() - 1).map(|d| d.start),
            Some(2)
        );
    }

    /// Checks `dirty_lines_since` against a line-by-line diff of the two texts.
    fn assert_dirty_lines_consistent(old: &str, buf: &Buffer, since: u64) {
        let old_lines: Vec<&str> = old.split('\n').collect();
        let new_text = buf.text();
        let new_lines: Vec<&str> = new_text.split('\n').collect();
        let d = buf.dirty_lines_since(since).expect("changed");
        let old_end = d.old_end.min(old_lines.len());
        assert_eq!(old_lines[..d.start], new_lines[..d.start], "prefix {d:?}");
        assert_eq!(
            old_lines[old_end..],
            new_lines[d.end..],
            "suffix {d:?} of {old:?} -> {new_text:?}"
        );
    }

    #[test]
    fn dirty_lines_since_merges_edits() {
        let old = "0\n1\n2\n3\n4\n5\n6\n7\n8\n9";
        let mut buf = Buffer::from_text(old);
        let rev = buf.revision();
        // Split line 2, join lines 6 and 7, delete line 0.
        buf.replace(5..5, "\n");
        let six = buf.rope().line_to_char(7);
        buf.replace(six + 1..six + 2, "");
        buf.replace(0..2, "");
        assert_dirty_lines_consistent(old, &buf, rev);
        let d = buf.dirty_lines_since(rev).unwrap();
        assert_eq!(d.start, 0);
        // A single insertion is exact.
        let mid = buf.revision();
        let before = buf.text();
        buf.replace(2..2, "a\nb\nc");
        let d = buf.dirty_lines_since(mid).unwrap();
        assert_eq!(d.end - d.start, 3);
        assert_eq!(d.old_end - d.start, 1);
        assert_dirty_lines_consistent(&before, &buf, mid);
    }

    #[test]
    fn dirty_lines_since_survives_random_edits() {
        let mut seed: u64 = 0x5eed;
        let mut next = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n.max(1)
        };
        let pieces = ["", "x", "\n", "ab\ncd", "\n\n", "q\n"];
        for _ in 0..300 {
            let old = "l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7";
            let mut buf = Buffer::from_text(old);
            let rev = buf.revision();
            for _ in 0..1 + next(6) {
                let len = buf.rope().len_chars();
                let start = next(len + 1);
                let end = (start + next(4)).min(len);
                buf.replace(start..end, pieces[next(pieces.len())]);
            }
            if buf.revision() != rev {
                assert_dirty_lines_consistent(old, &buf, rev);
            }
        }
    }

    #[test]
    fn dirty_lines_since_unknown_history_replaces_everything() {
        let buf = Buffer::from_text("a\nb");
        assert_eq!(
            buf.dirty_lines_since(7),
            Some(DirtyLines {
                start: 0,
                end: 2,
                old_end: usize::MAX
            })
        );
    }

    #[test]
    fn large_file_flag() {
        let buf = Buffer::from_text(&"a".repeat(10_001));
        assert!(buf.is_large());
        assert!(!Buffer::from_text("short").is_large());
    }

    #[test]
    fn saved_state_is_lost_when_redo_tail_is_dropped() {
        let path = temp_dir("saved").join("s.txt");
        let mut buf = Buffer::from_text("a");
        buf.replace(1..1, "b");
        buf.save_as(path).unwrap();
        assert!(!buf.is_modified());
        buf.undo();
        buf.replace(1..1, "c");
        buf.redo();
        assert!(buf.is_modified());
    }

    #[test]
    fn replace_and_load_normalize_carriage_returns() {
        let mut buf = Buffer::from_text("x\ry");
        assert_eq!(buf.text(), "x\ny");
        buf.replace(0..0, "a\r\nb");
        assert!(!buf.text().contains('\r'));
        assert_eq!(buf.text(), "a\nbx\ny");
        buf.undo();
        assert_eq!(buf.text(), "x\ny");
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_mode_and_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = temp_dir("mode");
        let path = dir.join("run.sh");
        fs::write(&path, "echo\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let mut buf = Buffer::load(&path).unwrap();
        buf.replace(0..0, "# ");
        buf.save().unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let link = dir.join("link.sh");
        symlink(&path, &link).unwrap();
        let mut lbuf = Buffer::load(&link).unwrap();
        lbuf.replace(0..0, "!");
        lbuf.save().unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "!# echo\n");
    }
}
