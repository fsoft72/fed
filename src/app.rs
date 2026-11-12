//! Application shell: tabs, file dialogs, shortcuts, status bar and session handling.

mod dialogs;
mod font_manager;
mod menu;
mod spell_menu;
mod status;
mod tab_bar;
mod vim;

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::buffer::{self, BACKGROUND_LOAD_BYTES, Buffer, LoadJob};
use crate::config::{self, ConfigPaths, Session, Settings, UiTheme};
use crate::editing::{EditCommand, EditSettings};
use crate::find_bar::{BarMode, FindBar};
use crate::fonts;
use crate::highlight::{AssetsHandle, HighlightAssets};
use crate::keymap::{Action, KeyChord, Keymap};
use crate::markdown::{self, Preview};
use crate::settings_ui::{SaveDebounce, SettingsEvent, SettingsWindow};
use crate::spell::SpellService;
use crate::tab::{EditorEnv, Tab};
use crate::view::{self, KeyHandler, ViewOptions};
use crate::vim::VimEffect;
use dialogs::Pending;
use font_manager::{FontManager, FontSlot};
use menu::MenuChoice;
use spell_menu::SpellMenu;
pub use status::status_text;
use status::{Severity, StatusBar};
use tab_bar::TabBarAction;
use vim::VimMessage;
pub use vim::vim_status_text;

/// How often pending background file loads are checked.
const LOAD_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Name shown for a tab without a file path.
const UNTITLED: &str = "untitled";
/// Status message shown when the preview is requested on a non-Markdown tab.
const PREVIEW_UNAVAILABLE: &str = "preview is only available for Markdown";
/// Fixed, non-configurable chord aliases, used only when the keymap has no binding for the
/// pressed chord.
const SHORTCUT_ALIASES: &[(&str, Action)] = &[
    ("Cmd+Shift+Z", Action::Redo),
    ("Ctrl+Tab", Action::NextTab),
    ("Ctrl+Shift+Tab", Action::PrevTab),
];

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

/// True when the app shortcut `action` bound to a chord with `mods` may fire while vim mode is
/// on: only chords with Ctrl/Cmd, and never Undo/Redo/SelectAll (vim owns `u`, `Ctrl+R`).
fn shortcut_allowed_in_vim(action: Action, mods: egui::Modifiers) -> bool {
    (mods.ctrl || mods.command) && !needs_editor_focus(action)
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
    PARSED_ALIASES
        .iter()
        .find_map(|(alias, action)| (*alias == chord).then_some(*action))
}

/// `SHORTCUT_ALIASES` parsed once.
static PARSED_ALIASES: LazyLock<Vec<(KeyChord, Action)>> = LazyLock::new(|| {
    SHORTCUT_ALIASES
        .iter()
        .map(|(text, action)| {
            let chord = KeyChord::parse(text).expect("shortcut aliases are valid chords");
            (chord, *action)
        })
        .collect()
});

/// True for actions that edit the text and must leave text fields (find bar) alone.
fn needs_editor_focus(action: Action) -> bool {
    matches!(action, Action::Undo | Action::Redo | Action::SelectAll)
}

/// Name of the code theme of `settings` for the dark or light UI.
fn code_theme(settings: &Settings, dark: bool) -> &str {
    match dark {
        true => &settings.code_theme_dark,
        false => &settings.code_theme_light,
    }
}

/// Directory of the file of `buffer`, where file dialogs start; `None` when untitled or
/// when the path has no directory part.
fn dialog_dir(buffer: &Buffer) -> Option<&Path> {
    buffer
        .path()
        .and_then(Path::parent)
        .filter(|dir| !dir.as_os_str().is_empty())
}

/// File dialogs used by the application, behind a trait so `FedApp` can be driven in tests
/// without native windows.
pub trait FileDialogs {
    /// Asks for files to open, starting in `dir`; `None` when cancelled.
    fn pick_files(&self, dir: Option<&Path>) -> Option<Vec<PathBuf>>;

    /// Asks where to save a file named `name`, starting in `dir`; `None` when cancelled.
    fn save_file(&self, dir: Option<&Path>, name: &str) -> Option<PathBuf>;
}

