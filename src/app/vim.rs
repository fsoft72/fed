//! Vim mode in the application: routing editor keys to the tab's vim state, carrying out the
//! resulting effects (writes, quits, `:e`, `:{n}`) and drawing the command bar.

use std::path::{Path, PathBuf};

use eframe::egui;

use super::{FedApp, SaveOutcome, Severity, close_requires_confirmation, same_file, tab_path};
use crate::editing::EditSettings;
use crate::fonts;
use crate::tab::Tab;
use crate::vim::{Mode, VimEffect, VimKey, VimState, keys};

/// Vim error for `:w {file}` on an existing file.
const VIM_FILE_EXISTS: &str = "E13: File exists (add ! to override)";
/// Vim error for `:q` on a buffer with unsaved changes.
const VIM_NO_WRITE: &str = "No write since last change (add ! to override)";
/// The vim command bar font is this much larger than the editor font.
const VIM_BAR_FONT_EXTRA: f32 = 2.0;

/// Returns the vim part of the status bar: the mode (`-- NORMAL --`, `-- INSERT --`,
/// `-- VISUAL --`, `-- VISUAL LINE --`, nothing on the command line) and the pending keys.
pub fn vim_status_text(vim: &VimState) -> String {
    let mode = match vim.mode() {
        Mode::Normal => "-- NORMAL --",
        Mode::Insert => "-- INSERT --",
        Mode::Visual => "-- VISUAL --",
        Mode::VisualLine => "-- VISUAL LINE --",
        Mode::CommandLine => "",
    };
    format!("{mode}  {}", vim.pending()).trim().to_string()
}

/// Resolves a path typed in a vim command: relative paths are taken from the directory of
/// `current` (the active file), else from the process working directory.
fn resolve_vim_path(path: PathBuf, current: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match current.and_then(Path::parent) {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(path),
        _ => path,
    }
}

/// Command bar text for a vim write of `path`: `"path" written`, the error, or nothing when
/// the Save As dialog was cancelled.
fn vim_save_message(outcome: &SaveOutcome, path: &str) -> Option<VimMessage> {
    match outcome {
        SaveOutcome::Saved => Some(VimMessage {
            text: format!("\"{path}\" written"),
            error: false,
        }),
        SaveOutcome::Failed(e) => Some(VimMessage {
            text: e.clone(),
            error: true,
        }),
        SaveOutcome::Cancelled => None,
    }
}

/// True when `:w {target}` must refuse without `!`: the file exists and is not the tab's own
/// file (`own`).
fn write_copy_refused(target: &Path, own: Option<&Path>) -> bool {
    target.exists() && !own.is_some_and(|own| same_file(own, target))
}

/// True for effects after which the rest of the frame's vim keys and effects are dropped,
/// because they may close or switch the active tab.
fn ends_vim_frame(effect: &VimEffect) -> bool {
    matches!(
        effect,
        VimEffect::Quit { .. } | VimEffect::WriteQuit | VimEffect::Edit(_)
    )
}

/// Text shown in the vim command bar until the next key.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct VimMessage {
    /// Message text.
    text: String,
    /// Drawn in the error color.
    error: bool,
}

/// Hands one editor keyboard event of `tab` to its vim state and collects the effects in
/// `effects`. Once an effect that may close or switch tabs was produced (`frame_ended`), the
/// rest of the frame's keys are swallowed. Always consumes the event.
pub(super) fn route_event(
    ev: &egui::Event,
    tab: &mut Tab,
    edit: &EditSettings,
    effects: &mut Vec<VimEffect>,
    frame_ended: &mut bool,
) -> bool {
    if *frame_ended {
        return true;
    }
    let keys: Vec<VimKey> = match ev {
        // Pasting types the text in Insert mode only (p/P use the register).
        // One key for the whole text: a big paste is a single edit, not one per char.
        egui::Event::Paste(text) if tab.vim.mode() == Mode::Insert && !text.is_empty() => {
            vec![VimKey::Text(text.clone())]
        }
        _ => keys::egui_event_to_vim_keys(ev),
    };
    for key in keys {
        let fx = tab
            .vim
            .handle_key(key, &mut tab.buffer, &mut tab.editor, edit);
        *frame_ended = fx.iter().any(ends_vim_frame);
        effects.extend(fx);
        if *frame_ended {
            break;
        }
    }
    true
}

