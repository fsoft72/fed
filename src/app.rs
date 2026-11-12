//! Application shell: tabs, file dialogs, shortcuts, status bar and session handling.

mod dialogs;
mod menu;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::buffer::{Buffer, LineEnding};
use crate::config::{self, ConfigPaths, Session, Settings, UiTheme};
use crate::editing::{EditCommand, EditSettings};
use crate::find_bar::{BarMode, FindBar, MatchDecorator};
use crate::fonts::{self, FontCatalog, FontLoad, LoadedFont};
use crate::highlight::{AssetsHandle, HighlightAssets, HighlightDecorator};
use crate::keymap::{Action, KeyChord, Keymap};
use crate::markdown::{self, Preview};
use crate::settings_ui::{SaveDebounce, SettingsEvent, SettingsWindow};
use crate::spell::{SpellDecorator, SpellMode, SpellService, SpellStatus};
use crate::tab::Tab;
use crate::view::{self, KeyHandler, ViewOptions};
use crate::vim::{Mode, VimEffect, VimKey, VimState};
use dialogs::Pending;
use menu::MenuChoice;

/// How long a status bar message stays visible.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Status bar notice for large files (highlighting, spell check and word wrap disabled).
const LARGE_FILE_NOTICE: &str = "large file: highlighting, spell check and wrap off";
/// Status bar text while the dictionaries are downloading.
const SPELL_DOWNLOADING_NOTICE: &str = "downloading dictionaries...";
/// Name shown for a tab without a file path.
const UNTITLED: &str = "untitled";
/// Status message shown when the preview is requested on a non-Markdown tab.
const PREVIEW_UNAVAILABLE: &str = "preview is only available for Markdown";
/// Suffix of a tab label when the buffer has unsaved changes. Drawn with the monospace
/// font because the default proportional fonts have no glyph for it.
const TAB_HEIGHT: f32 = 32.0;
/// Horizontal padding inside a tab.
const TAB_PADDING: f32 = 14.0;
/// Size of the close button hit area of a tab.
const TAB_CLOSE_SIZE: f32 = 20.0;
/// Accent line drawn on top of the selected tab.
const TAB_ACCENT: egui::Color32 = egui::Color32::from_rgb(0, 122, 204);
/// Thickness of the accent line.
/// Which configurable font a load or apply refers to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FontSlot {
    /// The editor font.
    Editor,
    /// The Markdown preview font.
    Preview,
}

/// Width of the "Preview" button in the tab bar.
const PREVIEW_BUTTON_WIDTH: f32 = 110.0;
/// Corner radius of the "Preview" button.
const PREVIEW_BUTTON_RADIUS: f32 = 4.0;
const TAB_ACCENT_HEIGHT: f32 = 2.0;
/// Suffix of the status text when the buffer has unsaved changes.
const MODIFIED_STATUS_SUFFIX: &str = "  [+]";
/// Fixed, non-configurable chord aliases, used only when the keymap has no binding for the
/// pressed chord.
const SHORTCUT_ALIASES: &[(&str, Action)] = &[
    ("Cmd+Shift+Z", Action::Redo),
    ("Ctrl+Tab", Action::NextTab),
    ("Ctrl+Shift+Tab", Action::PrevTab),
];
/// Vim error for `:w {file}` on an existing file.
const VIM_FILE_EXISTS: &str = "E13: File exists (add ! to override)";
/// Vim error for `:q` on a buffer with unsaved changes.
const VIM_NO_WRITE: &str = "No write since last change (add ! to override)";
/// The vim command bar font is this much larger than the editor font.
const VIM_BAR_FONT_EXTRA: f32 = 2.0;

/// Returns true when closing `tab` would lose unsaved changes.
pub fn close_requires_confirmation(tab: &Tab) -> bool {
    tab.buffer.is_modified()
}

/// Returns the indices of the tabs that would lose unsaved changes when closed.
pub fn tabs_needing_confirmation(tabs: &[Tab]) -> Vec<usize> {
    tabs.iter()
        .enumerate()
        .filter(|(_, tab)| close_requires_confirmation(tab))
        .map(|(i, _)| i)
        .collect()
}

/// Returns the status bar text for `tab`: path (as opened, or "untitled"), 1-based
/// `line:col` of the caret, line ending, and `"  [+]"` when modified.
pub fn status_text(tab: &Tab) -> String {
    let rope = tab.buffer.rope();
    let head = tab.editor.selection.head.min(rope.len_chars());
    let line = rope.char_to_line(head);
    let col = head - rope.line_to_char(line);
    let ending = match tab.buffer.line_ending() {
        LineEnding::Lf => "LF",
        LineEnding::Crlf => "CRLF",
    };
    let mut text = format!("{}  {}:{}  {ending}", tab_path(tab), line + 1, col + 1);
    if tab.buffer.is_modified() {
        text.push_str(MODIFIED_STATUS_SUFFIX);
    }
    text
}

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

/// True when the app shortcut `action` bound to a chord with `mods` may fire while vim mode is
/// on: only chords with Ctrl/Cmd, and never Undo/Redo/SelectAll (vim owns `u`, `Ctrl+R`).
fn shortcut_allowed_in_vim(action: Action, mods: egui::Modifiers) -> bool {
    (mods.ctrl || mods.command) && !needs_editor_focus(action)
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

/// Path of `tab` as opened, or "untitled".
fn tab_path(tab: &Tab) -> String {
    tab.buffer
        .path()
        .map_or_else(|| UNTITLED.to_string(), |p| p.display().to_string())
}

/// True for an untitled tab that is empty and unmodified (replaced when opening a file).
fn is_blank(tab: &Tab) -> bool {
    tab.buffer.path().is_none() && !tab.buffer.is_modified() && tab.buffer.rope().len_chars() == 0
}

/// True when `a` and `b` name the same file (same text, or same canonical path).
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Returns the action fired by pressing `key` with `mods`: the keymap binding of that exact
/// chord, or the action of a fixed `SHORTCUT_ALIASES` entry.
fn shortcut_action(keymap: &Keymap, key: egui::Key, mods: egui::Modifiers) -> Option<Action> {
    let chord = KeyChord::from_egui(key, mods);
    if let Some(action) = keymap.lookup(&chord) {
        return Some(action);
    }
    SHORTCUT_ALIASES.iter().find_map(|(text, action)| {
        let alias = KeyChord::parse(text).expect("shortcut aliases are valid chords");
        (alias == chord).then_some(*action)
    })
}

/// True for actions that edit the text and must leave text fields (find bar) alone.
fn needs_editor_focus(action: Action) -> bool {
    matches!(action, Action::Undo | Action::Redo | Action::SelectAll)
}

/// Shows the native save dialog, starting from the buffer's current location.
fn save_dialog(buffer: &Buffer) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_file_name(buffer.display_name());
    if let Some(dir) = buffer.path().and_then(Path::parent)
        && !dir.as_os_str().is_empty()
    {
        dialog = dialog.set_directory(dir);
    }
    dialog.save_file()
}

