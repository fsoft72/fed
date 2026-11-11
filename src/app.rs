//! Application shell: tabs, file dialogs, shortcuts, status bar and session handling.

mod dialogs;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::buffer::{Buffer, LineEnding};
use crate::config::{self, ConfigPaths, Session, Settings, UiTheme};
use crate::editing::{EditCommand, EditSettings};
use crate::find_bar::{BarMode, FindBar, MatchDecorator};
use crate::highlight::{self, AssetsHandle, HighlightAssets, HighlightDecorator};
use crate::keymap::{Action, KeyChord, Keymap};
use crate::spell::{SpellDecorator, SpellMode, SpellService, SpellStatus};
use crate::tab::Tab;
use crate::view::{self, ViewOptions};
use dialogs::Pending;

/// How long a status bar message stays visible.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Status bar notice for tabs with highlighting disabled.
const LARGE_FILE_NOTICE: &str = "large file: highlighting off";
/// Status bar text while the dictionaries are downloading.
const SPELL_DOWNLOADING_NOTICE: &str = "downloading dictionaries...";
/// Name shown for a tab without a file path.
const UNTITLED: &str = "untitled";
/// Suffix of a tab label when the buffer has unsaved changes. Drawn with the monospace
/// font because the default proportional fonts have no glyph for it.
const MODIFIED_LABEL_SUFFIX: &str = " ●";
/// Suffix of the status text when the buffer has unsaved changes.
const MODIFIED_STATUS_SUFFIX: &str = "  [+]";
/// Fixed, non-configurable alias for Redo.
const REDO_ALIAS: &str = "Cmd+Shift+Z";

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
/// chord, or Redo for the fixed `REDO_ALIAS`.
fn shortcut_action(keymap: &Keymap, key: egui::Key, mods: egui::Modifiers) -> Option<Action> {
    let chord = KeyChord::from_egui(key, mods);
    if let Some(action) = keymap.lookup(&chord) {
        return Some(action);
    }
    let alias = KeyChord::parse(REDO_ALIAS).expect("the redo alias is a valid chord");
    (chord == alias).then_some(Action::Redo)
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

/// Something the user did in the tab bar.
enum TabBarAction {
    /// Switch to the tab at this index.
    Activate(usize),
    /// Close the tab at this index.
    Close(usize),
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
}

impl FedApp {
    /// Creates the application; `files` are the paths passed on the command line. Loads the
    /// settings, then opens one tab per file (or the previous session's files when `files` is
    /// empty and session restore is on), falling back to one empty tab.
    pub fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> FedApp {
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
        };
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

    /// Writes the current settings (including the keymap) to the config directory.
    pub fn save_settings(&mut self) {
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
            Action::OpenSettings | Action::TogglePreview => {
                self.set_status(
                    format!("{}: not available yet", action.label()),
                    Severity::Info,
                );
            }
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

    /// Builds the editor widget options from the current settings.
    fn view_options(&self) -> ViewOptions {
        ViewOptions {
            font: egui::FontId::monospace(self.settings.font_size),
            line_numbers: self.settings.line_numbers,
            word_wrap: self.settings.word_wrap,
            edit: self.edit_settings(),
            keys_enabled: self.pending.is_none(),
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
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        tab.focus_editor();
        self.active = idx;
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
        let Some(tab) = self.tabs.get_mut(idx) else {
            return false;
        };
        tab.end_typing();
        if tab.buffer.path().is_none() {
            return self.save_tab_as(idx);
        }
        let result = tab.buffer.save();
        self.report_save(idx, result)
    }

    /// Asks for a path and saves the tab at `idx` there. Returns true on success.
    fn save_tab_as(&mut self, idx: usize) -> bool {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return false;
        };
        tab.end_typing();
        let Some(path) = save_dialog(&tab.buffer) else {
            return false;
        };
        let result = tab.buffer.save_as(path);
        self.report_save(idx, result)
    }

    /// Shows the outcome of saving the tab at `idx`; returns true when it succeeded.
    fn report_save(&mut self, idx: usize, result: Result<(), crate::buffer::BufferError>) -> bool {
        match result {
            Ok(()) => {
                let path = tab_path(&self.tabs[idx]);
                self.set_status(format!("saved {path}"), Severity::Info);
                true
            }
            Err(e) => {
                self.set_status(e.to_string(), Severity::Error);
                false
            }
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
    /// chord) and dispatches their actions.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() {
            return;
        }
        let editor_focused = self.editor_has_focus(ctx);
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
                if needs_editor_focus(action) && !editor_focused {
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

    /// Draws the tab bar and returns what the user clicked.
    fn tab_bar(&self, ui: &mut egui::Ui) -> Option<TabBarAction> {
        let mut action = None;
        egui::Panel::top("fed_tabs").show(ui, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let mut label = egui::Atoms::new(tab.buffer.display_name());
                        if tab.buffer.is_modified() {
                            label
                                .push_right(egui::RichText::new(MODIFIED_LABEL_SUFFIX).monospace());
                        }
                        let resp = ui
                            .selectable_label(i == self.active, label)
                            .on_hover_text(tab_path(tab));
                        if resp.clicked() {
                            action = Some(TabBarAction::Activate(i));
                        }
                        if resp.middle_clicked() {
                            action = Some(TabBarAction::Close(i));
                        }
                        if ui.small_button("x").on_hover_text("Close tab").clicked() {
                            action = Some(TabBarAction::Close(i));
                        }
                        ui.separator();
                    }
                });
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
        let tab = &self.tabs[self.active];
        egui::Panel::bottom("fed_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(status_text(tab));
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
        let Some(checker) = self.spell.as_ref().and_then(SpellService::checker) else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(range) = tab
            .spell_marks
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
}

impl eframe::App for FedApp {
    /// Draws one frame: shortcuts first, then tab bar, status bar, editor and dialogs.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        self.expire_status(&ctx);
        self.handle_close_request(&ctx);
        self.ensure_spell(&ctx);
        self.handle_shortcuts(&ctx);
        match self.tab_bar(ui) {
            Some(TabBarAction::Activate(i)) => self.activate(i),
            Some(TabBarAction::Close(i)) => self.request_close_tab(&ctx, i),
            None => {}
        }
        self.status_bar(ui);
        self.find_bar_panel(ui);
        let opts = self.view_options();
        let checker = match self.settings.spell_check {
            true => self.spell.as_ref().and_then(SpellService::checker),
            false => None,
        };
        let mut context_click = None;
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
                tab.syntax = Some(highlight::syntax_for_path(a, tab.buffer.path()));
            }
            let theme = if ui.visuals().dark_mode {
                &self.settings.code_theme_dark
            } else {
                &self.settings.code_theme_light
            };
            let syntax = tab.syntax.clone().unwrap_or_default();
            // Decorator state is moved out of the tab so the decorators do not borrow it.
            let mut cache = std::mem::take(&mut tab.highlight);
            let mut markdown = tab.spell_md.take();
            let mut marks = std::mem::take(&mut tab.spell_marks);
            let large = tab.buffer.is_large();
            let mut highlighted =
                HighlightDecorator::new(&mut cache, assets, &syntax, theme, !large);
            let spell_checker = checker.filter(|_| !large);
            let out = match spell_checker {
                Some(c) => {
                    let mode = SpellMode::for_syntax(&syntax);
                    let mut spell =
                        SpellDecorator::new(&mut highlighted, c, mode, &mut markdown, &mut marks);
                    let mut deco = MatchDecorator::new(&mut spell, &matches);
                    view::show_editor(ui, tab, &opts, &mut deco, None)
                }
                None => {
                    let mut deco = MatchDecorator::new(&mut highlighted, &matches);
                    view::show_editor(ui, tab, &opts, &mut deco, None)
                }
            };
            tab.highlight = cache;
            tab.spell_md = markdown;
            tab.spell_marks = marks;
            if let Some(text) = out.clipboard {
                ui.ctx().copy_text(text);
            }
            if let Some((range, _)) = out.context_word {
                context_click = ui.input(|i| i.pointer.latest_pos()).map(|pos| (pos, range));
            }
        });
        if let Some((pos, range)) = context_click {
            self.open_spell_menu(pos, range);
        }
        self.show_spell_menu(&ctx);
        self.show_dialogs(&ctx);
    }

    /// Saves the session when the application exits.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
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
}