impl FedApp {
    /// Resets every tab's vim state when vim mode was switched on or off in the settings.
    pub(super) fn sync_vim_mode(&mut self) {
        if self.vim_applied == self.settings.vim_mode {
            return;
        }
        self.vim_applied = self.settings.vim_mode;
        self.vim_message = None;
        self.tabs.iter_mut().for_each(Tab::reset_vim);
    }

    /// Shows `text` in the vim command bar, in the error color when `error`.
    pub(super) fn set_vim_message(&mut self, text: String, error: bool) {
        self.vim_message = Some(VimMessage { text, error });
    }

    /// Carries out the effects of the vim keys handled this frame on the active tab.
    pub(super) fn apply_vim_effects(&mut self, ctx: &egui::Context, effects: Vec<VimEffect>) {
        for effect in effects {
            let last = ends_vim_frame(&effect);
            match effect {
                VimEffect::SetClipboard(text) => ctx.copy_text(text),
                VimEffect::Message(text) => self.set_vim_message(text, false),
                VimEffect::Error(text) => self.set_vim_message(text, true),
                VimEffect::Write(None) => {
                    self.vim_write();
                }
                VimEffect::Write(Some(path)) => self.vim_write_copy(path, false),
                VimEffect::ForceWrite(path) => self.vim_write_copy(path, true),
                VimEffect::Quit { force } => self.vim_quit(ctx, force),
                VimEffect::WriteQuit => {
                    if self.vim_write() {
                        self.remove_tab(ctx, self.active);
                    }
                }
                VimEffect::Edit(path) => self.vim_edit(path),
                VimEffect::GotoLine(line) => self.vim_goto_line(line),
            }
            if last {
                break;
            }
        }
    }

    /// `:w`: saves the active tab (Save As dialog when untitled) and reports the outcome in
    /// the command bar. Returns true when the file was written.
    pub(super) fn vim_write(&mut self) -> bool {
        let outcome = self.save_tab_outcome(self.active);
        let saved = self.report_save(self.active, &outcome);
        let path = tab_path(&self.tabs[self.active]);
        self.vim_message = vim_save_message(&outcome, &path);
        saved
    }

    /// `:w {file}`: writes a copy of the active tab, which stays bound to its own path. An
    /// existing other file is only overwritten with `force` (`:w!`); the tab's own file is a
    /// plain `:w`.
    pub(super) fn vim_write_copy(&mut self, path: PathBuf, force: bool) {
        let tab = &mut self.tabs[self.active];
        let path = resolve_vim_path(path, tab.buffer.path());
        if tab.buffer.path().is_some_and(|own| same_file(own, &path)) {
            self.vim_write();
            return;
        }
        if !force && write_copy_refused(&path, tab.buffer.path()) {
            self.set_vim_message(VIM_FILE_EXISTS.to_string(), true);
            return;
        }
        tab.end_typing();
        match tab.buffer.write_copy(&path) {
            Ok(()) => self.set_vim_message(format!("\"{}\" written", path.display()), false),
            Err(e) => self.set_vim_message(e.to_string(), true),
        }
    }

    /// `:q` / `:q!`: closes the active tab; without `force` it refuses unsaved changes.
    pub(super) fn vim_quit(&mut self, ctx: &egui::Context, force: bool) {
        let tab = &mut self.tabs[self.active];
        tab.end_typing();
        if !force && close_requires_confirmation(tab) {
            self.set_vim_message(VIM_NO_WRITE.to_string(), true);
            return;
        }
        self.remove_tab(ctx, self.active);
    }