/// The native dialogs of the platform (rfd).
pub struct NativeDialogs;

impl FileDialogs for NativeDialogs {
    /// Shows the native open dialog with multiple selection.
    fn pick_files(&self, dir: Option<&Path>) -> Option<Vec<PathBuf>> {
        let mut dialog = rfd::FileDialog::new();
        if let Some(dir) = dir {
            dialog = dialog.set_directory(dir);
        }
        dialog.pick_files()
    }

    /// Shows the native save dialog with `name` prefilled.
    fn save_file(&self, dir: Option<&Path>, name: &str) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new().set_file_name(name);
        if let Some(dir) = dir {
            dialog = dialog.set_directory(dir);
        }
        dialog.save_file()
    }
}

/// Result of a save attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SaveOutcome {
    /// The file was written.
    Saved,
    /// The user cancelled the Save As dialog.
    Cancelled,
    /// Nothing was written: another program changed the file since it was read.
    ChangedOnDisk,
    /// Writing failed with this message.
    Failed(String),
}

impl SaveOutcome {
    /// Converts a buffer save result.
    fn from_result(result: Result<(), crate::buffer::BufferError>) -> SaveOutcome {
        match result {
            Ok(()) => SaveOutcome::Saved,
            Err(crate::buffer::BufferError::ChangedOnDisk(_)) => SaveOutcome::ChangedOnDisk,
            Err(e) => SaveOutcome::Failed(e.to_string()),
        }
    }
}

/// What the editor panel reported during one frame.
#[derive(Default)]
struct EditorFrame {
    /// Screen position and char range of a right-clicked word.
    context_click: Option<(egui::Pos2, std::ops::Range<usize>)>,
    /// Effects of the keys handled by vim.
    vim_effects: Vec<VimEffect>,
    /// True when vim received at least one key.
    vim_key_seen: bool,
}

