//! The settings window: editor, appearance, vim, spell check, session and key bindings,
//! including live capture of a new key chord.

use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key};

use crate::config::{Settings, UiTheme};
use crate::fonts::FontCatalog;
use crate::keymap::{Action, KeyChord, Keymap, chord_has_modifier};
use crate::spell::SpellStatus;

/// Window title.
const WINDOW_TITLE: &str = "Settings";
/// Default window size in logical pixels.
const DEFAULT_SIZE: [f32; 2] = [760.0, 560.0];
/// Width of the section list on the left of the window.
const NAV_WIDTH: f32 = 170.0;
/// Inner margin of the section list frame, in logical pixels.
const NAV_MARGIN: i8 = 4;
/// Font size range offered by the settings window, in points.
const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 6.0..=72.0;
/// Tab width range offered by the settings window, in columns.
const TAB_WIDTH_RANGE: std::ops::RangeInclusive<usize> = 1..=16;
/// Height of the font family list inside its combo box.
const FONT_LIST_HEIGHT: f32 = 300.0;
/// Width of the font family and code theme combo boxes.
const COMBO_WIDTH: f32 = 260.0;
/// Combo box text for "no custom font".
const DEFAULT_FONT_LABEL: &str = "Default (Hack)";
/// Combo entry that selects the built-in proportional font for the preview.
const PREVIEW_DEFAULT_FONT_LABEL: &str = "Default";
/// Button text while waiting for a key chord.
const CAPTURE_PROMPT: &str = "Press keys... (Esc to cancel)";
/// Chord column text of an action without a binding.
const UNBOUND_LABEL: &str = "unbound";
/// Inline message after capturing a chord without Ctrl/Cmd/Alt that is not a function key.
pub const NEEDS_MODIFIER_MESSAGE: &str = "Shortcut needs a modifier key";
/// Delay between a live (dragged) setting change and its save to disk.
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Something the user changed in the settings window, for the app to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsEvent {
    /// Some setting changed: save the settings file.
    Changed,
    /// A dragged value changed: apply it now, save it later (see `SaveDebounce`).
    ChangedLive,
    /// The user stopped dragging or left a dragged value: save it now if a save is pending.
    CommitLive,
    /// The editor font family changed: load and apply it.
    FontChanged,
    /// The preview font family changed: load and apply it.
    PreviewFontChanged,
    /// A key binding changed.
    KeymapChanged,
    /// The user asked to download the spelling dictionaries again.
    RetryDictionaries,
}

/// A page of the settings window, chosen in the section list on the left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsSection {
    /// Font, indentation, line numbers and word wrap.
    #[default]
    Editor,
    /// UI theme and code themes.
    Appearance,
    /// Markdown preview font.
    Preview,
    /// Vim mode.
    Vim,
    /// Spell checking.
    Spell,
    /// Session restore.
    Session,
    /// Key bindings.
    Keys,
}

impl SettingsSection {
    /// Every section, in list order.
    pub const ALL: &'static [SettingsSection] = &[
        SettingsSection::Editor,
        SettingsSection::Appearance,
        SettingsSection::Preview,
        SettingsSection::Vim,
        SettingsSection::Spell,
        SettingsSection::Session,
        SettingsSection::Keys,
    ];

    /// Name shown in the section list.
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Editor => "Editor",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Preview => "Markdown preview",
            SettingsSection::Vim => "Vim",
            SettingsSection::Spell => "Spell check",
            SettingsSection::Session => "Session",
            SettingsSection::Keys => "Key bindings",
        }
    }
}

