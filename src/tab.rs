//! An open document: buffer, editor state and per-tab view caches.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::buffer::Buffer;
use crate::editing::{self, EditCommand, EditSettings, EditorState};
use crate::highlight::{self, HighlightAssets, HighlightCache};
use crate::markdown::Preview;
use crate::spell::SpellCache;
use crate::view::ViewState;
use crate::vim::VimState;

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
    /// Buffer path `syntax` was computed for, to recompute it only when the path changes.
    syntax_path: Option<PathBuf>,
    /// Spell check state: Markdown prose ranges, painted marks and per-line results.
    pub spell: SpellCache,
    /// True while the Markdown preview panel is shown for this tab.
    pub preview_open: bool,
    /// Markdown preview state, created when the preview is first opened.
    pub preview: Option<Preview>,
    /// Vim emulation state (used only while vim mode is on).
    pub vim: VimState,
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
            syntax_path: None,
            spell: SpellCache::default(),
            preview_open: false,
            preview: None,
            vim: VimState::new(),
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

    /// Picks the highlighting syntax from the buffer path. Cheap when nothing changed: the
    /// lookup only runs the first time (assets just loaded) and after the path changed.
    pub fn refresh_syntax(&mut self, assets: &HighlightAssets) {
        let path = self.buffer.path();
        if self.syntax.is_some() && self.syntax_path.as_deref() == path {
            return;
        }
        self.syntax = Some(highlight::syntax_for_path(assets, path));
        self.syntax_path = path.map(PathBuf::from);
    }

    /// Gives the editor widget keyboard focus the next time this tab is shown
    /// (after switching tabs or closing a dialog).
    pub fn focus_editor(&mut self) {
        self.view.request_focus();
    }

    /// Closes the open typing undo group and the undo group of a vim Insert session, if
    /// any. Call before editing `buffer` directly, saving it, switching away from the tab,
    /// or running undo/redo outside of `apply`. Vim stays in Insert mode.
    pub fn end_typing(&mut self) {
        self.vim.end_insert_group(&mut self.buffer);
        if !self.typing {
            return;
        }
        self.buffer.end_group();
        self.typing = false;
    }

    /// Replaces the vim state with a fresh one in Normal mode (vim mode switched on or off),
    /// closing any open undo group first and collapsing the selection to the caret.
    pub fn reset_vim(&mut self) {
        self.end_typing();
        self.vim = VimState::new();
        self.editor.selection = crate::editing::Selection::caret(self.editor.selection.head);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vim::{Mode, VimKey};

    const SET: EditSettings = EditSettings {
        tab_width: 4,
        insert_spaces: true,
    };

    #[test]
    fn syntax_is_recomputed_only_when_the_path_changes() {
        let assets = HighlightAssets::load();
        let mut tab = Tab::new(Buffer::new());
        tab.refresh_syntax(&assets);
        assert_eq!(tab.syntax.as_deref(), Some("Plain Text"));
        tab.buffer.set_path(PathBuf::from("main.rs"));
        tab.refresh_syntax(&assets);
        assert_eq!(tab.syntax.as_deref(), Some("Rust"));
        // Same path: the cached name is kept (a sentinel proves no lookup ran).
        tab.syntax = Some("sentinel".to_string());
        tab.refresh_syntax(&assets);
        assert_eq!(tab.syntax.as_deref(), Some("sentinel"));
        tab.buffer.set_path(PathBuf::from("notes.md"));
        tab.refresh_syntax(&assets);
        assert_eq!(tab.syntax.as_deref(), Some("Markdown"));
    }

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

    #[test]
    fn end_typing_closes_vim_insert_group() {
        let mut tab = Tab::new(Buffer::from_text(""));
        for key in [VimKey::Char('i'), VimKey::Char('a')] {
            tab.vim
                .handle_key(key, &mut tab.buffer, &mut tab.editor, &SET);
        }
        tab.end_typing();
        assert!(
            tab.buffer.undo().is_some(),
            "the insert session is a committed step"
        );
        assert_eq!(tab.buffer.text(), "");
        assert_eq!(tab.vim.mode(), Mode::Insert);
        tab.reset_vim();
        assert_eq!(tab.vim.mode(), Mode::Normal);
    }
}