    /// `:e {file}`: opens the file (relative to the active file's directory) in a tab.
    pub(super) fn vim_edit(&mut self, path: PathBuf) {
        let path = resolve_vim_path(path, self.tabs[self.active].buffer.path());
        if let Err(e) = self.try_open(path) {
            self.set_vim_message(e.clone(), true);
            self.set_status(e, Severity::Error);
        }
    }

    /// Moves the caret of the active tab to the first non-blank of 1-based `line`.
    pub(super) fn vim_goto_line(&mut self, line: usize) {
        let tab = &mut self.tabs[self.active];
        let rope = tab.buffer.rope();
        let line = line.clamp(1, rope.len_lines()) - 1;
        let pos = crate::text_nav::first_non_blank(rope, rope.line_to_char(line));
        tab.editor.selection = crate::editing::Selection::caret(pos);
        tab.scroll_to_cursor = true;
    }

    /// Draws the vim command bar above the status bar: the command line with a block cursor
    /// while typing a command, otherwise the last message.
    pub(super) fn vim_command_bar(&mut self, ui: &mut egui::Ui) {
        if !self.settings.vim_mode {
            return;
        }
        let vim = &self.tabs[self.active].vim;
        let fill = ui.visuals().extreme_bg_color;
        let strong = ui.visuals().strong_text_color();
        let error = ui.visuals().error_fg_color;
        let format = |color: egui::Color32| egui::TextFormat {
            font_id: fonts::editor_font_id(self.settings.font_size + VIM_BAR_FONT_EXTRA),
            color,
            ..Default::default()
        };
        let mut job = egui::text::LayoutJob::default();
        match (vim.mode(), &self.vim_message) {
            (Mode::CommandLine, _) => {
                job.append(vim.cmdline(), 0.0, format(strong));
                let cursor = egui::TextFormat {
                    background: strong,
                    ..format(fill)
                };
                job.append(" ", 0.0, cursor);
            }
            (_, Some(msg)) => {
                let color = if msg.error { error } else { strong };
                job.append(&msg.text, 0.0, format(color));
            }
            // A blank keeps the bar one line high.
            _ => job.append(" ", 0.0, format(strong)),
        }
        let frame = egui::Frame::side_top_panel(ui.style()).fill(fill);
        egui::Panel::bottom("fed_vim_cmdline")
            .frame(frame)
            .show(ui, |ui| {
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Truncate));
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    const SET: EditSettings = EditSettings {
        tab_width: 4,
        insert_spaces: true,
    };

    #[test]
    fn insert_mode_paste_is_routed_as_one_text_key() {
        let mut tab = Tab::new(Buffer::from_text("\n"));
        let (mut fx, mut ended) = (Vec::new(), false);
        let paste = egui::Event::Paste("one\ntwo".into());
        route_event(&paste, &mut tab, &SET, &mut fx, &mut ended);
        assert_eq!(tab.buffer.text(), "\n", "Normal mode ignores the paste");
        route_event(
            &egui::Event::Text("i".into()),
            &mut tab,
            &SET,
            &mut fx,
            &mut ended,
        );
        let before = tab.buffer.revision();
        route_event(&paste, &mut tab, &SET, &mut fx, &mut ended);
        assert_eq!(tab.buffer.text(), "one\ntwo\n");
        assert_eq!(tab.buffer.revision(), before + 1);
    }

    #[test]
    fn vim_status_shows_mode_and_pending_keys() {
        let mut tab = Tab::new(Buffer::from_text("abc\n"));
        assert_eq!(vim_status_text(&tab.vim), "-- NORMAL --");
        let feed = |tab: &mut Tab, key: VimKey| {
            tab.vim
                .handle_key(key, &mut tab.buffer, &mut tab.editor, &SET);
        };
        feed(&mut tab, VimKey::Char('2'));
        feed(&mut tab, VimKey::Char('d'));
        assert_eq!(vim_status_text(&tab.vim), "-- NORMAL --  2d");
        feed(&mut tab, VimKey::Esc);
        feed(&mut tab, VimKey::Char('V'));
        assert_eq!(vim_status_text(&tab.vim), "-- VISUAL LINE --");
        feed(&mut tab, VimKey::Char('v'));
        assert_eq!(vim_status_text(&tab.vim), "-- VISUAL --");
        feed(&mut tab, VimKey::Esc);
        feed(&mut tab, VimKey::Char('i'));
        assert_eq!(vim_status_text(&tab.vim), "-- INSERT --");
    }

    #[test]
    fn vim_paths_are_relative_to_the_current_file() {
        let current = Path::new("/work/src/main.rs");
        assert_eq!(
            resolve_vim_path(PathBuf::from("other.txt"), Some(current)),
            PathBuf::from("/work/src/other.txt")
        );
        assert_eq!(
            resolve_vim_path(PathBuf::from("/abs/x"), Some(current)),
            PathBuf::from("/abs/x")
        );
        assert_eq!(
            resolve_vim_path(PathBuf::from("a.txt"), None),
            PathBuf::from("a.txt")
        );
        assert_eq!(
            resolve_vim_path(PathBuf::from("a.txt"), Some(Path::new("b.txt"))),
            PathBuf::from("a.txt")
        );
    }

    #[test]
    fn write_copy_refuses_existing_other_files() {
        let dir = std::env::temp_dir().join(format!("fed-app-{}-wcopy", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let own = dir.join("own.txt");
        let other = dir.join("other.txt");
        std::fs::write(&own, "a").unwrap();
        std::fs::write(&other, "b").unwrap();
        assert!(write_copy_refused(&other, Some(&own)));
        assert!(write_copy_refused(&other, None));
        assert!(
            !write_copy_refused(&own, Some(&own)),
            "own file is a plain write"
        );
        assert!(!write_copy_refused(&dir.join("new.txt"), Some(&own)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vim_save_message_reflects_the_outcome() {
        assert_eq!(
            vim_save_message(&SaveOutcome::Saved, "a.txt"),
            Some(VimMessage {
                text: "\"a.txt\" written".into(),
                error: false
            })
        );
        assert_eq!(
            vim_save_message(&SaveOutcome::Failed("denied".into()), "a.txt"),
            Some(VimMessage {
                text: "denied".into(),
                error: true
            })
        );
        assert_eq!(vim_save_message(&SaveOutcome::Cancelled, "a.txt"), None);
    }

    #[test]
    fn tab_changing_effects_end_the_vim_frame() {
        assert!(ends_vim_frame(&VimEffect::Quit { force: false }));
        assert!(ends_vim_frame(&VimEffect::WriteQuit));
        assert!(ends_vim_frame(&VimEffect::Edit(PathBuf::from("x"))));
        assert!(!ends_vim_frame(&VimEffect::Write(None)));
        assert!(!ends_vim_frame(&VimEffect::Message("m".into())));
    }

    #[test]
    fn quit_refusal_depends_on_modified_state() {
        let mut tab = Tab::new(Buffer::from_text("a\n"));
        tab.vim
            .handle_key(VimKey::Char('x'), &mut tab.buffer, &mut tab.editor, &SET);
        assert!(close_requires_confirmation(&tab), "`:q` must refuse");
        tab.vim
            .handle_key(VimKey::Char('u'), &mut tab.buffer, &mut tab.editor, &SET);
        assert!(!close_requires_confirmation(&tab), "`:q` may close");
    }

    #[test]
    fn routing_stops_after_a_tab_changing_effect() {
        let mut tab = Tab::new(Buffer::from_text("abc\n"));
        let mut effects = Vec::new();
        let mut ended = false;
        let typed = |text: &str| egui::Event::Text(text.to_string());
        for ev in [
            typed(":"),
            typed("q!"),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            typed("x"),
        ] {
            assert!(route_event(&ev, &mut tab, &SET, &mut effects, &mut ended));
        }
        assert!(ended);
        assert!(matches!(
            effects.last(),
            Some(VimEffect::Quit { force: true })
        ));
        assert_eq!(tab.buffer.text(), "abc\n", "keys after :q! are dropped");
    }
}