/// Draws a font family combo box ("default" plus the scanned families whose lowercase name
/// contains `filter`, only monospaced ones when `monospace_only`). Returns true when the
/// selection in `current` changed.
fn family_combo(
    ui: &mut egui::Ui,
    id: &str,
    current: &mut Option<String>,
    default_label: &str,
    families: Option<crate::fonts::FamilyList>,
    filter: &str,
    monospace_only: bool,
) -> bool {
    let selected = current.clone().unwrap_or_else(|| default_label.to_string());
    let mut choice: Option<Option<String>> = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(COMBO_WIDTH)
        .height(FONT_LIST_HEIGHT)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(current.is_none(), default_label)
                .clicked()
            {
                choice = Some(None);
            }
            let Some(families) = &families else {
                ui.label("scanning fonts...");
                return;
            };
            let visible = families
                .iter()
                .filter(|f| (f.monospace || !monospace_only) && f.lower.contains(filter));
            for family in visible {
                let is_current = current.as_deref() == Some(family.name.as_str());
                if ui.selectable_label(is_current, &family.name).clicked() {
                    choice = Some(Some(family.name.clone()));
                }
            }
        });
    let Some(choice) = choice else { return false };
    if choice == *current {
        return false;
    }
    *current = choice;
    true
}

/// Outcome of a key capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureResult {
    /// The user pressed this chord.
    Chord(KeyChord),
    /// The user pressed Escape: keep the old binding.
    Cancel,
}

/// True for keys that are only modifiers or locks and never form a chord on their own.
fn is_bare_modifier(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
            | Key::CapsLock
            | Key::NumLock
            | Key::ScrollLock
    )
}

/// Events for a dragged value widget: `changed` applies the value live, `committed` (drag
/// stopped or focus lost) saves it.
fn drag_value_events(changed: bool, committed: bool) -> Vec<SettingsEvent> {
    let mut events = Vec::new();
    if changed {
        events.push(SettingsEvent::ChangedLive);
    }
    if committed {
        events.push(SettingsEvent::CommitLive);
    }
    events
}

/// Deadline of a deferred settings save: live changes save at most once per `SAVE_DEBOUNCE`.
#[derive(Debug, Default)]
pub struct SaveDebounce {
    /// When the pending save is due; `None` when nothing is pending.
    due: Option<Instant>,
}

impl SaveDebounce {
    /// Schedules a save `SAVE_DEBOUNCE` after `now`, unless one is already pending (so a long
    /// drag still saves every `SAVE_DEBOUNCE`).
    pub fn schedule(&mut self, now: Instant) {
        if self.due.is_some() {
            return;
        }
        self.due = Some(now + SAVE_DEBOUNCE);
    }

    /// True while a save is pending.
    pub fn is_pending(&self) -> bool {
        self.due.is_some()
    }

    /// Drops the pending save (the settings were just saved).
    pub fn cancel(&mut self) {
        self.due = None;
    }

    /// Returns true and clears the deadline when the pending save is due at `now`.
    pub fn take_due(&mut self, now: Instant) -> bool {
        let Some(due) = self.due else { return false };
        if now < due {
            return false;
        }
        self.due = None;
        true
    }

    /// Time left until the pending save is due, or `None` when nothing is pending.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.due.map(|due| due.saturating_duration_since(now))
    }
}

/// Returns the chord of the first key press in `events` that is not a bare modifier, or
/// `Cancel` when that press is Escape without modifiers. `None` when nothing was pressed.
pub fn capture_chord(events: &[Event]) -> Option<CaptureResult> {
    events.iter().find_map(|event| match event {
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } if !is_bare_modifier(*key) => Some(match (*key, modifiers.is_none()) {
            (Key::Escape, true) => CaptureResult::Cancel,
            _ => CaptureResult::Chord(KeyChord::from_egui(*key, *modifiers)),
        }),
        _ => None,
    })
}

/// Text of the spell checking status line.
fn spell_status_text(status: &SpellStatus) -> String {
    match status {
        SpellStatus::Disabled => "off".to_string(),
        SpellStatus::Loading => "loading dictionaries...".to_string(),
        SpellStatus::Downloading => "downloading dictionaries...".to_string(),
        SpellStatus::Ready => "ready".to_string(),
        SpellStatus::Failed(message) => message.clone(),
    }
}

/// State of the settings window.
pub struct SettingsWindow {
    /// True while the window is shown.
    pub open: bool,
    /// Action waiting for a new chord, while capturing.
    capture: Option<Action>,
    /// True when the last captured chord was rejected for lacking a modifier.
    capture_rejected: bool,
    /// A captured chord already used by another action: (action, chord, other action).
    pending_conflict: Option<(Action, KeyChord, Action)>,
    /// Text filter of the font family list.
    font_filter: String,
    /// Show only monospaced font families.
    monospace_only: bool,
    /// Text filter of the preview font family list.
    preview_font_filter: String,
    /// Layer of the window as last drawn, to tell whether it owns the keyboard focus.
    layer: Option<egui::LayerId>,
    /// Section shown on the right of the window.
    section: SettingsSection,
}