/// Top-level editor application state.
pub struct FedApp {
    /// Open documents, one per tab; never empty.
    tabs: Vec<Tab>,
    /// Big files being read on background threads; each becomes a tab when done.
    loads: Vec<LoadJob>,
    /// The open files changed since the session was last written.
    session_dirty: bool,
    /// Index of the tab shown in the editor area.
    active: usize,
    /// Editor settings.
    settings: Settings,
    /// Keyboard shortcuts.
    keymap: Keymap,
    /// Config file locations; `None` runs without persistence.
    paths: Option<ConfigPaths>,
    /// Status bar message and link opener errors.
    status: StatusBar,
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
    /// Editor and preview fonts and the system font catalog.
    fonts: FontManager,
    /// Deferred save of live (dragged) settings changes.
    settings_save: SaveDebounce,
    /// Process start time, while the first frame is still to be traced (`FED_TRACE_STARTUP`).
    startup_trace: Option<Instant>,
    /// Open and save dialogs.
    dialogs: Box<dyn FileDialogs>,
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
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let paths = ConfigPaths::detect();
        let dialogs = Box::new(NativeDialogs);
        FedApp::with_deps(&cc.egui_ctx, files, startup_trace, paths, dialogs)
    }

    /// Creates the application like `new`, with explicit config locations (`None` runs
    /// without persistence) and file dialogs, for tests and other hosts.
    pub fn with_deps(
        ctx: &egui::Context,
        files: Vec<PathBuf>,
        startup_trace: Option<Instant>,
        paths: Option<ConfigPaths>,
        dialogs: Box<dyn FileDialogs>,
    ) -> FedApp {
        let (settings, mut problems) = match &paths {
            Some(p) => config::load_settings(p),
            None => (
                Settings::default(),
                vec!["no config directory: settings and session are not saved".to_string()],
            ),
        };
        let (keymap, key_warnings) = Keymap::from_overrides(&settings.keybindings);
        problems.extend(key_warnings);
        let fonts = FontManager::new(ctx, &settings);
        let mut app = FedApp {
            tabs: Vec::new(),
            loads: Vec::new(),
            session_dirty: false,
            active: 0,
            settings,
            keymap,
            paths,
            status: StatusBar::new(),
            pending: None,
            applied_theme: None,
            find_bar: FindBar::new(),
            assets: AssetsHandle::spawn(ctx.clone()),
            loaded_assets: None,
            spell: None,
            spell_menu: None,
            allow_close: false,
            about_open: false,
            vim_applied: false,
            vim_message: None,
            settings_window: SettingsWindow::new(),
            fonts,
            settings_save: SaveDebounce::default(),
            startup_trace,
            dialogs,
        };
        app.vim_applied = app.settings.vim_mode;
        app.apply_theme(ctx);
        app.ensure_spell(ctx);
        let files = if files.is_empty() && app.settings.restore_session {
            let (files, warnings) = app.session_files();
            problems.extend(warnings);
            files
        } else {
            files
        };
        if !problems.is_empty() {
            app.set_status(problems.join("; "), Severity::Warning);
        }
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

    /// Opens `path` in a tab (see `open_path`), returning a readable error. Files of
    /// `BACKGROUND_LOAD_BYTES` or more are read on a background thread and get their tab in a
    /// later frame (`poll_loads`); files over `buffer::MAX_FILE_BYTES` are refused.
    fn try_open(&mut self, path: PathBuf) -> Result<(), String> {
        // One canonicalize for the new path; open tabs and loads keep theirs.
        let canonical = path.canonicalize().ok();
        let canonical = canonical.as_deref();
        let open = self
            .tabs
            .iter()
            .position(|t| t.buffer.is_bound_to(&path, canonical));
        if let Some(idx) = open {
            self.activate(idx);
            return Ok(());
        }
        if self
            .loads
            .iter()
            .any(|job| job.is_loading(&path, canonical))
        {
            return Ok(());
        }
        let size = buffer::check_size(&path).map_err(|e| e.to_string())?;
        if size >= BACKGROUND_LOAD_BYTES {
            self.set_status(format!("loading {}...", path.display()), Severity::Info);
            self.loads.push(LoadJob::spawn(path));
            return Ok(());
        }
        let buffer = Buffer::load(&path).map_err(|e| e.to_string())?;
        self.insert_tab(Tab::new(buffer));
        Ok(())
    }

    /// Shows `tab` as the active tab, replacing the active tab when it is blank.
    fn insert_tab(&mut self, tab: Tab) {
        self.leave_active();
        self.session_dirty = true;
        match self.tabs.get(self.active) {
            Some(current) if is_blank(current) => self.tabs[self.active] = tab,
            _ => {
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
            }
        }
    }

    /// Opens the files whose background load finished (errors go to the status bar) and
    /// keeps polling while loads are pending.
    fn poll_loads(&mut self, ctx: &egui::Context) {
        if self.loads.is_empty() {
            return;
        }
        for job in std::mem::take(&mut self.loads) {
            match job.poll() {
                None => self.loads.push(job),
                Some(Ok(buffer)) => {
                    self.insert_tab(Tab::new(buffer));
                    self.set_status(format!("opened {}", job.path().display()), Severity::Info);
                }
                Some(Err(e)) => self.set_status(e.to_string(), Severity::Error),
            }
        }
        if !self.loads.is_empty() {
            ctx.request_repaint_after(LOAD_POLL_INTERVAL);
        }
    }

    /// Files of the previous session that still exist, plus warnings about a session file
    /// that could not be read.
    fn session_files(&self) -> (Vec<PathBuf>, Vec<String>) {
        let Some(paths) = &self.paths else {
            return (Vec::new(), Vec::new());
        };
        let (session, warnings) = config::load_session(paths);
        let files = session.files.into_iter().filter(|p| p.exists()).collect();
        (files, warnings)
    }

    /// Writes the open file paths as the session, when session restore is on.
    fn save_session(&self) -> Result<(), String> {
        if !self.settings.restore_session {
            return Ok(());
        }
        let Some(paths) = &self.paths else {
            return Ok(());
        };
        let files = self
            .tabs
            .iter()
            .filter_map(|t| t.buffer.path())
            .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
            .collect();
        config::save_session(paths, &Session { files })
            .map_err(|e| format!("cannot save session to {}: {e}", paths.dir.display()))
    }

    /// Saves the session when the set of open files changed this frame, so a failure shows
    /// up in the status bar while the editor runs instead of only at exit.
    fn sync_session(&mut self) {
        if !std::mem::take(&mut self.session_dirty) {
            return;
        }
        if let Err(e) = self.save_session() {
            self.set_status(e, Severity::Error);
        }
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
        let dir = dialog_dir(&self.tabs[self.active].buffer);
        let Some(files) = self.dialogs.pick_files(dir) else {
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
        let name = tab.buffer.display_name();
        let Some(path) = self.dialogs.save_file(dialog_dir(&tab.buffer), &name) else {
            return SaveOutcome::Cancelled;
        };
        let outcome = SaveOutcome::from_result(tab.buffer.save_as(path));
        // The tab may have a new path.
        self.session_dirty |= outcome == SaveOutcome::Saved;
        outcome
    }

    /// Saves the tab at `idx` to its file even when the file changed on disk, without
    /// reporting.
    fn overwrite_tab_outcome(&mut self, idx: usize) -> SaveOutcome {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return SaveOutcome::Cancelled;
        };
        tab.end_typing();
        SaveOutcome::from_result(tab.buffer.save_overwrite())
    }

    /// Replaces the text of the tab at `idx` with its file on disk (one undo step) and reports
    /// the result in the status bar.
    fn reload_tab(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        tab.end_typing();
        match tab.buffer.reload() {
            Ok(()) => {
                tab.clamp_selection();
                let path = tab_path(tab);
                self.set_status(format!("reloaded {path}"), Severity::Info);
            }
            Err(e) => self.set_status(e.to_string(), Severity::Error),
        }
    }

    /// Shows the outcome of saving the tab at `idx` in the status bar (a file changed on disk
    /// asks whether to overwrite or reload it); returns true when the file was written.
    fn report_save(&mut self, idx: usize, outcome: &SaveOutcome) -> bool {
        match outcome {
            SaveOutcome::ChangedOnDisk => {
                let path = tab_path(&self.tabs[idx]);
                self.set_status(format!("{path} changed on disk"), Severity::Warning);
                self.pending = Some(Pending::ChangedOnDisk(self.tabs[idx].id));
                false
            }
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
        tab.toggle_preview();
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
        let layout = markdown::PreviewLayout {
            font_size: self.settings.preview_font_size,
            line_spacing: self.settings.preview_line_spacing,
        };
        egui::Panel::right("fed_preview")
            .resizable(true)
            .default_size(half)
            .show(ui, |ui| {
                let preview = tab.preview.get_or_insert_with(Preview::new);
                preview.show(ui, &tab.buffer, layout);
            });
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
            .as_deref()
            .map_or(&[][..], HighlightAssets::theme_names);
        let catalog = self.fonts.catalog(ctx);
        let events = self.settings_window.show(
            ctx,
            &mut self.settings,
            &mut self.keymap,
            catalog,
            theme_names,
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

    /// Switches `slot` to its configured family (loaded in the background; a family that does
    /// not exist is reported by the next `FontManager::poll`).
    fn load_font(&mut self, ctx: &egui::Context, slot: FontSlot) {
        let family = match slot {
            FontSlot::Editor => self.settings.font_family.as_deref(),
            FontSlot::Preview => self.settings.preview_font_family.as_deref(),
        };
        self.fonts.load(ctx, slot, family);
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

    /// Takes the syntax assets once their background load is done.
    fn poll_assets(&mut self) {
        if self.loaded_assets.is_none() {
            self.loaded_assets = self.assets.get();
        }
    }

    /// Draws the editor of the active tab in the central panel. In vim mode its keys go to
    /// the tab's vim state; the resulting effects are returned for the caller to carry out.
    fn editor_panel(&mut self, ui: &mut egui::Ui) -> EditorFrame {
        let opts = self.view_options();
        let checker = match self.settings.spell_check {
            true => self.spell.as_ref().and_then(SpellService::checker),
            false => None,
        };
        let mut frame = EditorFrame::default();
        egui::CentralPanel::default().show(ui, |ui| {
            let theme = code_theme(&self.settings, ui.visuals().dark_mode);
            let Some(tab) = self.tabs.get_mut(self.active) else {
                return;
            };
            let env = EditorEnv {
                assets: self.loaded_assets.as_ref(),
                theme,
                checker,
                find: &self.find_bar,
            };
            let edit = opts.edit;
            let mut frame_ended = false;
            let (effects, key_seen) = (&mut frame.vim_effects, &mut frame.vim_key_seen);
            let mut route_to_vim = |ev: &egui::Event, tab: &mut Tab| {
                *key_seen = true;
                vim::route_event(ev, tab, &edit, effects, &mut frame_ended)
            };
            let key_handler: Option<&mut KeyHandler<'_>> = match opts.vim {
                true => Some(&mut route_to_vim),
                false => None,
            };
            let out = tab.show_editor(ui, &opts, env, key_handler);
            if let Some((range, _)) = out.context_word {
                frame.context_click = ui.input(|i| i.pointer.latest_pos()).map(|pos| (pos, range));
            }
        });
        frame
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
}

impl eframe::App for FedApp {
    /// Draws one frame: shortcuts first, then tab bar, status bar, editor and dialogs.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        self.status.expire(&ctx);
        self.handle_close_request(&ctx);
        self.ensure_spell(&ctx);
        self.sync_vim_mode();
        for warning in self.fonts.poll(&ctx) {
            self.set_status(warning, Severity::Warning);
        }
        self.status.poll_link_errors();
        self.poll_loads(&ctx);
        self.poll_assets();
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
        let editor = self.editor_panel(ui);
        if editor.vim_key_seen {
            self.vim_message = None;
        }
        self.apply_vim_effects(&ctx, editor.vim_effects);
        if let Some((pos, range)) = editor.context_click {
            self.open_spell_menu(pos, range);
        }
        self.show_spell_menu(&ctx);
        self.show_settings(&ctx);
        self.show_about(&ctx);
        self.show_dialogs(&ctx);
        self.flush_settings_save(&ctx);
        self.sync_session();
        self.open_links(&ctx);
        self.trace_first_frame();
    }

    /// Writes a pending settings save and the session when the application exits.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.settings_save.is_pending() {
            self.save_settings();
        }
        // The window is closing: stderr is the only place left to report to.
        if let Err(e) = self.save_session() {
            eprintln!("fed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::EditCommand;

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
    fn every_shortcut_alias_is_parsed() {
        assert_eq!(PARSED_ALIASES.len(), SHORTCUT_ALIASES.len());
        for ((text, action), (chord, parsed)) in SHORTCUT_ALIASES.iter().zip(PARSED_ALIASES.iter())
        {
            assert_eq!(KeyChord::parse(text).as_ref(), Ok(chord));
            assert_eq!(action, parsed);
        }
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

    /// File dialogs answering with fixed results.
    #[derive(Default)]
    struct FakeDialogs {
        /// Files returned by `pick_files`.
        open: Option<Vec<PathBuf>>,
        /// Path returned by `save_file`.
        save: Option<PathBuf>,
    }

    impl FileDialogs for FakeDialogs {
        fn pick_files(&self, _dir: Option<&Path>) -> Option<Vec<PathBuf>> {
            self.open.clone()
        }

        fn save_file(&self, _dir: Option<&Path>, _name: &str) -> Option<PathBuf> {
            self.save.clone()
        }
    }

    /// Builds an application without config directory, opening `files`.
    fn test_app(files: Vec<PathBuf>, dialogs: FakeDialogs) -> FedApp {
        let ctx = egui::Context::default();
        FedApp::with_deps(&ctx, files, None, None, Box::new(dialogs))
    }

    /// Creates a fresh temp directory for an app test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fed-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saving_a_file_changed_on_disk_asks_first() {
        let dir = temp_dir("changed");
        let path = dir.join("a.txt");
        std::fs::write(&path, "alpha").unwrap();
        let mut app = test_app(vec![path.clone()], FakeDialogs::default());
        app.apply_to_active(EditCommand::InsertText("x".into()));
        std::fs::write(&path, "changed elsewhere").unwrap();
        assert!(!app.save_tab(0));
        assert_eq!(app.pending, Some(Pending::ChangedOnDisk(app.tabs[0].id)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed elsewhere");
        app.pending = None;
        app.resolve_changed_on_disk(0, dialogs::ChangedOnDiskChoice::Overwrite);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xalpha");
        std::fs::write(&path, "third version").unwrap();
        app.resolve_changed_on_disk(0, dialogs::ChangedOnDiskChoice::Reload);
        assert_eq!(app.tabs[0].buffer.text(), "third version");
        assert!(!app.tabs[0].buffer.is_modified());
    }

    #[test]
    fn vim_write_refuses_a_changed_file_unless_forced() {
        let dir = temp_dir("vim-changed");
        let path = dir.join("a.txt");
        std::fs::write(&path, "alpha").unwrap();
        let mut app = test_app(vec![path.clone()], FakeDialogs::default());
        app.apply_to_active(EditCommand::InsertText("x".into()));
        std::fs::write(&path, "changed elsewhere").unwrap();
        assert!(!app.vim_write(false));
        assert!(app.vim_message.is_some());
        assert_eq!(
            app.pending, None,
            "vim reports in the command bar, no dialog"
        );
        assert!(app.vim_write(true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xalpha");
    }

    #[test]
    fn command_line_files_open_once_each() {
        let dir = temp_dir("open");
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "alpha").unwrap();
        let mut app = test_app(vec![a.clone(), b.clone()], FakeDialogs::default());
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tabs[0].buffer.text(), "alpha");
        assert_eq!(
            app.tabs[1].buffer.path(),
            Some(b.as_path()),
            "a missing file opens empty"
        );
        app.active = 1;
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        app.open_path(dir.join("sub").join("..").join("a.txt"));
        assert_eq!(app.active, 0, "the same file through another path");
        app.open_path(a);
        assert_eq!(
            app.tabs.len(),
            2,
            "an open file is activated, not opened again"
        );
        assert_eq!(app.active, 0);
    }

    #[test]
    fn open_dialog_replaces_the_blank_tab() {
        let dir = temp_dir("dialog");
        let files = vec![dir.join("x.txt"), dir.join("y.txt")];
        let dialogs = FakeDialogs {
            open: Some(files.clone()),
            save: None,
        };
        let mut app = test_app(Vec::new(), dialogs);
        assert_eq!(app.tabs.len(), 1);
        app.dispatch(&egui::Context::default(), Action::OpenFile);
        let paths: Vec<_> = app.tabs.iter().map(|t| t.buffer.path().unwrap()).collect();
        assert_eq!(
            paths,
            files.iter().map(PathBuf::as_path).collect::<Vec<_>>()
        );
        assert_eq!(app.active, 1);
    }

    #[test]
    fn saving_an_untitled_tab_asks_for_a_path() {
        let path = temp_dir("save").join("new.txt");
        let dialogs = FakeDialogs {
            open: None,
            save: Some(path.clone()),
        };
        let mut app = test_app(Vec::new(), dialogs);
        app.apply_to_active(EditCommand::InsertText("hello".into()));
        app.dispatch(&egui::Context::default(), Action::Save);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert_eq!(app.tabs[0].buffer.path(), Some(path.as_path()));
        assert!(!app.tabs[0].buffer.is_modified());
        assert!(app.session_dirty, "the open files changed");
    }

    #[test]
    fn cancelled_save_keeps_the_changes() {
        let mut app = test_app(Vec::new(), FakeDialogs::default());
        app.apply_to_active(EditCommand::InsertText("draft".into()));
        assert_eq!(app.save_tab_outcome(0), SaveOutcome::Cancelled);
        assert!(app.tabs[0].buffer.is_modified());
        assert!(app.tabs[0].buffer.path().is_none());
    }

    #[test]
    fn code_theme_follows_the_ui_mode() {
        let settings = Settings {
            code_theme_dark: "dark".into(),
            code_theme_light: "light".into(),
            ..Settings::default()
        };
        assert_eq!(code_theme(&settings, true), "dark");
        assert_eq!(code_theme(&settings, false), "light");
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
}