/// How a status bar message is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Severity {
    /// Normal text color.
    Info,
    /// Warning color (settings problems).
    Warning,
    /// Error color (failed file operations).
    Error,
}

/// A transient message on the right side of the status bar.
struct StatusMessage {
    /// Message text.
    text: String,
    /// Color class.
    severity: Severity,
    /// When the message appeared (it disappears after `STATUS_TIMEOUT`).
    shown_at: Instant,
}

/// Result of a save attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SaveOutcome {
    /// The file was written.
    Saved,
    /// The user cancelled the Save As dialog.
    Cancelled,
    /// Writing failed with this message.
    Failed(String),
}

impl SaveOutcome {
    /// Converts a buffer save result.
    fn from_result(result: Result<(), crate::buffer::BufferError>) -> SaveOutcome {
        match result {
            Ok(()) => SaveOutcome::Saved,
            Err(e) => SaveOutcome::Failed(e.to_string()),
        }
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
struct VimMessage {
    /// Message text.
    text: String,
    /// Drawn in the error color.
    error: bool,
}

/// Something the user did in the tab bar.
enum TabBarAction {
    /// Switch to the tab at this index.
    Activate(usize),
    /// Close the tab at this index.
    Close(usize),
    /// Toggle the Markdown preview of the active tab.
    TogglePreview,
    /// An entry of the application menu was picked.
    Menu(MenuChoice),
}

/// The spelling suggestions popup opened by a right click on a misspelled word.
struct SpellMenu {
    /// Id of the tab the word belongs to.
    tab_id: u64,
    /// Screen position of the popup.
    pos: egui::Pos2,
    /// Char range of the word in the buffer.
    range: std::ops::Range<usize>,
    /// The misspelled word.
    word: String,
    /// Suggested replacements.
    suggestions: Vec<String>,
    /// True until the popup has been drawn once (the opening click must not close it).
    fresh: bool,
}

/// Top-level editor application state.
pub struct FedApp {
    /// Open documents, one per tab; never empty.
    tabs: Vec<Tab>,
    /// Index of the tab shown in the editor area.
    active: usize,
    /// Editor settings.
    settings: Settings,
    /// Keyboard shortcuts.
    keymap: Keymap,
    /// Config file locations; `None` runs without persistence.
    paths: Option<ConfigPaths>,
    /// Transient status bar message.
    status: Option<StatusMessage>,
    /// Confirmation dialog currently shown.
    pending: Option<Pending>,
    /// UI theme last applied to the context.
    applied_theme: Option<UiTheme>,
    /// Find/replace and go-to-line bars.
    find_bar: FindBar,
    /// Syntax and theme assets loading in the background.
    assets: AssetsHandle,
    /// The loaded assets, once available.
    loaded_assets: Option<Arc<HighlightAssets>>,
    /// Background spell checker; started when spell checking is on.
    spell: Option<SpellService>,
    /// Suggestions popup, when open.
    spell_menu: Option<SpellMenu>,
    /// Set once the user agreed to close the window, so the close is not intercepted again.
    allow_close: bool,
    /// True while the About dialog is shown.
    about_open: bool,
    /// Vim mode state the tabs were last set up for (to react to settings changes).
    vim_applied: bool,
    /// Last vim message or error, shown in the command bar.
    vim_message: Option<VimMessage>,
    /// The settings window.
    settings_window: SettingsWindow,
    /// System font families, scanned the first time the settings window opens.
    font_catalog: Option<FontCatalog>,
    /// Startup load of the configured editor font, until it finishes.
    font_load: Option<FontLoad>,
    /// Startup load of the configured preview font, until it finishes.
    preview_font_load: Option<FontLoad>,
    /// The loaded editor font, if any.
    editor_font: Option<LoadedFont>,
    /// The loaded preview font, if any.
    preview_font: Option<LoadedFont>,
    /// Deferred save of live (dragged) settings changes.
    settings_save: SaveDebounce,
    /// Process start time, while the first frame is still to be traced (`FED_TRACE_STARTUP`).
    startup_trace: Option<Instant>,
    /// Sends errors from the threads that open clicked links.
    link_error_tx: Sender<String>,
    /// Receives the errors of `link_error_tx`, shown in the status bar.
    link_error_rx: Receiver<String>,
}

impl FedApp {
    /// Creates the application; `files` are the paths passed on the command line. Loads the
    /// settings, then opens one tab per file (or the previous session's files when `files` is
    /// empty and session restore is on), falling back to one empty tab. With `startup_trace`
    /// the time from that instant to the end of the first frame is printed to stderr.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        files: Vec<PathBuf>,
        startup_trace: Option<Instant>,
    ) -> FedApp {
        let paths = ConfigPaths::detect();
        let (settings, mut problems) = match &paths {
            Some(p) => config::load_settings(p),
            None => (
                Settings::default(),
                vec!["no config directory: settings and session are not saved".to_string()],
            ),
        };
        let (keymap, key_warnings) = Keymap::from_overrides(&settings.keybindings);
        problems.extend(key_warnings);
        let (link_error_tx, link_error_rx) = mpsc::channel();
        let mut app = FedApp {
            tabs: Vec::new(),
            active: 0,
            settings,
            keymap,
            paths,
            status: None,
            pending: None,
            applied_theme: None,
            find_bar: FindBar::new(),
            assets: AssetsHandle::spawn(cc.egui_ctx.clone()),
            loaded_assets: None,
            spell: None,
            spell_menu: None,
            allow_close: false,
            about_open: false,
            vim_applied: false,
            vim_message: None,
            settings_window: SettingsWindow::new(),
            font_catalog: None,
            font_load: None,
            preview_font_load: None,
            editor_font: None,
            preview_font: None,
            settings_save: SaveDebounce::default(),
            startup_trace,
            link_error_tx,
            link_error_rx,
        };
        app.vim_applied = app.settings.vim_mode;
        fonts::apply_fonts(&cc.egui_ctx, None, None);
        if let Some(family) = &app.settings.font_family {
            app.font_load = Some(FontLoad::spawn(cc.egui_ctx.clone(), family.clone()));
        }
        if let Some(family) = &app.settings.preview_font_family {
            app.preview_font_load = Some(FontLoad::spawn(cc.egui_ctx.clone(), family.clone()));
        }
        egui_extras::install_image_loaders(&cc.egui_ctx);
        app.apply_theme(&cc.egui_ctx);
        app.ensure_spell(&cc.egui_ctx);
        if !problems.is_empty() {
            app.set_status(problems.join("; "), Severity::Warning);
        }
        let files = if files.is_empty() && app.settings.restore_session {
            app.session_files()
        } else {
            files
        };
        let errors: Vec<String> = files
            .into_iter()
            .filter_map(|path| app.try_open(path).err())
            .collect();
        if app.tabs.is_empty() {
            app.tabs.push(Tab::new(Buffer::new()));
        }
        app.active = 0;
        if !errors.is_empty() {
            app.set_status(errors.join("; "), Severity::Error);
        }
        app
    }

    /// Opens `path` in a tab. If it is already open that tab is activated; if the active tab
    /// is an empty unmodified untitled tab it is replaced. Errors go to the status bar.
    pub fn open_path(&mut self, path: PathBuf) {
        if let Err(e) = self.try_open(path) {
            self.set_status(e, Severity::Error);
        }
    }

    /// Writes the current settings (including the keymap) to the config directory, dropping
    /// any deferred save.
    pub fn save_settings(&mut self) {
        self.settings_save.cancel();
        self.settings.keybindings = self.keymap.to_overrides();
        let Some(paths) = &self.paths else { return };
        if let Err(e) = config::save_settings(paths, &self.settings) {
            self.set_status(format!("cannot save settings: {e}"), Severity::Error);
        }
    }

    /// Executes `action` on the application or the active tab.
    pub fn dispatch(&mut self, ctx: &egui::Context, action: Action) {
        let count = self.tabs.len();
        match action {
            Action::NewTab => {
                self.leave_active();
                self.tabs.push(Tab::new(Buffer::new()));
                self.active = self.tabs.len() - 1;
            }
            Action::OpenFile => self.open_dialog(),
            Action::Save => {
                self.save_tab(self.active);
            }
            Action::SaveAs => {
                self.save_tab_as(self.active);
            }
            Action::CloseTab => self.request_close_tab(ctx, self.active),
            Action::NextTab => self.activate((self.active + 1) % count),
            Action::PrevTab => self.activate((self.active + count - 1) % count),
            Action::Undo => self.apply_to_active(EditCommand::Undo),
            Action::Redo => self.apply_to_active(EditCommand::Redo),
            Action::SelectAll => self.apply_to_active(EditCommand::SelectAll),
            Action::Find => self.open_bar(BarMode::Find),
            Action::Replace => self.open_bar(BarMode::Replace),
            Action::GoToLine => self.open_bar(BarMode::GoToLine),
            Action::TogglePreview => self.toggle_preview(),
            Action::OpenSettings => self.toggle_settings(ctx),
        }
    }

    /// Shows the find, replace or go-to-line bar for the active tab.
    fn open_bar(&mut self, mode: BarMode) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        self.find_bar.open(mode, tab);
    }