impl Default for SettingsWindow {
    /// A closed window listing monospaced fonts only.
    fn default() -> Self {
        SettingsWindow {
            open: false,
            capture: None,
            capture_rejected: false,
            pending_conflict: None,
            font_filter: String::new(),
            monospace_only: true,
            preview_font_filter: String::new(),
            layer: None,
            section: SettingsSection::default(),
        }
    }
}

impl SettingsWindow {
    /// Creates a closed settings window.
    pub fn new() -> SettingsWindow {
        SettingsWindow::default()
    }

    /// Opens the window when closed, closes it when open.
    pub fn toggle(&mut self) {
        match self.open {
            true => self.close(),
            false => self.open = true,
        }
    }

    /// Closes the window, dropping any capture or pending conflict.
    pub fn close(&mut self) {
        self.open = false;
        self.capture = None;
        self.capture_rejected = false;
        self.pending_conflict = None;
    }

    /// Shows `section`. Leaving the key bindings drops any capture or pending conflict, so
    /// no hidden capture keeps swallowing keys.
    fn select_section(&mut self, section: SettingsSection) {
        if section == self.section {
            return;
        }
        self.section = section;
        self.stop_capture();
        self.pending_conflict = None;
    }

    /// True while the window waits for a key chord.
    pub fn is_capturing(&self) -> bool {
        self.open && self.capture.is_some()
    }

    /// True when the keyboard focus is on a widget of this window.
    pub fn has_focus(&self, ctx: &egui::Context) -> bool {
        self.open
            && ctx
                .memory(|m| m.focused())
                .is_some_and(|id| self.owns_widget(ctx, id))
    }

    /// True when the widget `id` was drawn in this window last frame.
    fn owns_widget(&self, ctx: &egui::Context, id: egui::Id) -> bool {
        let Some(layer) = self.layer else {
            return false;
        };
        ctx.read_response(id).is_some_and(|r| r.layer_id == layer)
    }

