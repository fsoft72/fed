//! An open document: buffer, editor state and per-tab view caches.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::buffer::Buffer;
use crate::editing::{self, EditCommand, EditSettings, EditorState};
use crate::highlight::HighlightCache;
use crate::view::ViewState;

/// Source of unique tab ids.
static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);

/// One open document shown in a tab.
pub struct Tab {
    /// Unique id, stable for the lifetime of the tab (used for widget ids).
    pub id: u64,
    /// The text being edited.
    pub buffer: Buffer,
    /// Selection and caret state.
    pub editor: EditorState,
    /// When set, the editor widget scrolls the caret into view on the next frame.
    pub scroll_to_cursor: bool,
    /// True while consecutive typed text is collected into one open undo group.
    typing: bool,
    /// Syntax highlighting cache.
    pub highlight: HighlightCache,
    /// Name of the syntax used for highlighting (`None` until assets are loaded).
    pub syntax: Option<String>,
    /// Markdown prose ranges (char ranges of the whole document) for the given buffer revision.
    pub spell_md: Option<(u64, Vec<Range<usize>>)>,
    /// Buffer char ranges of the misspelled words painted in the last frame.
    pub spell_marks: HashSet<Range<usize>>,
    /// Rendering caches owned by the editor widget.
    pub(crate) view: ViewState,
}

impl Tab {
    /// Creates a tab for `buffer` with the caret at the start and a fresh unique id.
    pub fn new(buffer: Buffer) -> Tab {
        Tab {
            id: NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed),
            buffer,
            editor: EditorState::default(),
            scroll_to_cursor: true,
            typing: false,
            highlight: HighlightCache::new(),
            syntax: None,
            spell_md: None,
            spell_marks: HashSet::new(),
            view: ViewState::default(),
        }
    }

    /// Applies an editing command, grouping consecutive `InsertText` commands into one undo
    /// step. Any other command closes the typing group first. Returns clipboard text for
    /// `Cut` and `Copy`. Use this instead of `editing::apply_command` for tab edits.
    pub fn apply(&mut self, cmd: EditCommand, settings: &EditSettings) -> Option<String> {
        if matches!(cmd, EditCommand::InsertText(_)) {
            if !self.typing {
                self.buffer.begin_group();
                self.typing = true;
            }
        } else {
            self.end_typing();
        }
        self.scroll_to_cursor = true;
        editing::apply_command(&mut self.editor, &mut self.buffer, cmd, settings)
    }

    /// Gives the editor widget keyboard focus the next time this tab is shown
    /// (after switching tabs or closing a dialog).
    pub fn focus_editor(&mut self) {
        self.view.request_focus();
    }

    /// Closes the open typing undo group, if any. Call before editing `buffer` directly,
    /// saving it, or running undo/redo outside of `apply`.
    pub fn end_typing(&mut self) {
        if !self.typing {
            return;
        }
        self.buffer.end_group();
        self.typing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: EditSettings = EditSettings {
        tab_width: 4,
        insert_spaces: true,
    };

    #[test]
    fn ids_are_unique() {
        let a = Tab::new(Buffer::new());
        let b = Tab::new(Buffer::new());
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn typed_text_is_one_undo_step() {
        let mut tab = Tab::new(Buffer::from_text(""));
        tab.apply(EditCommand::InsertText("a".into()), &SET);
        tab.apply(EditCommand::InsertText("b".into()), &SET);
        assert!(tab.buffer.is_modified());
        tab.apply(EditCommand::Newline, &SET);
        tab.apply(EditCommand::InsertText("c".into()), &SET);
        tab.apply(EditCommand::Undo, &SET);
        assert_eq!(tab.buffer.text(), "ab\n");
        tab.apply(EditCommand::Undo, &SET);
        assert_eq!(tab.buffer.text(), "ab");
        tab.apply(EditCommand::Undo, &SET);
        assert_eq!(tab.buffer.text(), "");
    }

    #[test]
    fn end_typing_closes_group() {
        let mut tab = Tab::new(Buffer::from_text(""));
        tab.apply(EditCommand::InsertText("a".into()), &SET);
        tab.end_typing();
        tab.apply(EditCommand::InsertText("b".into()), &SET);
        tab.end_typing();
        assert_eq!(tab.buffer.undo(), Some(1));
        assert_eq!(tab.buffer.text(), "a");
    }
}
