//! Rope-backed text buffer with undo/redo, marks, line endings and atomic saving.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use ropey::{Rope, RopeBuilder};

/// Files at or above this many bytes are flagged as large.
pub const LARGE_FILE_BYTES: usize = 5_000_000;
/// Files bigger than this many bytes are refused instead of opened.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Files at or above this many bytes are read on a background thread.
pub const BACKGROUND_LOAD_BYTES: u64 = 1024 * 1024;
/// Lines longer than this many chars flag the buffer as large.
pub const LONG_LINE_CHARS: usize = 10_000;
/// Number of recent edits remembered by `first_dirty_line_since`.
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
    /// The background loader thread stopped without a result.
    #[error("{0}: loading was interrupted")]
    LoadInterrupted(PathBuf),
    /// A save was requested but the buffer is not bound to a path.
    #[error("buffer has no file path")]
    NoPath,
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

/// Lines touched by one primitive edit, remembered for `dirty_lines_since`.
#[derive(Clone, Copy, Debug)]
struct LoggedEdit {
    /// Buffer revision right after the edit.
    revision: u64,
    /// First line of the edit.
    start: usize,
    /// Last line the replaced text covered, before the edit.
    old_last: usize,
    /// Last line the inserted text covers, after the edit.
    new_last: usize,
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
    line_ending: LineEnding,
    large: bool,
    revision: u64,
    history: Vec<Vec<Edit>>,
    /// Number of history steps currently applied.
    position: usize,
    /// History position at the last save, if still reachable.
    saved_position: Option<usize>,
    group_depth: usize,
    group_edits: Vec<Edit>,
    marks: Vec<(char, usize)>,
    /// Lowest edited line since the last take (a `Cell` so decorators can take it from `&Buffer`).
    dirty_line: Cell<Option<usize>>,
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
        let large =
            text.len() >= LARGE_FILE_BYTES || rope.lines().any(|l| l.len_chars() > LONG_LINE_CHARS);
        Buffer {
            rope,
            path: None,
            line_ending: if crlf {
                LineEnding::Crlf
            } else {
                LineEnding::Lf
            },
            large,
            revision: 0,
            history: Vec::new(),
            position: 0,
            saved_position: Some(0),
            group_depth: 0,
            group_edits: Vec::new(),
            marks: Vec::new(),
            dirty_line: Cell::new(None),
            edit_log: VecDeque::new(),
        }
    }

    /// Loads `path`. A missing file yields an empty buffer bound to `path`; a file bigger than
    /// `MAX_FILE_BYTES` is refused before it is read. The text is held at most twice while
    /// loading (the file contents and the rope).
    pub fn load(path: &Path) -> Result<Buffer, BufferError> {
        let io_err = |source| BufferError::Io {
            path: path.to_path_buf(),
            source,
        };
        check_size(path)?;
        let mut buf = match fs::read(path) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|_| BufferError::NotUtf8(path.to_path_buf()))?;
                Self::from_text(&text)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::new(),
            Err(e) => return Err(io_err(e)),
        };
        buf.path = Some(path.to_path_buf());
        Ok(buf)
    }

    /// Returns the bound file path, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Binds the buffer to `path` without saving.
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
    }

    /// Saves to the bound path. Callers must close any open undo group first.
    pub fn save(&mut self) -> Result<(), BufferError> {
        debug_assert_eq!(self.group_depth, 0, "save with an open undo group");
        let path = self.path.clone().ok_or(BufferError::NoPath)?;
        self.write_copy(&path)?;
        self.saved_position = Some(self.position);
        Ok(())
    }

    /// Saves to `path` and binds the buffer to it. On failure the buffer keeps its previous
    /// path, so a failed "Save As" does not silently retarget later saves.
    pub fn save_as(&mut self, path: PathBuf) -> Result<(), BufferError> {
        self.write_copy(&path)?;
        self.path = Some(path);
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
        let mut content = self.rope.to_string();
        if self.line_ending == LineEnding::Crlf {
            content = content.replace('\n', "\r\n");
        }
        fs::write(&tmp, content).map_err(io_err)?;
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
        Ok(())
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

    /// Returns true when the buffer is big or has very long lines (computed at creation).
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

    /// Undoes the last step; returns the char index for the cursor. Callers must close any
    /// open undo group first.
    pub fn undo(&mut self) -> Option<usize> {
        debug_assert_eq!(self.group_depth, 0, "undo with an open undo group");
        if self.position == 0 {
            return None;
        }
        self.position -= 1;
        let step = self.history[self.position].clone();
        let mut cursor = 0;
        for edit in step.iter().rev() {
            let end = edit.pos + edit.inserted.chars().count();
            self.apply(edit.pos..end, &edit.removed);
            cursor = edit.pos + edit.removed.chars().count();
        }
        Some(cursor)
    }

    /// Redoes the next step; returns the char index for the cursor. Callers must close any
    /// open undo group first.
    pub fn redo(&mut self) -> Option<usize> {
        debug_assert_eq!(self.group_depth, 0, "redo with an open undo group");
        if self.position >= self.history.len() {
            return None;
        }
        let step = self.history[self.position].clone();
        self.position += 1;
        let mut cursor = 0;
        for edit in &step {
            let end = edit.pos + edit.removed.chars().count();
            self.apply(edit.pos..end, &edit.inserted);
            cursor = edit.pos + edit.inserted.chars().count();
        }
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

    /// Returns and clears the lowest line index touched since the last call.
    pub fn take_dirty_line(&self) -> Option<usize> {
        self.dirty_line.take()
    }

    /// Returns the lowest line edited after revision `since`, or `None` when the buffer is
    /// still at `since`. See `dirty_lines_since`.
    pub fn first_dirty_line_since(&self, since: u64) -> Option<usize> {
        self.dirty_lines_since(since).map(|d| d.start)
    }

    /// Returns the lines changed after revision `since`, merged into one range, or `None`
    /// when the buffer is still at `since`. Every caller tracks its own revision, unlike
    /// `take_dirty_line`. When `since` is older than the remembered edits (or newer than the
    /// buffer, i.e. a different buffer) the whole buffer counts as replaced.
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
        let mut dirty: Option<(usize, usize, isize)> = None;
        for e in self.edit_log.range(first..) {
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

    /// Pushes a finished step, dropping any redo tail.
    fn push_step(&mut self, edits: Vec<Edit>) {
        self.history.truncate(self.position);
        if self.saved_position.is_some_and(|s| s > self.position) {
            self.saved_position = None;
        }
        self.history.push(edits);
        self.position += 1;
    }

    /// Applies a raw replacement: updates rope, revision, dirty line and marks.
    fn apply(&mut self, range: Range<usize>, text: &str) -> Edit {
        let removed = self.rope.slice(range.clone()).to_string();
        let new_len = text.chars().count();
        let old_len = range.len();
        let line = self.rope.char_to_line(range.start);
        let old_last = self.rope.char_to_line(range.end);
        let dirty = self.dirty_line.get();
        self.dirty_line
            .set(Some(dirty.map_or(line, |d| d.min(line))));
        self.rope.remove(range.clone());
        self.rope.insert(range.start, text);
        self.revision += 1;
        if self.edit_log.len() == EDIT_LOG_LEN {
            self.edit_log.pop_front();
        }
        self.edit_log.push_back(LoggedEdit {
            revision: self.revision,
            start: line,
            old_last,
            new_last: self.rope.char_to_line(range.start + new_len),
        });
        for (_, pos) in &mut self.marks {
            if *pos >= range.end {
                *pos = *pos + new_len - old_len;
            } else if *pos > range.start {
                *pos = range.start;
            }
        }
        Edit {
            pos: range.start,
            removed,
            inserted: text.to_string(),
        }
    }
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
    /// Receives the load result.
    rx: Receiver<Result<Buffer, BufferError>>,
}

impl LoadJob {
    /// Starts loading `path` with `Buffer::load` on a new thread.
    pub fn spawn(path: PathBuf) -> LoadJob {
        let (tx, rx) = mpsc::channel();
        let thread_path = path.clone();
        std::thread::spawn(move || {
            // Nobody listening (the app closed) is fine.
            let _ = tx.send(Buffer::load(&thread_path));
        });
        LoadJob { path, rx }
    }

    /// The file being loaded.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the result once the thread is done, `None` while it is still loading.
    pub fn poll(&self) -> Option<Result<Buffer, BufferError>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err(BufferError::LoadInterrupted(self.path.clone())))
            }
        }
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
    fn dirty_line_reports_lowest_edited_line() {
        let mut buf = Buffer::from_text("one\ntwo\nthree");
        buf.replace(8..8, "x");
        buf.replace(4..4, "y");
        assert_eq!(buf.take_dirty_line(), Some(1));
        assert_eq!(buf.take_dirty_line(), None);
    }

    #[test]
    fn first_dirty_line_since_is_per_caller() {
        let mut buf = Buffer::from_text("one\ntwo\nthree");
        let start = buf.revision();
        assert_eq!(buf.first_dirty_line_since(start), None);
        buf.replace(8..8, "x");
        let mid = buf.revision();
        buf.replace(4..4, "y");
        assert_eq!(buf.first_dirty_line_since(start), Some(1));
        assert_eq!(buf.first_dirty_line_since(mid), Some(1));
        // The single-consumer dirty line is not consumed by these queries.
        assert_eq!(buf.take_dirty_line(), Some(1));
        assert_eq!(buf.first_dirty_line_since(buf.revision()), None);
        // A revision newer than the buffer means another buffer: everything is dirty.
        assert_eq!(buf.first_dirty_line_since(buf.revision() + 5), Some(0));
    }

    #[test]
    fn first_dirty_line_since_falls_back_to_zero_past_the_log() {
        let mut buf = Buffer::from_text("a\nb\nc");
        for _ in 0..EDIT_LOG_LEN + 1 {
            buf.replace(4..4, "x");
        }
        assert_eq!(buf.first_dirty_line_since(0), Some(0));
        assert_eq!(buf.first_dirty_line_since(buf.revision() - 1), Some(2));
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