    /// While capturing, consumes this frame's key and text events (so neither shortcuts nor
    /// widgets see them) and binds the captured chord. Returns `None` when not capturing, so
    /// the app dispatches shortcuts as usual; `Some(events)` means skip dispatch this frame.
    pub fn capture_keys(
        &mut self,
        ctx: &egui::Context,
        keymap: &mut Keymap,
    ) -> Option<Vec<SettingsEvent>> {
        if !self.is_capturing() {
            return None;
        }
        let action = self.capture?;
        let result = ctx.input_mut(|i| {
            let result = capture_chord(&i.events);
            i.events.retain(|e| {
                !matches!(
                    e,
                    Event::Key { .. } | Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_)
                )
            });
            result
        });
        let Some(result) = result else {
            return Some(Vec::new());
        };
        Some(self.finish_capture(action, result, keymap))
    }

    /// Ends the capture for `action` with `result`. A chord without a modifier (see
    /// `chord_has_modifier`) is rejected with an inline message and the capture goes on.
    fn finish_capture(
        &mut self,
        action: Action,
        result: CaptureResult,
        keymap: &mut Keymap,
    ) -> Vec<SettingsEvent> {
        let chord = match result {
            CaptureResult::Chord(chord) if !chord_has_modifier(&chord) => {
                self.capture_rejected = true;
                return Vec::new();
            }
            CaptureResult::Chord(chord) => chord,
            CaptureResult::Cancel => {
                self.stop_capture();
                return Vec::new();
            }
        };
        self.stop_capture();
        self.bind(action, chord, keymap)
    }

    /// Leaves capture mode and clears its message.
    fn stop_capture(&mut self) {
        self.capture = None;
        self.capture_rejected = false;
    }

    /// Binds `chord` to `action`, or records a conflict when another action uses it.
    fn bind(&mut self, action: Action, chord: KeyChord, keymap: &mut Keymap) -> Vec<SettingsEvent> {
        if keymap.chord(action) == Some(chord) {
            return Vec::new();
        }
        match keymap.set(action, chord) {
            Ok(()) => vec![SettingsEvent::KeymapChanged, SettingsEvent::Changed],
            Err(other) => {
                self.pending_conflict = Some((action, chord, other));
                Vec::new()
            }
        }
    }

    /// Draws the window (when open) and returns what the user changed. Escape closes it
    /// when the keyboard focus is in the window or nowhere.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        settings: &mut Settings,
        keymap: &mut Keymap,
        fonts: &FontCatalog,
        theme_names: &[String],
        spell: &SpellStatus,
    ) -> Vec<SettingsEvent> {
        if !self.open {
            return Vec::new();
        }
        let focus = ctx.memory(|m| m.focused());
        let mut events = Vec::new();
        let mut open = true;
        let shown = egui::Window::new(WINDOW_TITLE)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(DEFAULT_SIZE)
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    self.section_list(ui);
                    ui.vertical(|ui| {
                        ui.heading(self.section.label());
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| match self.section {
                                SettingsSection::Editor => {
                                    self.editor_section(ui, settings, fonts, &mut events)
                                }
                                SettingsSection::Appearance => {
                                    self.appearance_section(ui, settings, theme_names, &mut events)
                                }
                                SettingsSection::Preview => {
                                    self.preview_section(ui, settings, fonts, &mut events)
                                }
                                SettingsSection::Vim => self.vim_section(ui, settings, &mut events),
                                SettingsSection::Spell => {
                                    self.spell_section(ui, settings, spell, &mut events)
                                }
                                SettingsSection::Session => {
                                    self.session_section(ui, settings, &mut events)
                                }
                                SettingsSection::Keys => self.keys_section(ui, keymap, &mut events),
                            });
                    });
                });
            });
        if let Some(shown) = &shown {
            self.layer = Some(shown.response.layer_id);
        }
        let escape = ctx.input(|i| i.key_pressed(Key::Escape));
        if escape && focus.is_none_or(|id| self.owns_widget(ctx, id)) {
            open = false;
        }
        if !open {
            self.close();
        }
        events
    }

    /// The section list on the left: one full-width entry per section, the current one selected.
    fn section_list(&mut self, ui: &mut egui::Ui) {
        let mut clicked = None;
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(NAV_MARGIN)
            .show(ui, |ui| {
                ui.set_width(NAV_WIDTH);
                ui.set_min_height(ui.available_height());
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                    for section in SettingsSection::ALL {
                        let selected = *section == self.section;
                        if ui.selectable_label(selected, section.label()).clicked() {
                            clicked = Some(*section);
                        }
                    }
                });
            });
        let Some(section) = clicked else { return };
        self.select_section(section);
    }

    /// Font family, size, indentation, line numbers and word wrap.
    fn editor_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        fonts: &FontCatalog,
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::Grid::new("fed_settings_editor")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Font family");
                self.font_combo(ui, settings, fonts, events);
                ui.end_row();

                ui.label("");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.font_filter)
                            .hint_text("filter fonts")
                            .desired_width(140.0),
                    );
                    ui.checkbox(&mut self.monospace_only, "monospace only");
                });
                ui.end_row();

                ui.label("Font size");
                let size = egui::DragValue::new(&mut settings.font_size)
                    .range(FONT_SIZE_RANGE)
                    .speed(0.5);
                drag_changed(ui.add(size), events);
                ui.end_row();

                ui.label("Tab width");
                let width = egui::DragValue::new(&mut settings.tab_width)
                    .range(TAB_WIDTH_RANGE)
                    .speed(0.1);
                drag_changed(ui.add(width), events);
                ui.end_row();
            });
        changed(
            ui.checkbox(&mut settings.insert_spaces, "Insert spaces instead of tabs"),
            events,
        );
        changed(
            ui.checkbox(&mut settings.line_numbers, "Show line numbers"),
            events,
        );
        changed(ui.checkbox(&mut settings.word_wrap, "Word wrap"), events);
    }

    /// The editor font family combo box: "Default" plus the scanned families matching the filter.
    fn font_combo(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        fonts: &FontCatalog,
        events: &mut Vec<SettingsEvent>,
    ) {
        let filter = self.font_filter.to_lowercase();
        let changed = family_combo(
            ui,
            "fed_settings_font",
            &mut settings.font_family,
            DEFAULT_FONT_LABEL,
            fonts.families(),
            &filter,
            self.monospace_only,
        );
        if changed {
            events.extend([SettingsEvent::FontChanged, SettingsEvent::Changed]);
        }
    }

    /// Font family and size of the Markdown preview.
    fn preview_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        fonts: &FontCatalog,
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::Grid::new("fed_settings_preview")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Font family");
                let filter = self.preview_font_filter.to_lowercase();
                let changed = family_combo(
                    ui,
                    "fed_settings_preview_font",
                    &mut settings.preview_font_family,
                    PREVIEW_DEFAULT_FONT_LABEL,
                    fonts.families(),
                    &filter,
                    false,
                );
                if changed {
                    events.extend([SettingsEvent::PreviewFontChanged, SettingsEvent::Changed]);
                }
                ui.end_row();

                ui.label("");
                ui.add(
                    egui::TextEdit::singleline(&mut self.preview_font_filter)
                        .hint_text("filter fonts")
                        .desired_width(140.0),
                );
                ui.end_row();

                ui.label("Font size");
                let size = egui::DragValue::new(&mut settings.preview_font_size)
                    .range(FONT_SIZE_RANGE)
                    .speed(0.5);
                drag_changed(ui.add(size), events);
                ui.end_row();
            });
    }

    /// UI theme and the syntax themes for light and dark mode.
    fn appearance_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        theme_names: &[String],
        events: &mut Vec<SettingsEvent>,
    ) {
        ui.horizontal(|ui| {
            ui.label("UI theme");
            for (theme, label) in [
                (UiTheme::System, "System"),
                (UiTheme::Light, "Light"),
                (UiTheme::Dark, "Dark"),
            ] {
                changed(ui.radio_value(&mut settings.ui_theme, theme, label), events);
            }
        });
        ui.add_enabled_ui(!theme_names.is_empty(), |ui| {
            egui::Grid::new("fed_settings_themes")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Code theme (light)");
                    theme_combo(
                        ui,
                        "fed_theme_light",
                        &mut settings.code_theme_light,
                        theme_names,
                        events,
                    );
                    ui.end_row();
                    ui.label("Code theme (dark)");
                    theme_combo(
                        ui,
                        "fed_theme_dark",
                        &mut settings.code_theme_dark,
                        theme_names,
                        events,
                    );
                    ui.end_row();
                });
        });
    }

    /// Vim mode switch.
    fn vim_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        events: &mut Vec<SettingsEvent>,
    ) {
        changed(
            ui.checkbox(&mut settings.vim_mode, "Enable vim keybindings"),
            events,
        );
    }

    /// Spell check switch, status and the dictionary download button.
    fn spell_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        spell: &SpellStatus,
        events: &mut Vec<SettingsEvent>,
    ) {
        changed(
            ui.checkbox(
                &mut settings.spell_check,
                "Check spelling (Italian, English)",
            ),
            events,
        );
        ui.horizontal(|ui| {
            ui.label("Status:");
            ui.label(spell_status_text(spell));
        });
        if matches!(spell, SpellStatus::Failed(_)) && ui.button("Download dictionaries").clicked() {
            events.push(SettingsEvent::RetryDictionaries);
        }
    }

    /// Session restore switch.
    fn session_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        events: &mut Vec<SettingsEvent>,
    ) {
        changed(
            ui.checkbox(
                &mut settings.restore_session,
                "Reopen files from the last session",
            ),
            events,
        );
    }

    /// The key binding grid with capture buttons, the conflict prompt and the reset button.
    fn keys_section(
        &mut self,
        ui: &mut egui::Ui,
        keymap: &mut Keymap,
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::Grid::new("fed_settings_keys")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for action in Action::ALL {
                    self.key_row(ui, *action, keymap, events);
                }
            });
        if ui.button("Reset to defaults").clicked() {
            keymap.reset();
            self.stop_capture();
            self.pending_conflict = None;
            events.extend([SettingsEvent::KeymapChanged, SettingsEvent::Changed]);
        }
    }

    /// One key binding row, followed by the conflict prompt when it concerns `action`.
    fn key_row(
        &mut self,
        ui: &mut egui::Ui,
        action: Action,
        keymap: &mut Keymap,
        events: &mut Vec<SettingsEvent>,
    ) {
        ui.label(action.label());
        let chord = keymap
            .chord(action)
            .map_or_else(|| UNBOUND_LABEL.to_string(), |c| c.to_string());
        ui.monospace(chord);
        let capturing = self.capture == Some(action);
        if capturing {
            if ui.button(CAPTURE_PROMPT).clicked() {
                self.stop_capture();
            }
        } else if ui.button("Change").clicked() {
            self.capture = Some(action);
            self.capture_rejected = false;
            self.pending_conflict = None;
        }
        ui.end_row();
        if capturing && self.capture_rejected {
            ui.label("");
            ui.colored_label(ui.visuals().warn_fg_color, NEEDS_MODIFIER_MESSAGE);
            ui.end_row();
        }
        let Some((conflict_action, chord, other)) = self.pending_conflict else {
            return;
        };
        if conflict_action != action {
            return;
        }
        ui.label("");
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("Already used by {}", other.label()),
        );
        ui.horizontal(|ui| {
            if ui.button("Replace").clicked() {
                keymap.force_set(action, chord);
                self.pending_conflict = None;
                events.extend([SettingsEvent::KeymapChanged, SettingsEvent::Changed]);
            }
            if ui.button("Cancel").clicked() {
                self.pending_conflict = None;
            }
        });
        ui.end_row();
    }
}

