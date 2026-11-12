//! Rope-backed text buffer with undo/redo, marks, line endings and atomic saving.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use ropey::Rope;

/// Files at or above this many bytes are flagged as large.
pub const LARGE_FILE_BYTES: usize = 5_000_000;
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
    /// Recent edits as `(revision after the edit, first edited line)`, oldest first.
    edit_log: VecDeque<(u64, usize)>,
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
        let rope = Rope::from_str(&normalize_newlines(text));
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

    /// Loads `path`. A missing file yields an empty buffer bound to `path`.
    pub fn load(path: &Path) -> Result<Buffer, BufferError> {
        let io_err = |source| BufferError::Io {
            path: path.to_path_buf(),
            source,
        };
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

    /// Saves to the bound path.
    pub fn save(&mut self) -> Result<(), BufferError> {
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

    /// Undoes the last step; returns the char index for the cursor.
    pub fn undo(&mut self) -> Option<usize> {
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

    /// Redoes the next step; returns the char index for the cursor.
    pub fn redo(&mut self) -> Option<usize> {
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
    /// still at `since`. Every caller tracks its own revision, unlike `take_dirty_line`.
    /// When `since` is older than the remembered edits (or newer than the buffer, i.e. a
    /// different buffer), the whole buffer counts as dirty and line 0 is returned.
    pub fn first_dirty_line_since(&self, since: u64) -> Option<usize> {
        if since == self.revision {
            return None;
        }
        let covered = since < self.revision
            && self
                .edit_log
                .front()
                .is_some_and(|&(rev, _)| rev <= since + 1);
        if !covered {
            return Some(0);
        }
        self.edit_log
            .iter()
            .rev()
            .take_while(|(rev, _)| *rev > since)
            .map(|(_, line)| *line)
            .min()
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
        let dirty = self.dirty_line.get();
        self.dirty_line
            .set(Some(dirty.map_or(line, |d| d.min(line))));
        self.rope.remove(range.clone());
        self.rope.insert(range.start, text);
        self.revision += 1;
        if self.edit_log.len() == EDIT_LOG_LEN {
            self.edit_log.pop_front();
        }
        self.edit_log.push_back((self.revision, line));
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

/// Maps `\r\n` and lone `\r` to `\n` so the rope never contains `\r`.
fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
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
