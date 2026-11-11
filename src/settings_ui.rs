//! The settings window: editor, appearance, vim, spell check, session and key bindings,
//! including live capture of a new key chord.

use eframe::egui::{self, Event, Key};

use crate::config::{Settings, UiTheme};
use crate::fonts::FontCatalog;
use crate::keymap::{Action, KeyChord, Keymap};
use crate::spell::SpellStatus;

/// Window title.
const WINDOW_TITLE: &str = "Settings";
/// Default window size in logical pixels.
const DEFAULT_SIZE: [f32; 2] = [520.0, 600.0];
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
/// Button text while waiting for a key chord.
const CAPTURE_PROMPT: &str = "Press keys... (Esc to cancel)";
/// Chord column text of an action without a binding.
const UNBOUND_LABEL: &str = "unbound";

/// Something the user changed in the settings window, for the app to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsEvent {
    /// Some setting changed: save the settings file.
    Changed,
    /// The editor font family changed: load and apply it.
    FontChanged,
    /// A key binding changed.
    KeymapChanged,
    /// The user asked to download the spelling dictionaries again.
    RetryDictionaries,
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
    /// A captured chord already used by another action: (action, chord, other action).
    pending_conflict: Option<(Action, KeyChord, Action)>,
    /// Text filter of the font family list.
    font_filter: String,
    /// Show only monospaced font families.
    monospace_only: bool,
    /// Layer of the window as last drawn, to tell whether it owns the keyboard focus.
    layer: Option<egui::LayerId>,
}

impl Default for SettingsWindow {
    /// A closed window listing monospaced fonts only.
    fn default() -> Self {
        SettingsWindow {
            open: false,
            capture: None,
            pending_conflict: None,
            font_filter: String::new(),
            monospace_only: true,
            layer: None,
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
        self.capture = None;
        match result {
            CaptureResult::Cancel => Some(Vec::new()),
            CaptureResult::Chord(chord) => Some(self.bind(action, chord, keymap)),
        }
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
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.editor_section(ui, settings, fonts, &mut events);
                        self.appearance_section(ui, settings, theme_names, &mut events);
                        self.vim_section(ui, settings, &mut events);
                        self.spell_section(ui, settings, spell, &mut events);
                        self.session_section(ui, settings, &mut events);
                        self.keys_section(ui, keymap, &mut events);
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

    /// Font family, size, indentation, line numbers and word wrap.
    fn editor_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        fonts: &FontCatalog,
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::CollapsingHeader::new("Editor")
            .default_open(true)
            .show(ui, |ui| {
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
                        changed(ui.add(size), events);
                        ui.end_row();

                        ui.label("Tab width");
                        let width = egui::DragValue::new(&mut settings.tab_width)
                            .range(TAB_WIDTH_RANGE)
                            .speed(0.1);
                        changed(ui.add(width), events);
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
            });
    }

    /// The font family combo box: "Default" plus the scanned families matching the filter.
    fn font_combo(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        fonts: &FontCatalog,
        events: &mut Vec<SettingsEvent>,
    ) {
        let families = fonts.families();
        let selected = settings
            .font_family
            .clone()
            .unwrap_or_else(|| DEFAULT_FONT_LABEL.to_string());
        let filter = self.font_filter.to_lowercase();
        let mut choice: Option<Option<String>> = None;
        egui::ComboBox::from_id_salt("fed_settings_font")
            .selected_text(selected)
            .width(COMBO_WIDTH)
            .height(FONT_LIST_HEIGHT)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(settings.font_family.is_none(), DEFAULT_FONT_LABEL)
                    .clicked()
                {
                    choice = Some(None);
                }
                let Some(families) = &families else {
                    ui.label("scanning fonts...");
                    return;
                };
                let visible = families.iter().filter(|(name, mono)| {
                    (*mono || !self.monospace_only) && name.to_lowercase().contains(&filter)
                });
                for (name, _) in visible {
                    let current = settings.font_family.as_deref() == Some(name.as_str());
                    if ui.selectable_label(current, name).clicked() {
                        choice = Some(Some(name.clone()));
                    }
                }
            });
        let Some(choice) = choice else { return };
        if choice == settings.font_family {
            return;
        }
        settings.font_family = choice;
        events.extend([SettingsEvent::FontChanged, SettingsEvent::Changed]);
    }

    /// UI theme and the syntax themes for light and dark mode.
    fn appearance_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        theme_names: &[String],
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::CollapsingHeader::new("Appearance")
            .default_open(true)
            .show(ui, |ui| {
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
            });
    }

    /// Vim mode switch.
    fn vim_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::CollapsingHeader::new("Vim")
            .default_open(true)
            .show(ui, |ui| {
                changed(
                    ui.checkbox(&mut settings.vim_mode, "Enable vim keybindings"),
                    events,
                );
            });
    }

    /// Spell check switch, status and the dictionary download button.
    fn spell_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        spell: &SpellStatus,
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::CollapsingHeader::new("Spell check")
            .default_open(true)
            .show(ui, |ui| {
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
                if matches!(spell, SpellStatus::Failed(_))
                    && ui.button("Download dictionaries").clicked()
                {
                    events.push(SettingsEvent::RetryDictionaries);
                }
            });
    }

    /// Session restore switch.
    fn session_section(
        &mut self,
        ui: &mut egui::Ui,
        settings: &mut Settings,
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::CollapsingHeader::new("Session")
            .default_open(true)
            .show(ui, |ui| {
                changed(
                    ui.checkbox(
                        &mut settings.restore_session,
                        "Reopen files from the last session",
                    ),
                    events,
                );
            });
    }

    /// The key binding grid with capture buttons, the conflict prompt and the reset button.
    fn keys_section(
        &mut self,
        ui: &mut egui::Ui,
        keymap: &mut Keymap,
        events: &mut Vec<SettingsEvent>,
    ) {
        egui::CollapsingHeader::new("Key bindings")
            .default_open(true)
            .show(ui, |ui| {
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
                    self.capture = None;
                    self.pending_conflict = None;
                    events.extend([SettingsEvent::KeymapChanged, SettingsEvent::Changed]);
                }
            });
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
        if self.capture == Some(action) {
            if ui.button(CAPTURE_PROMPT).clicked() {
                self.capture = None;
            }
        } else if ui.button("Change").clicked() {
            self.capture = Some(action);
            self.pending_conflict = None;
        }
        ui.end_row();
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