/// Records a `Changed` event when the widget `response` changed its value.
fn changed(response: egui::Response, events: &mut Vec<SettingsEvent>) {
    if response.changed() {
        events.push(SettingsEvent::Changed);
    }
}

/// Records the events of a dragged value widget (see `drag_value_events`).
fn drag_changed(response: egui::Response, events: &mut Vec<SettingsEvent>) {
    let committed = response.drag_stopped() || response.lost_focus();
    events.extend(drag_value_events(response.changed(), committed));
}

/// A combo box choosing one of `names` for `value`.
fn theme_combo(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut String,
    names: &[String],
    events: &mut Vec<SettingsEvent>,
) {
    let mut choice = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.as_str())
        .width(COMBO_WIDTH)
        .show_ui(ui, |ui| {
            for name in names {
                if ui.selectable_label(name == value, name).clicked() {
                    choice = Some(name.clone());
                }
            }
        });
    let Some(choice) = choice else { return };
    if choice == *value {
        return;
    }
    *value = choice;
    events.push(SettingsEvent::Changed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key press event.
    fn press(key: Key, modifiers: egui::Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn capture_ignores_bare_modifiers() {
        let ctrl = egui::Modifiers::COMMAND;
        let events = vec![
            press(Key::ControlLeft, ctrl),
            press(Key::ShiftLeft, ctrl),
            Event::Key {
                key: Key::A,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: ctrl,
            },
        ];
        assert_eq!(capture_chord(&events), None);
        let mut events = events;
        events.push(press(Key::K, ctrl));
        let chord = KeyChord::from_egui(Key::K, ctrl);
        assert_eq!(capture_chord(&events), Some(CaptureResult::Chord(chord)));
    }

    #[test]
    fn capture_escape_cancels() {
        let events = vec![
            press(Key::Escape, egui::Modifiers::NONE),
            press(Key::K, egui::Modifiers::NONE),
        ];
        assert_eq!(capture_chord(&events), Some(CaptureResult::Cancel));
        let shift_esc = vec![press(Key::Escape, egui::Modifiers::SHIFT)];
        assert!(matches!(
            capture_chord(&shift_esc),
            Some(CaptureResult::Chord(c)) if c.key == Key::Escape && c.shift
        ));
    }

    #[test]
    fn capture_returns_chord_with_modifiers() {
        let mods = egui::Modifiers {
            ctrl: true,
            command: true,
            shift: true,
            ..Default::default()
        };
        let Some(CaptureResult::Chord(chord)) = capture_chord(&[press(Key::K, mods)]) else {
            panic!("expected a chord");
        };
        assert!(chord.command && chord.shift && !chord.alt);
        assert_eq!(chord.key, Key::K);
        if !cfg!(target_os = "macos") {
            assert!(!chord.ctrl, "off macOS Ctrl is folded into command");
            assert_eq!(chord.to_string(), "Ctrl+Shift+K");
        }
    }

    #[test]
    fn capture_rejects_chord_without_modifier_and_keeps_capturing() {
        let mut window = SettingsWindow::new();
        window.open = true;
        window.capture = Some(Action::NewTab);
        let mut keymap = Keymap::defaults();
        let before = keymap.chord(Action::NewTab);
        let bare = CaptureResult::Chord(KeyChord::parse("Shift+K").unwrap());
        assert!(
            window
                .finish_capture(Action::NewTab, bare, &mut keymap)
                .is_empty()
        );
        assert!(window.capture_rejected);
        assert!(window.is_capturing(), "the user can try another chord");
        assert_eq!(keymap.chord(Action::NewTab), before);

        let f9 = KeyChord::parse("F9").unwrap();
        assert_eq!(
            window.finish_capture(Action::NewTab, CaptureResult::Chord(f9), &mut keymap),
            vec![SettingsEvent::KeymapChanged, SettingsEvent::Changed]
        );
        assert!(!window.capture_rejected);
        assert!(!window.is_capturing());
        assert_eq!(keymap.chord(Action::NewTab), Some(f9));
    }

    #[test]
    fn drag_values_apply_live_and_save_on_commit() {
        assert!(drag_value_events(false, false).is_empty());
        assert_eq!(
            drag_value_events(true, false),
            vec![SettingsEvent::ChangedLive]
        );
        assert_eq!(
            drag_value_events(true, true),
            vec![SettingsEvent::ChangedLive, SettingsEvent::CommitLive]
        );
    }

    #[test]
    fn save_debounce_saves_at_most_once_per_interval() {
        let start = Instant::now();
        let mut debounce = SaveDebounce::default();
        assert!(!debounce.take_due(start));
        assert_eq!(debounce.remaining(start), None);
        debounce.schedule(start);
        // Later changes do not push the deadline back.
        debounce.schedule(start + Duration::from_millis(400));
        assert!(debounce.is_pending());
        assert_eq!(
            debounce.remaining(start + Duration::from_millis(100)),
            Some(Duration::from_millis(400))
        );
        assert!(!debounce.take_due(start + Duration::from_millis(499)));
        assert!(debounce.take_due(start + SAVE_DEBOUNCE));
        assert!(!debounce.is_pending());
        debounce.schedule(start);
        debounce.cancel();
        assert!(!debounce.take_due(start + SAVE_DEBOUNCE));
    }

    #[test]
    fn leaving_key_bindings_stops_capture() {
        let mut window = SettingsWindow::new();
        window.open = true;
        window.select_section(SettingsSection::Keys);
        window.capture = Some(Action::NewTab);
        window.select_section(SettingsSection::Keys);
        assert!(window.is_capturing(), "same section keeps the capture");
        window.select_section(SettingsSection::Editor);
        assert!(!window.is_capturing());
        assert_eq!(window.section, SettingsSection::Editor);
    }

    #[test]
    fn capture_binds_or_reports_conflicts() {
        let mut window = SettingsWindow::new();
        let mut keymap = Keymap::defaults();
        let save = keymap.chord(Action::Save).unwrap();
        assert!(window.bind(Action::NewTab, save, &mut keymap).is_empty());
        assert_eq!(
            window.pending_conflict,
            Some((Action::NewTab, save, Action::Save))
        );
        assert_eq!(
            keymap.chord(Action::Save),
            Some(save),
            "nothing rebound yet"
        );
        let free = KeyChord::parse("Ctrl+Alt+F9").unwrap();
        assert_eq!(
            window.bind(Action::NewTab, free, &mut keymap),
            vec![SettingsEvent::KeymapChanged, SettingsEvent::Changed]
        );
        assert_eq!(keymap.chord(Action::NewTab), Some(free));
        assert!(window.bind(Action::NewTab, free, &mut keymap).is_empty());
    }
}