    /// Opens `path` in a tab (see `open_path`), returning a readable error.
    fn try_open(&mut self, path: PathBuf) -> Result<(), String> {
        let open = self
            .tabs
            .iter()
            .position(|t| t.buffer.path().is_some_and(|p| same_file(p, &path)));
        if let Some(idx) = open {
            self.activate(idx);
            return Ok(());
        }
        let tab = Tab::new(Buffer::load(&path).map_err(|e| e.to_string())?);
        self.leave_active();
        match self.tabs.get(self.active) {
            Some(current) if is_blank(current) => self.tabs[self.active] = tab,
            _ => {
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
            }
        }
        Ok(())
    }

    /// Files of the previous session that still exist.
    fn session_files(&self) -> Vec<PathBuf> {
        let Some(paths) = &self.paths else {
            return Vec::new();
        };
        config::load_session(paths)
            .files
            .into_iter()
            .filter(|p| p.exists())
            .collect()
    }

    /// Writes the open file paths as the session, when session restore is on.
    fn save_session(&self) {
        if !self.settings.restore_session {
            return;
        }
        let Some(paths) = &self.paths else { return };
        let files = self
            .tabs
            .iter()
            .filter_map(|t| t.buffer.path())
            .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
            .collect();
        if let Err(e) = config::save_session(paths, &Session { files }) {
            eprintln!("fed: cannot save session to {}: {e}", paths.dir.display());
        }
    }

    /// Shows a message in the status bar for `STATUS_TIMEOUT`.
    fn set_status(&mut self, text: String, severity: Severity) {
        self.status = Some(StatusMessage {
            text,
            severity,
            shown_at: Instant::now(),
        });
    }

    /// Drops the status message once it timed out, or schedules a repaint for that moment.
    fn expire_status(&mut self, ctx: &egui::Context) {
        let Some(msg) = &self.status else { return };
        let elapsed = msg.shown_at.elapsed();
        if elapsed >= STATUS_TIMEOUT {
            self.status = None;
            return;
        }
        ctx.request_repaint_after(STATUS_TIMEOUT - elapsed);
    }

