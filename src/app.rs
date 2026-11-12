//! Application shell: tabs, file dialogs, shortcuts, status bar and session handling.

mod dialogs;
mod font_manager;
mod menu;
mod spell_menu;
mod status;
mod tab_bar;
mod vim;

use std::path::{Path, PathBuf};
use std::sync::Arc;
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
        let fonts = FontManager::new(&cc.egui_ctx, &settings);
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
            assets: AssetsHandle::spawn(cc.egui_ctx.clone()),
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
        };
        app.vim_applied = app.settings.vim_mode;
        egui_extras::install_image_loaders(&cc.egui_ctx);
        app.apply_theme(&cc.egui_ctx);
        app.ensure_spell(&cc.egui_ctx);
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
        let open = self
            .tabs
            .iter()
            .position(|t| t.buffer.path().is_some_and(|p| same_file(p, &path)));
        if let Some(idx) = open {
            self.activate(idx);
            return Ok(());
        }
        if self.loads.iter().any(|job| same_file(job.path(), &path)) {
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
        let outcome = SaveOutcome::from_result(tab.buffer.save_as(path));
        // The tab may have a new path.
        self.session_dirty |= outcome == SaveOutcome::Saved;
        outcome
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
            let theme = match ui.visuals().dark_mode {
                true => &self.settings.code_theme_dark,
                false => &self.settings.code_theme_light,
            };
            let env = EditorEnv {
                assets: self.loaded_assets.as_ref(),
                theme,
                checker,
                matches: &matches,
            };
            let edit = opts.edit;
            let mut frame_ended = false;
            let mut route_to_vim = |ev: &egui::Event, tab: &mut Tab| {
                vim_key_seen = true;
                vim::route_event(ev, tab, &edit, &mut vim_effects, &mut frame_ended)
            };
            let key_handler: Option<&mut KeyHandler<'_>> = match vim_mode {
                true => Some(&mut route_to_vim),
                false => None,
            };
            let out = tab.show_editor(ui, &opts, env, key_handler);
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