    /// Applies the configured UI theme when it changed since the last frame.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme == Some(self.settings.ui_theme) {
            return;
        }
        ctx.set_theme(match self.settings.ui_theme {
            UiTheme::System => egui::ThemePreference::System,
            UiTheme::Light => egui::ThemePreference::Light,
            UiTheme::Dark => egui::ThemePreference::Dark,
        });
        self.applied_theme = Some(self.settings.ui_theme);
    }

    /// Indentation settings from the current settings.
    fn edit_settings(&self) -> EditSettings {
        EditSettings {
            tab_width: self.settings.tab_width,
            insert_spaces: self.settings.insert_spaces,
        }
    }

    /// Builds the editor widget options from the current settings. Word wrap is off for a
    /// large active file.
    fn view_options(&self) -> ViewOptions {
        let large = self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.buffer.is_large());
        ViewOptions {
            font: fonts::editor_font_id(self.settings.font_size),
            line_numbers: self.settings.line_numbers,
            word_wrap: self.settings.word_wrap && !large,
            edit: self.edit_settings(),
            keys_enabled: self.pending.is_none(),
            vim: self.settings.vim_mode,
        }
    }

    /// Applies an editing command to the active tab.
    fn apply_to_active(&mut self, cmd: EditCommand) {
        let edit = self.edit_settings();
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        tab.apply(cmd, &edit);
    }

    /// Makes the tab at `idx` active and gives it keyboard focus.
    fn activate(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        if idx != self.active {
            self.leave_active();
        }
        self.tabs[idx].focus_editor();
        self.active = idx;
    }

    /// Closes the open undo groups of the active tab before another tab becomes active.
    fn leave_active(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.end_typing();
        }
    }

    /// Shows the native open dialog and opens every picked file.
    fn open_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(dir) = self.tabs[self.active].buffer.path().and_then(Path::parent)
            && !dir.as_os_str().is_empty()
        {
            dialog = dialog.set_directory(dir);
        }
        let Some(files) = dialog.pick_files() else {
            return;
        };
        for path in files {
            self.open_path(path);
        }
    }

    /// Saves the tab at `idx`, asking for a path when it is untitled. Returns true on success.
    fn save_tab(&mut self, idx: usize) -> bool {
        let outcome = self.save_tab_outcome(idx);
        self.report_save(idx, &outcome)
    }

    /// Asks for a path and saves the tab at `idx` there. Returns true on success.
    fn save_tab_as(&mut self, idx: usize) -> bool {
        let outcome = self.save_tab_as_outcome(idx);
        self.report_save(idx, &outcome)
    }

    /// Saves the tab at `idx` (Save As dialog when untitled) without reporting.
    fn save_tab_outcome(&mut self, idx: usize) -> SaveOutcome {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return SaveOutcome::Cancelled;
        };
        tab.end_typing();
        if tab.buffer.path().is_none() {
            return self.save_tab_as_outcome(idx);
        }
        SaveOutcome::from_result(tab.buffer.save())
    }

    /// Asks for a path and saves the tab at `idx` there, without reporting.
    fn save_tab_as_outcome(&mut self, idx: usize) -> SaveOutcome {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return SaveOutcome::Cancelled;
        };
        tab.end_typing();
        let Some(path) = save_dialog(&tab.buffer) else {
            return SaveOutcome::Cancelled;
        };
        SaveOutcome::from_result(tab.buffer.save_as(path))
    }

    /// Shows the outcome of saving the tab at `idx` in the status bar; returns true when the
    /// file was written.
    fn report_save(&mut self, idx: usize, outcome: &SaveOutcome) -> bool {
        match outcome {
            SaveOutcome::Saved => {
                let path = tab_path(&self.tabs[idx]);
                self.set_status(format!("saved {path}"), Severity::Info);
                true
            }
            SaveOutcome::Failed(e) => {
                self.set_status(e.clone(), Severity::Error);
                false
            }
            SaveOutcome::Cancelled => false,
        }
    }

    /// True when the active editor (or nothing) has keyboard focus, so editing shortcuts
    /// must not be stolen from a focused text field.
    fn editor_has_focus(&self, ctx: &egui::Context) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        ctx.memory(|m| m.focused())
            .is_none_or(|id| id == view::editor_id(tab.id))
    }

    /// Consumes this frame's shortcut key presses (each event is matched against the exact
    /// chord) and dispatches their actions. While a settings window text field has focus
    /// only OpenSettings fires.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() {
            return;
        }
        let editor_focused = self.editor_has_focus(ctx);
        let settings_typing = self.settings_window.has_focus(ctx) && ctx.text_edit_focused();
        let vim = self.settings.vim_mode;
        let mut fired = Vec::new();
        ctx.input_mut(|i| {
            i.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                else {
                    return true;
                };
                let Some(action) = shortcut_action(&self.keymap, *key, *modifiers) else {
                    return true;
                };
                if vim && !shortcut_allowed_in_vim(action, *modifiers) {
                    return true;
                }
                if needs_editor_focus(action) && !editor_focused {
                    return true;
                }
                if settings_typing && action != Action::OpenSettings {
                    return true;
                }
                fired.push(action);
                false
            });
        });
        for action in fired {
            self.dispatch(ctx, action);
        }
    }

    /// Toggles the Markdown preview of the active tab, or reports that it is unavailable.
    fn toggle_preview(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if !markdown::is_markdown_path(tab.buffer.path()) {
            self.set_status(PREVIEW_UNAVAILABLE.to_string(), Severity::Info);
            return;
        }
        tab.preview_open = !tab.preview_open;
    }

    /// Draws the right-hand preview panel for the active Markdown tab, if it is open.
    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if !tab.preview_open || !markdown::is_markdown_path(tab.buffer.path()) {
            return;
        }
        let half = ui.available_width() / 2.0;
        let font_size = self.settings.preview_font_size;
        egui::Panel::right("fed_preview")
            .resizable(true)
            .default_size(half)
            .show(ui, |ui| {
                let preview = tab.preview.get_or_insert_with(Preview::new);
                preview.show(ui, &tab.buffer, font_size);
            });
    }

    /// Draws the tab bar and returns what the user clicked.
    fn tab_bar(&self, ui: &mut egui::Ui) -> Option<TabBarAction> {
        let preview_state = self
            .tabs
            .get(self.active)
            .filter(|t| markdown::is_markdown_path(t.buffer.path()))
            .map(|t| t.preview_open);
        let mut action = None;
        egui::Panel::top("fed_tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Reserve room for the Preview and menu buttons on the right; tabs scroll in
                // the rest.
                let spacing = ui.spacing().item_spacing.x;
                let mut reserved = menu::MENU_BUTTON_SIZE + spacing;
                if preview_state.is_some() {
                    reserved += PREVIEW_BUTTON_WIDTH + spacing;
                }
                let tabs_width = (ui.available_width() - reserved).max(0.0);
                egui::ScrollArea::horizontal()
                    .max_width(tabs_width)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            for (i, tab) in self.tabs.iter().enumerate() {
                                if let Some(tab_action) = draw_tab(ui, tab, i == self.active) {
                                    action = Some(match tab_action {
                                        TabClick::Activate => TabBarAction::Activate(i),
                                        TabClick::Close => TabBarAction::Close(i),
                                    });
                                }
                            }
                        });
                    });
                if let Some(open) = preview_state
                    && draw_preview_button(ui, open)
                {
                    action = Some(TabBarAction::TogglePreview);
                }
                if let Some(choice) = menu::hamburger_menu(ui, &self.keymap) {
                    action = Some(TabBarAction::Menu(choice));
                }
            });
        });
        action
    }

    /// Draws the find or go-to-line bar above the status bar and shows its message.
    fn find_bar_panel(&mut self, ui: &mut egui::Ui) {
        if self.find_bar.mode == BarMode::Hidden {
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let bar = &mut self.find_bar;
        let message = egui::Panel::bottom("fed_find_bar")
            .show(ui, |ui| bar.show(ui, tab))
            .inner;
        if let Some(text) = message {
            self.set_status(text, Severity::Info);
        }
    }

    /// Starts the spell checker service when spell checking is on and none runs yet.
    fn ensure_spell(&mut self, ctx: &egui::Context) {
        if !self.settings.spell_check || self.spell.is_some() {
            return;
        }
        let Some(paths) = self.paths.clone() else {
            return;
        };
        self.spell = Some(SpellService::spawn(ctx.clone(), paths, true));
    }

    /// Returns the spell checking status (`Disabled` when off or not started).
    fn spell_status(&self) -> SpellStatus {
        match &self.spell {
            Some(service) if self.settings.spell_check => service.status(),
            _ => SpellStatus::Disabled,
        }
    }

    /// Draws the status bar: tab status on the left, notices and messages on the right.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let spell_status = self.spell_status();
        let mut retry = false;
        let vim = self.settings.vim_mode;
        let tab = &self.tabs[self.active];
        egui::Panel::bottom("fed_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(status_text(tab));
                if vim {
                    ui.label(vim_status_text(&tab.vim));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(msg) = &self.status {
                        let color = match msg.severity {
                            Severity::Info => ui.visuals().text_color(),
                            Severity::Warning => ui.visuals().warn_fg_color,
                            Severity::Error => ui.visuals().error_fg_color,
                        };
                        ui.colored_label(color, &msg.text);
                    }
                    if tab.buffer.is_large() {
                        ui.label(LARGE_FILE_NOTICE);
                    }
                    match &spell_status {
                        SpellStatus::Downloading => {
                            ui.label(SPELL_DOWNLOADING_NOTICE);
                        }
                        SpellStatus::Failed(message) => {
                            retry = ui.small_button("retry").clicked();
                            ui.colored_label(ui.visuals().warn_fg_color, message);
                        }
                        _ => {}
                    }
                });
            });
        });
        if retry && let Some(service) = &mut self.spell {
            service.retry();
        }
    }

    /// Opens the suggestions popup for the misspelled word under a right click, if any.
    fn open_spell_menu(&mut self, pos: egui::Pos2, clicked: std::ops::Range<usize>) {
        if !self.settings.spell_check {
            return;
        }
        let Some(checker) = self.spell.as_ref().and_then(SpellService::checker) else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(range) = tab
            .spell
            .marks
            .iter()
            .find(|m| m.start >= clicked.start && m.end <= clicked.end)
            .cloned()
        else {
            return;
        };
        let word = tab.buffer.rope().slice(range.clone()).to_string();
        let suggestions = checker
            .read()
            .expect("spell checker poisoned")
            .suggest(&word);
        self.spell_menu = Some(SpellMenu {
            tab_id: tab.id,
            pos,
            range,
            word,
            suggestions,
            fresh: true,
        });
    }

    /// Draws the suggestions popup and applies the chosen action.
    fn show_spell_menu(&mut self, ctx: &egui::Context) {
        let Some(menu) = &mut self.spell_menu else {
            return;
        };
        let mut chosen: Option<Option<String>> = None;
        let shown = egui::Area::new(egui::Id::unique("fed_spell_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(menu.pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    for s in &menu.suggestions {
                        if ui.button(s).clicked() {
                            chosen = Some(Some(s.clone()));
                        }
                    }
                    if menu.suggestions.is_empty() {
                        ui.label("no suggestions");
                    }
                    ui.separator();
                    if ui.button("Add to dictionary").clicked() {
                        chosen = Some(None);
                    }
                });
            });
        let dismissed = !menu.fresh
            && (shown.response.clicked_elsewhere()
                || ctx.input(|i| i.key_pressed(egui::Key::Escape)));
        menu.fresh = false;
        if let Some(choice) = chosen {
            self.apply_spell_choice(choice);
        } else if dismissed {
            self.spell_menu = None;
        }
    }

    /// Replaces the word of the open popup with `replacement`, or adds it to the dictionary
    /// when `replacement` is `None`. Closes the popup.
    fn apply_spell_choice(&mut self, replacement: Option<String>) {
        let Some(menu) = self.spell_menu.take() else {
            return;
        };
        match replacement {
            Some(text) => {
                let Some(tab) = self.tabs.iter_mut().find(|t| t.id == menu.tab_id) else {
                    return;
                };
                let rope = tab.buffer.rope();
                if menu.range.end > rope.len_chars()
                    || rope.slice(menu.range.clone()) != menu.word.as_str()
                {
                    return;
                }
                tab.end_typing();
                tab.buffer.replace(menu.range.clone(), &text);
                let caret = menu.range.start + text.chars().count();
                tab.editor.selection = crate::editing::Selection::caret(caret);
                tab.editor.preferred_col = None;
                tab.scroll_to_cursor = true;
                tab.focus_editor();
            }
            None => {
                let result = self.spell.as_ref().map(|s| s.add_word(&menu.word));
                if let Some(Err(e)) = result {
                    self.set_status(
                        format!("cannot save personal dictionary: {e}"),
                        Severity::Error,
                    );
                }
            }
        }
    }

    /// Resets every tab's vim state when vim mode was switched on or off in the settings.
    fn sync_vim_mode(&mut self) {
        if self.vim_applied == self.settings.vim_mode {
            return;
        }
        self.vim_applied = self.settings.vim_mode;
        self.vim_message = None;
        self.tabs.iter_mut().for_each(Tab::reset_vim);
    }

    /// Opens or closes the settings window. Opening takes the keyboard focus away from the
    /// editor (so Escape closes the window), closing gives it back.
    fn toggle_settings(&mut self, ctx: &egui::Context) {
        self.settings_window.toggle();
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        match self.settings_window.open {
            true => ctx.memory_mut(|m| m.surrender_focus(view::editor_id(tab.id))),
            false => tab.focus_editor(),
        }
    }

    /// Draws the settings window (scanning the system fonts the first time it opens) and
    /// applies what the user changed.
    fn show_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_window.open {
            return;
        }
        let spell = self.spell_status();
        let theme_names = self
            .loaded_assets
            .as_ref()
            .map(|a| a.theme_names())
            .unwrap_or_default();
        let catalog = self
            .font_catalog
            .get_or_insert_with(|| FontCatalog::spawn(ctx.clone()));
        let events = self.settings_window.show(
            ctx,
            &mut self.settings,
            &mut self.keymap,
            catalog,
            &theme_names,
            &spell,
        );
        self.apply_settings_events(ctx, events);
        if !self.settings_window.open {
            self.tabs[self.active].focus_editor();
        }
    }

    /// Carries out the settings window events: loads the font, retries the dictionaries and
    /// saves the settings once when anything changed. Live (dragged) changes only schedule a
    /// deferred save, written when the drag ends or `SAVE_DEBOUNCE` later. Theme, vim and
    /// spell check changes are picked up from `settings` by the per-frame sync functions.
    fn apply_settings_events(&mut self, ctx: &egui::Context, events: Vec<SettingsEvent>) {
        if events.is_empty() {
            return;
        }
        let mut save = false;
        for event in events {
            match event {
                SettingsEvent::Changed | SettingsEvent::KeymapChanged => save = true,
                SettingsEvent::ChangedLive => self.settings_save.schedule(Instant::now()),
                SettingsEvent::CommitLive => save |= self.settings_save.is_pending(),
                SettingsEvent::FontChanged => self.load_font(ctx, FontSlot::Editor),
                SettingsEvent::PreviewFontChanged => self.load_font(ctx, FontSlot::Preview),
                SettingsEvent::RetryDictionaries => {
                    if let Some(service) = &mut self.spell {
                        service.retry();
                    }
                }
            }
        }
        if save {
            self.save_settings();
        }
        self.sync_vim_mode();
        ctx.request_repaint();
    }

    /// Writes the deferred settings save once it is due, or schedules a repaint for then.
    fn flush_settings_save(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if self.settings_save.take_due(now) {
            self.save_settings();
            return;
        }
        if let Some(left) = self.settings_save.remaining(now) {
            ctx.request_repaint_after(left);
        }
    }

    /// Takes this frame's link open requests (clicked links in the Markdown preview) from
    /// egui's output and opens the allowed ones (see `markdown::is_openable_url`) with the
    /// system opener on background threads; other URLs are ignored. Removing the commands
    /// keeps the backend from trying (and failing) to open them itself.
    fn open_links(&mut self, ctx: &egui::Context) {
        let urls: Vec<String> = ctx.output_mut(|o| {
            let mut urls = Vec::new();
            o.commands.retain(|cmd| match cmd {
                egui::OutputCommand::OpenUrl(open) => {
                    urls.push(open.url.clone());
                    false
                }
                _ => true,
            });
            urls
        });
        let urls = urls
            .iter()
            .map(|u| u.trim().to_string())
            .filter(|u| markdown::is_openable_url(u));
        for url in urls {
            let tx = self.link_error_tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let Err(e) = open::that(&url) else { return };
                // The app may be gone already; then nobody needs the error.
                let _ = tx.send(format!("cannot open {url}: {e}"));
                ctx.request_repaint();
            });
        }
    }

    /// Shows the errors of link opener threads in the status bar.
    fn poll_link_errors(&mut self) {
        while let Ok(error) = self.link_error_rx.try_recv() {
            self.set_status(error, Severity::Error);
        }
    }

    /// Prints the time from process start to the end of the first frame, once, when
    /// startup tracing is on.
    fn trace_first_frame(&mut self) {
        let Some(start) = self.startup_trace.take() else {
            return;
        };
        eprintln!(
            "fed: first frame after {:.1} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }

    /// The configured family of a font slot.
    fn slot_family(&self, slot: FontSlot) -> Option<String> {
        match slot {
            FontSlot::Editor => self.settings.font_family.clone(),
            FontSlot::Preview => self.settings.preview_font_family.clone(),
        }
    }

    /// Applies the configured family of `slot` from the font catalog; an unknown family falls
    /// back to the default font with a status bar warning. Cancels a pending startup load.
    fn load_font(&mut self, ctx: &egui::Context, slot: FontSlot) {
        match slot {
            FontSlot::Editor => self.font_load = None,
            FontSlot::Preview => self.preview_font_load = None,
        }
        let Some(family) = self.slot_family(slot) else {
            self.apply_loaded_font(ctx, slot, "", None);
            return;
        };
        let font = self
            .font_catalog
            .as_ref()
            .and_then(|c| c.load_family_font(&family));
        self.apply_loaded_font(ctx, slot, &family, font);
    }

    /// Applies the results of the startup font loads once they are ready.
    fn poll_font_load(&mut self, ctx: &egui::Context) {
        for slot in [FontSlot::Editor, FontSlot::Preview] {
            let pending = match slot {
                FontSlot::Editor => &self.font_load,
                FontSlot::Preview => &self.preview_font_load,
            };
            let Some(load) = pending else { continue };
            let Some(font) = load.take() else { continue };
            let family = load.family().to_string();
            match slot {
                FontSlot::Editor => self.font_load = None,
                FontSlot::Preview => self.preview_font_load = None,
            }
            self.apply_loaded_font(ctx, slot, &family, font);
        }
    }

    /// Installs `font` in `slot`, or the default font plus a warning when the family
    /// `family` was not found (an empty `family` means "default", without a warning).
    fn apply_loaded_font(
        &mut self,
        ctx: &egui::Context,
        slot: FontSlot,
        family: &str,
        font: Option<egui::FontData>,
    ) {
        if font.is_none() && !family.is_empty() {
            self.set_status(
                format!("font '{family}' not found, using the default font"),
                Severity::Warning,
            );
        }
        let loaded = font.map(|f| (family.to_string(), f));
        match slot {
            FontSlot::Editor => self.editor_font = loaded,
            FontSlot::Preview => self.preview_font = loaded,
        }
        fonts::apply_fonts(ctx, self.editor_font.as_ref(), self.preview_font.as_ref());
    }

    /// Carries out an entry picked in the application menu.
    fn run_menu_choice(&mut self, ctx: &egui::Context, choice: MenuChoice) {
        match choice {
            MenuChoice::Action(action) => self.dispatch(ctx, action),
            MenuChoice::About => self.about_open = true,
            // Goes through the regular close request, which asks about unsaved files.
            MenuChoice::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    /// Shows the About dialog while it is open.
    fn show_about(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        if menu::show_about(ctx) {
            self.about_open = false;
            self.tabs[self.active].focus_editor();
        }
    }

    /// Shows `text` in the vim command bar, in the error color when `error`.
    fn set_vim_message(&mut self, text: String, error: bool) {
        self.vim_message = Some(VimMessage { text, error });
    }

    /// Carries out the effects of the vim keys handled this frame on the active tab.
    fn apply_vim_effects(&mut self, ctx: &egui::Context, effects: Vec<VimEffect>) {
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
    fn vim_write(&mut self) -> bool {
        let outcome = self.save_tab_outcome(self.active);
        let saved = self.report_save(self.active, &outcome);
        let path = tab_path(&self.tabs[self.active]);
        self.vim_message = vim_save_message(&outcome, &path);
        saved
    }

    /// `:w {file}`: writes a copy of the active tab, which stays bound to its own path. An
    /// existing other file is only overwritten with `force` (`:w!`); the tab's own file is a
    /// plain `:w`.
    fn vim_write_copy(&mut self, path: PathBuf, force: bool) {
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
    fn vim_quit(&mut self, ctx: &egui::Context, force: bool) {
        let tab = &mut self.tabs[self.active];
        tab.end_typing();
        if !force && close_requires_confirmation(tab) {
            self.set_vim_message(VIM_NO_WRITE.to_string(), true);
            return;
        }
        self.remove_tab(ctx, self.active);
    }

    /// `:e {file}`: opens the file (relative to the active file's directory) in a tab.
    fn vim_edit(&mut self, path: PathBuf) {
        let path = resolve_vim_path(path, self.tabs[self.active].buffer.path());
        if let Err(e) = self.try_open(path) {
            self.set_vim_message(e.clone(), true);
            self.set_status(e, Severity::Error);
        }
    }

    /// Moves the caret of the active tab to the first non-blank of 1-based `line`.
    fn vim_goto_line(&mut self, line: usize) {
        let tab = &mut self.tabs[self.active];
        let rope = tab.buffer.rope();
        let line = line.clamp(1, rope.len_lines()) - 1;
        let pos = crate::text_nav::first_non_blank(rope, rope.line_to_char(line));
        tab.editor.selection = crate::editing::Selection::caret(pos);
        tab.scroll_to_cursor = true;
    }

    /// Draws the vim command bar above the status bar: the command line with a block cursor
    /// while typing a command, otherwise the last message.
    fn vim_command_bar(&mut self, ui: &mut egui::Ui) {
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

impl eframe::App for FedApp {
    /// Draws one frame: shortcuts first, then tab bar, status bar, editor and dialogs.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        self.expire_status(&ctx);
        self.handle_close_request(&ctx);
        self.ensure_spell(&ctx);
        self.sync_vim_mode();
        self.poll_font_load(&ctx);
        self.poll_link_errors();
        match self.settings_window.capture_keys(&ctx, &mut self.keymap) {
            Some(events) => self.apply_settings_events(&ctx, events),
            None => self.handle_shortcuts(&ctx),
        }
        match self.tab_bar(ui) {
            Some(TabBarAction::Activate(i)) => self.activate(i),
            Some(TabBarAction::Close(i)) => self.request_close_tab(&ctx, i),
            Some(TabBarAction::TogglePreview) => self.toggle_preview(),
            Some(TabBarAction::Menu(choice)) => self.run_menu_choice(&ctx, choice),
            None => {}
        }
        self.status_bar(ui);
        self.vim_command_bar(ui);
        self.find_bar_panel(ui);
        self.preview_panel(ui);
        let opts = self.view_options();
        let checker = match self.settings.spell_check {
            true => self.spell.as_ref().and_then(SpellService::checker),
            false => None,
        };
        let mut context_click = None;
        let vim_mode = self.settings.vim_mode;
        let mut vim_effects: Vec<VimEffect> = Vec::new();
        let mut vim_key_seen = false;
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(tab) = self.tabs.get_mut(self.active) else {
                return;
            };
            let matches = self.find_bar.matches(tab);
            if self.loaded_assets.is_none() {
                self.loaded_assets = self.assets.get();
            }
            let assets = self.loaded_assets.as_deref();
            if let Some(a) = assets {
                tab.refresh_syntax(a);
            }
            let theme = if ui.visuals().dark_mode {
                &self.settings.code_theme_dark
            } else {
                &self.settings.code_theme_light
            };
            let syntax = tab.syntax.take().unwrap_or_default();
            // Decorator state is moved out of the tab so the decorators do not borrow it.
            let mut cache = std::mem::take(&mut tab.highlight);
            let mut spell_cache = std::mem::take(&mut tab.spell);
            let large = tab.buffer.is_large();
            let edit = opts.edit;
            let mut frame_ended = false;
            let mut route_to_vim = |ev: &egui::Event, tab: &mut Tab| {
                vim_key_seen = true;
                if frame_ended {
                    return true;
                }
                let keys: Vec<VimKey> = match ev {
                    // Pasting types the text in Insert mode only (p/P use the register).
                    egui::Event::Paste(text) if tab.vim.mode() == Mode::Insert => text
                        .chars()
                        .filter(|&c| c != '\r')
                        .map(VimKey::Char)
                        .collect(),
                    _ => view::egui_event_to_vim_keys(ev),
                };
                for key in keys {
                    let fx = tab
                        .vim
                        .handle_key(key, &mut tab.buffer, &mut tab.editor, &edit);
                    frame_ended = fx.iter().any(ends_vim_frame);
                    vim_effects.extend(fx);
                    if frame_ended {
                        break;
                    }
                }
                true
            };
            let key_handler: Option<&mut KeyHandler<'_>> = match vim_mode {
                true => Some(&mut route_to_vim),
                false => None,
            };
            // Plain text until the syntax regexes are compiled off the UI thread.
            let warm = self
                .loaded_assets
                .as_ref()
                .is_some_and(|a| a.ensure_warm(&syntax, tab.buffer.rope(), ui.ctx()));
            let mut highlighted =
                HighlightDecorator::new(&mut cache, assets, &syntax, theme, !large && warm);
            let spell_checker = checker.filter(|_| !large);
            let spell_active = spell_checker.is_some();
            let out = match spell_checker {
                Some(c) => {
                    let mode = SpellMode::for_syntax(&syntax);
                    let mut spell =
                        SpellDecorator::new(&mut highlighted, c, mode, &mut spell_cache, ui.ctx());
                    let mut deco = MatchDecorator::new(&mut spell, &matches);
                    view::show_editor(ui, tab, &opts, &mut deco, key_handler)
                }
                None => {
                    let mut deco = MatchDecorator::new(&mut highlighted, &matches);
                    view::show_editor(ui, tab, &opts, &mut deco, key_handler)
                }
            };
            tab.highlight = cache;
            tab.syntax = assets.map(|_| syntax);
            if !spell_active {
                // Nothing was checked: drop the marks so no stale suggestion popup opens.
                spell_cache.marks.clear();
            }
            tab.spell = spell_cache;
            if vim_mode && out.mouse_selected {
                tab.vim.mouse_selection(&mut tab.buffer, &mut tab.editor);
            }
            if let Some(text) = out.clipboard {
                ui.ctx().copy_text(text);
            }
            if let Some((range, _)) = out.context_word {
                context_click = ui.input(|i| i.pointer.latest_pos()).map(|pos| (pos, range));
            }
        });
        if vim_key_seen {
            self.vim_message = None;
        }
        self.apply_vim_effects(&ctx, vim_effects);
        if let Some((pos, range)) = context_click {
            self.open_spell_menu(pos, range);
        }
        self.show_spell_menu(&ctx);
        self.show_settings(&ctx);
        self.show_about(&ctx);
        self.show_dialogs(&ctx);
        self.flush_settings_save(&ctx);
        self.open_links(&ctx);
        self.trace_first_frame();
    }

    /// Writes a pending settings save and the session when the application exits.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.settings_save.is_pending() {
            self.save_settings();
        }
        self.save_session();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::{EditCommand, Selection};

    const SET: EditSettings = EditSettings {
        tab_width: 4,
        insert_spaces: true,
    };

    #[test]
    fn shortcut_lookup_is_exact() {
        let km = Keymap::defaults();
        let ctrl = egui::Modifiers::COMMAND;
        let ctrl_shift = egui::Modifiers {
            shift: true,
            ..egui::Modifiers::COMMAND
        };
        assert_eq!(
            shortcut_action(&km, egui::Key::N, ctrl),
            Some(Action::NewTab)
        );
        assert_eq!(shortcut_action(&km, egui::Key::N, ctrl_shift), None);
        assert_eq!(shortcut_action(&km, egui::Key::S, ctrl), Some(Action::Save));
        assert_eq!(
            shortcut_action(&km, egui::Key::S, ctrl_shift),
            Some(Action::SaveAs)
        );
        assert_eq!(shortcut_action(&km, egui::Key::Z, ctrl), Some(Action::Undo));
        assert_eq!(
            shortcut_action(&km, egui::Key::Z, ctrl_shift),
            Some(Action::Redo)
        );
        assert_eq!(
            shortcut_action(&km, egui::Key::N, egui::Modifiers::NONE),
            None
        );
    }

    #[test]
    fn tab_switch_shortcuts_and_aliases() {
        let km = Keymap::defaults();
        let ctrl = egui::Modifiers::COMMAND;
        assert_eq!(
            shortcut_action(&km, egui::Key::Num1, ctrl),
            Some(Action::PrevTab)
        );
        assert_eq!(
            shortcut_action(&km, egui::Key::Num2, ctrl),
            Some(Action::NextTab)
        );
        let chord = KeyChord::parse("Ctrl+Tab").unwrap();
        let mods = egui::Modifiers {
            command: chord.command,
            ctrl: chord.ctrl || chord.command,
            ..egui::Modifiers::NONE
        };
        assert_eq!(
            shortcut_action(&km, egui::Key::Tab, mods),
            Some(Action::NextTab)
        );
        let shifted = egui::Modifiers {
            shift: true,
            ..mods
        };
        assert_eq!(
            shortcut_action(&km, egui::Key::Tab, shifted),
            Some(Action::PrevTab)
        );
    }

    #[test]
    fn close_requires_confirmation_when_modified() {
        let mut tab = Tab::new(Buffer::from_text("hello"));
        assert!(!close_requires_confirmation(&tab));
        tab.apply(EditCommand::InsertText("x".into()), &SET);
        assert!(close_requires_confirmation(&tab));
        tab.end_typing();
        tab.apply(EditCommand::Undo, &SET);
        assert!(!close_requires_confirmation(&tab));
    }

    #[test]
    fn tabs_needing_confirmation_lists_modified_only() {
        let mut tabs = vec![
            Tab::new(Buffer::from_text("a")),
            Tab::new(Buffer::from_text("b")),
            Tab::new(Buffer::from_text("c")),
        ];
        assert!(tabs_needing_confirmation(&tabs).is_empty());
        tabs[0].apply(EditCommand::InsertText("x".into()), &SET);
        tabs[2].apply(EditCommand::InsertText("y".into()), &SET);
        assert_eq!(tabs_needing_confirmation(&tabs), vec![0, 2]);
    }

    #[test]
    fn status_text_format() {
        let tab = Tab::new(Buffer::new());
        assert_eq!(status_text(&tab), "untitled  1:1  LF");

        let mut tab = Tab::new(Buffer::from_text("a\r\nbc"));
        tab.buffer.set_path(PathBuf::from("src/lib.rs"));
        tab.editor.selection = Selection::caret(3);
        assert_eq!(status_text(&tab), "src/lib.rs  2:2  CRLF");

        tab.apply(EditCommand::InsertText("x".into()), &SET);
        assert_eq!(status_text(&tab), "src/lib.rs  2:3  CRLF  [+]");
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
    fn vim_mode_keeps_only_ctrl_shortcuts_and_leaves_undo_to_vim() {
        let ctrl = egui::Modifiers::COMMAND;
        assert!(shortcut_allowed_in_vim(Action::Save, ctrl));
        assert!(shortcut_allowed_in_vim(Action::Find, ctrl));
        assert!(!shortcut_allowed_in_vim(
            Action::Save,
            egui::Modifiers::NONE
        ));
        assert!(!shortcut_allowed_in_vim(
            Action::NextTab,
            egui::Modifiers::ALT
        ));
        assert!(!shortcut_allowed_in_vim(Action::Undo, ctrl));
        assert!(!shortcut_allowed_in_vim(Action::Redo, ctrl));
        assert!(!shortcut_allowed_in_vim(Action::SelectAll, ctrl));
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
}

/// What the user did on a single tab.
enum TabClick {
    Activate,
    Close,
}

/// Draws one tab (label, close button, accent line when selected) and returns the click, if any.
fn draw_tab(ui: &mut egui::Ui, tab: &Tab, selected: bool) -> Option<TabClick> {
    let visuals = ui.visuals().clone();
    let text_color = if selected {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley =
        ui.painter()
            .layout_no_wrap(tab.buffer.display_name().to_string(), font, text_color);
    let width =
        TAB_PADDING + galley.size().x + TAB_PADDING / 2.0 + TAB_CLOSE_SIZE + TAB_PADDING / 2.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, TAB_HEIGHT), egui::Sense::click());
    let resp = resp.on_hover_text(tab_path(tab));
    let mut result = None;
    if resp.clicked() {
        result = Some(TabClick::Activate);
    }
    if resp.middle_clicked() {
        result = Some(TabClick::Close);
    }
    let bg = if selected {
        visuals.extreme_bg_color
    } else {
        visuals.panel_fill
    };
    let bg = if !selected && resp.hovered() {
        visuals.faint_bg_color
    } else {
        bg
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, bg);
    if selected {
        let line = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), TAB_ACCENT_HEIGHT));
        painter.rect_filled(line, 0.0, TAB_ACCENT);
    }
    painter.vline(
        rect.right(),
        rect.y_range(),
        egui::Stroke::new(1.0, visuals.window_stroke.color),
    );
    painter.galley(
        egui::pos2(
            rect.left() + TAB_PADDING,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        text_color,
    );
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(
            rect.right() - TAB_PADDING / 2.0 - TAB_CLOSE_SIZE / 2.0,
            rect.center().y,
        ),
        egui::Vec2::splat(TAB_CLOSE_SIZE),
    );
    let close_resp = ui
        .interact(
            close_rect,
            egui::Id::unique(("fed_tab_close", tab.id)),
            egui::Sense::click(),
        )
        .on_hover_text("Close tab");
    if close_resp.clicked() {
        result = Some(TabClick::Close);
    }
    let modified = tab.buffer.is_modified();
    let show_x = close_resp.hovered() || (!modified && (selected || resp.hovered()));
    let painter = ui.painter();
    if close_resp.hovered() {
        painter.rect_filled(close_rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
    }
    if show_x {
        let half = 4.0;
        let c = close_rect.center();
        let stroke = egui::Stroke::new(1.3, visuals.strong_text_color());
        painter.line_segment(
            [c + egui::vec2(-half, -half), c + egui::vec2(half, half)],
            stroke,
        );
        painter.line_segment(
            [c + egui::vec2(-half, half), c + egui::vec2(half, -half)],
            stroke,
        );
    } else if modified {
        painter.circle_filled(close_rect.center(), 3.5, text_color);
    }
    result
}

/// Draws the wide "Preview" toggle at the right of the tab bar; returns true when clicked.
fn draw_preview_button(ui: &mut egui::Ui, open: bool) -> bool {
    let visuals = ui.visuals().clone();
    let size = egui::vec2(PREVIEW_BUTTON_WIDTH, TAB_HEIGHT - 8.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let bg = if open {
        visuals.widgets.active.weak_bg_fill
    } else if resp.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.widgets.inactive.weak_bg_fill
    };
    let text_color = if open {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let painter = ui.painter();
    painter.rect_filled(rect, PREVIEW_BUTTON_RADIUS, bg);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Preview",
        egui::TextStyle::Body.resolve(ui.style()),
        text_color,
    );
    resp.clicked()
}
