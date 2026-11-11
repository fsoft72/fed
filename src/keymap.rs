//! Keyboard actions, key chords and the configurable keymap.

use std::collections::BTreeMap;
use std::fmt;

use eframe::egui::{Key, Modifiers};

/// True when running on macOS, where `Cmd` and `Ctrl` are different keys.
const IS_MAC: bool = cfg!(target_os = "macos");

/// Config string meaning "this action has no key binding".
const UNBOUND: &str = "";

/// A user-triggerable editor action that can be bound to a key chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// Open a new empty tab.
    NewTab,
    /// Show the open-file dialog.
    OpenFile,
    /// Save the current file.
    Save,
    /// Save the current file under a new name.
    SaveAs,
    /// Close the current tab.
    CloseTab,
    /// Switch to the next tab.
    NextTab,
    /// Switch to the previous tab.
    PrevTab,
    /// Open the settings window.
    OpenSettings,
    /// Open the find bar.
    Find,
    /// Open the find and replace bar.
    Replace,
    /// Open the go-to-line bar.
    GoToLine,
    /// Toggle the markdown preview.
    TogglePreview,
    /// Undo the last edit.
    Undo,
    /// Redo the last undone edit.
    Redo,
    /// Select the whole buffer.
    SelectAll,
}

impl Action {
    /// Every action, in display order.
    pub const ALL: &'static [Action] = &[
        Action::NewTab,
        Action::OpenFile,
        Action::Save,
        Action::SaveAs,
        Action::CloseTab,
        Action::NextTab,
        Action::PrevTab,
        Action::OpenSettings,
        Action::Find,
        Action::Replace,
        Action::GoToLine,
        Action::TogglePreview,
        Action::Undo,
        Action::Redo,
        Action::SelectAll,
    ];

    /// Stable snake_case name used as the key in `settings.toml`.
    pub fn name(&self) -> &'static str {
        match self {
            Action::NewTab => "new_tab",
            Action::OpenFile => "open_file",
            Action::Save => "save",
            Action::SaveAs => "save_as",
            Action::CloseTab => "close_tab",
            Action::NextTab => "next_tab",
            Action::PrevTab => "prev_tab",
            Action::OpenSettings => "open_settings",
            Action::Find => "find",
            Action::Replace => "replace",
            Action::GoToLine => "go_to_line",
            Action::TogglePreview => "toggle_preview",
            Action::Undo => "undo",
            Action::Redo => "redo",
            Action::SelectAll => "select_all",
        }
    }

    /// Human-readable label for the settings UI.
    pub fn label(&self) -> &'static str {
        match self {
            Action::NewTab => "New tab",
            Action::OpenFile => "Open file",
            Action::Save => "Save",
            Action::SaveAs => "Save as",
            Action::CloseTab => "Close tab",
            Action::NextTab => "Next tab",
            Action::PrevTab => "Previous tab",
            Action::OpenSettings => "Open settings",
            Action::Find => "Find",
            Action::Replace => "Replace",
            Action::GoToLine => "Go to line",
            Action::TogglePreview => "Toggle preview",
            Action::Undo => "Undo",
            Action::Redo => "Redo",
            Action::SelectAll => "Select all",
        }
    }

    /// Looks an action up by its `name()`.
    pub fn from_name(name: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|a| a.name() == name)
    }

    /// Default chord string for this action (`Cmd` means the platform command key).
    fn default_chord(&self) -> &'static str {
        match self {
            Action::NewTab => "Cmd+N",
            Action::OpenFile => "Cmd+O",
            Action::Save => "Cmd+S",
            Action::SaveAs => "Cmd+Shift+S",
            Action::CloseTab => "Cmd+W",
            Action::NextTab => "Ctrl+Tab",
            Action::PrevTab => "Ctrl+Shift+Tab",
            Action::OpenSettings => "Cmd+,",
            Action::Find => "Cmd+F",
            Action::Replace => "Cmd+H",
            Action::GoToLine => "Cmd+G",
            Action::TogglePreview => "Cmd+Shift+V",
            Action::Undo => "Cmd+Z",
            Action::Redo => "Cmd+Y",
            Action::SelectAll => "Cmd+A",
        }
    }
}

/// A key plus modifiers. `command` is Cmd on macOS and Ctrl elsewhere; `ctrl` is the
/// literal Ctrl key and is only ever set on macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyChord {
    /// Platform command modifier (Cmd on macOS, Ctrl elsewhere).
    pub command: bool,
    /// Literal Ctrl key (distinct from `command` only on macOS).
    pub ctrl: bool,
    /// Alt / Option.
    pub alt: bool,
    /// Shift.
    pub shift: bool,
    /// The main key.
    pub key: Key,
}

impl KeyChord {
    /// Parses text like `"Ctrl+Shift+S"`, `"Cmd+,"` or `"F3"` for the current platform.
    pub fn parse(s: &str) -> Result<KeyChord, String> {
        KeyChord::parse_for(s, IS_MAC)
    }

    /// Parses a chord for an explicit platform.
    fn parse_for(s: &str, mac: bool) -> Result<KeyChord, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty key chord".to_string());
        }
        // A trailing "+" is the Plus key itself (e.g. "Ctrl++").
        let (mod_part, key_part) = match s.strip_suffix('+') {
            Some("") => ("", "+"),
            Some(head) => (head.strip_suffix('+').unwrap_or(head), "+"),
            None => match s.rsplit_once('+') {
                Some((m, k)) => (m, k),
                None => ("", s),
            },
        };
        let mut chord = KeyChord {
            command: false,
            ctrl: false,
            alt: false,
            shift: false,
            key: Key::A,
        };
        if !mod_part.is_empty() {
            for m in mod_part.split('+') {
                match m.trim().to_ascii_lowercase().as_str() {
                    "cmd" | "command" => chord.command = true,
                    "ctrl" | "control" if mac => chord.ctrl = true,
                    "ctrl" | "control" => chord.command = true,
                    "alt" | "option" => chord.alt = true,
                    "shift" => chord.shift = true,
                    other => return Err(format!("unknown modifier '{other}' in '{s}'")),
                }
            }
        }
        chord.key = Key::from_name(key_part.trim())
            .ok_or_else(|| format!("unknown key '{key_part}' in '{s}'"))?;
        Ok(chord)
    }

    /// Builds a chord from an egui key event for the current platform.
    pub fn from_egui(key: Key, mods: Modifiers) -> KeyChord {
        KeyChord::from_egui_for(key, mods, IS_MAC)
    }

    /// Builds a chord from an egui key event for an explicit platform.
    fn from_egui_for(key: Key, mods: Modifiers, mac: bool) -> KeyChord {
        KeyChord {
            command: mods.command,
            // Off macOS egui folds Ctrl into `command`, so a separate ctrl would be a duplicate.
            ctrl: mods.ctrl && mac,
            alt: mods.alt,
            shift: mods.shift,
            key,
        }
    }

    /// Formats the chord with the platform's modifier names.
    fn display_for(&self, mac: bool) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.command {
            parts.push(if mac { "Cmd" } else { "Ctrl" });
        }
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        parts.push(self.key.name());
        parts.join("+")
    }
}

impl fmt::Display for KeyChord {
    /// Prints the chord using the current platform's modifier names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_for(IS_MAC))
    }
}

/// Maps actions to key chords; an action without an entry is unbound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    bindings: BTreeMap<Action, KeyChord>,
}

impl Keymap {
    /// The factory default bindings for the current platform.
    pub fn defaults() -> Keymap {
        Keymap::defaults_for(IS_MAC)
    }

    /// The factory default bindings for an explicit platform.
    fn defaults_for(mac: bool) -> Keymap {
        let bindings = Action::ALL
            .iter()
            .map(|a| {
                let chord =
                    KeyChord::parse_for(a.default_chord(), mac).expect("default chords are valid");
                (*a, chord)
            })
            .collect();
        Keymap { bindings }
    }

    /// Builds a keymap from settings overrides. Bad names, bad chords and chords bound to
    /// two actions produce a warning and fall back to the default (or unbound for duplicates).
    /// An empty chord string means the action is unbound.
    pub fn from_overrides(map: &BTreeMap<String, String>) -> (Keymap, Vec<String>) {
        let mut keymap = Keymap::defaults();
        let mut warnings = Vec::new();
        for (name, text) in map {
            let Some(action) = Action::from_name(name) else {
                warnings.push(format!("keybinding '{name}' ignored: unknown action"));
                continue;
            };
            if text == UNBOUND {
                keymap.bindings.remove(&action);
                continue;
            }
            match KeyChord::parse(text) {
                Ok(chord) => {
                    keymap.bindings.insert(action, chord);
                }
                Err(e) => warnings.push(format!("keybinding '{name}' ignored: {e}")),
            }
        }
        keymap.drop_duplicates(&mut warnings);
        (keymap, warnings)
    }

    /// Unbinds every action whose chord is already taken by an earlier action.
    fn drop_duplicates(&mut self, warnings: &mut Vec<String>) {
        let mut seen: BTreeMap<String, Action> = BTreeMap::new();
        for action in Action::ALL {
            let Some(chord) = self.bindings.get(action).copied() else {
                continue;
            };
            let Some(first) = seen.insert(chord.to_string(), *action) else {
                continue;
            };
            warnings.push(format!(
                "keybinding '{}' ignored: {chord} is already used by '{}'",
                action.name(),
                first.name()
            ));
            self.bindings.remove(action);
        }
    }

    /// The overrides needed to recreate this keymap from the defaults; unbound actions are
    /// stored as an empty string.
    pub fn to_overrides(&self) -> BTreeMap<String, String> {
        let defaults = Keymap::defaults();
        Action::ALL
            .iter()
            .filter(|a| self.bindings.get(a) != defaults.bindings.get(a))
            .map(|a| {
                let text = self
                    .bindings
                    .get(a)
                    .map(KeyChord::to_string)
                    .unwrap_or_default();
                (a.name().to_string(), text)
            })
            .collect()
    }

    /// The chord bound to `a`, or `None` when unbound.
    pub fn chord(&self, a: Action) -> Option<KeyChord> {
        self.bindings.get(&a).copied()
    }

    /// The action bound to `c`, if any.
    pub fn lookup(&self, c: &KeyChord) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(_, bound)| *bound == c)
            .map(|(a, _)| *a)
    }

    /// Binds `c` to `a`. Fails with the conflicting action if another action already uses `c`.
    pub fn set(&mut self, a: Action, c: KeyChord) -> Result<(), Action> {
        match self.lookup(&c) {
            Some(other) if other != a => Err(other),
            _ => {
                self.bindings.insert(a, c);
                Ok(())
            }
        }
    }

    /// Binds `c` to `a`, unbinding any other action that used `c`.
    pub fn force_set(&mut self, a: Action, c: KeyChord) {
        self.bindings.retain(|_, bound| *bound != c);
        self.bindings.insert(a, c);
    }

    /// Restores the default bindings.
    pub fn reset(&mut self) {
        *self = Keymap::defaults();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_roundtrip() {
        for mac in [false, true] {
            for s in ["Ctrl+Shift+S", "Cmd+,", "Ctrl+Tab", "F3", "Alt+Enter"] {
                let c = KeyChord::parse_for(s, mac).unwrap();
                let shown = c.display_for(mac);
                assert_eq!(
                    KeyChord::parse_for(&shown, mac).unwrap(),
                    c,
                    "{s} on mac={mac}"
                );
            }
        }
        assert_eq!(
            KeyChord::parse_for("Ctrl+S", false)
                .unwrap()
                .display_for(false),
            "Ctrl+S"
        );
        assert_eq!(
            KeyChord::parse_for("Cmd+S", true)
                .unwrap()
                .display_for(true),
            "Cmd+S"
        );
        assert!(KeyChord::parse_for("Ctrl+Bogus", false).is_err());
        assert!(KeyChord::parse_for("", false).is_err());
    }

    #[test]
    fn parse_modifier_semantics_per_platform() {
        let c = KeyChord::parse_for("Ctrl+S", false).unwrap();
        assert!(c.command && !c.ctrl);
        let c = KeyChord::parse_for("Cmd+S", false).unwrap();
        assert!(c.command && !c.ctrl);
        let c = KeyChord::parse_for("Ctrl+S", true).unwrap();
        assert!(!c.command && c.ctrl);
        let c = KeyChord::parse_for("Cmd+S", true).unwrap();
        assert!(c.command && !c.ctrl);
    }

    #[test]
    fn parse_comma_key() {
        let c = KeyChord::parse_for("Cmd+,", false).unwrap();
        assert_eq!(c.key, eframe::egui::Key::Comma);
        assert!(c.command);
    }

    #[test]
    fn from_egui_maps_modifiers() {
        let mods = eframe::egui::Modifiers {
            ctrl: true,
            command: true,
            shift: true,
            ..Default::default()
        };
        let c = KeyChord::from_egui_for(eframe::egui::Key::S, mods, false);
        assert!(c.command && !c.ctrl && c.shift && !c.alt);
        let mods = eframe::egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let c = KeyChord::from_egui_for(eframe::egui::Key::S, mods, true);
        assert!(c.ctrl && !c.command);
    }

    #[test]
    fn lookup_default_save() {
        let km = Keymap::defaults();
        let chord = KeyChord::parse("Cmd+S").unwrap();
        assert_eq!(km.lookup(&chord), Some(Action::Save));
        assert_eq!(km.chord(Action::Save), Some(chord));
    }

    #[test]
    fn defaults_have_no_conflicts() {
        let km = Keymap::defaults();
        for a in Action::ALL {
            let c = km.chord(*a).unwrap();
            assert_eq!(km.lookup(&c), Some(*a));
        }
    }

    #[test]
    fn action_names_roundtrip() {
        for a in Action::ALL {
            assert_eq!(Action::from_name(a.name()), Some(*a));
            assert!(!a.label().is_empty());
        }
        assert_eq!(Action::from_name("nope"), None);
    }

    #[test]
    fn set_conflict_returns_other_action() {
        let mut km = Keymap::defaults();
        let save_chord = km.chord(Action::Save).unwrap();
        assert_eq!(km.set(Action::Find, save_chord), Err(Action::Save));
        assert_eq!(km.chord(Action::Save), Some(save_chord));
        assert_ne!(km.chord(Action::Find), Some(save_chord));
        let free = KeyChord::parse_for("Ctrl+Alt+F", false).unwrap();
        assert_eq!(km.set(Action::Find, free), Ok(()));
        assert_eq!(km.chord(Action::Find), Some(free));
        // Re-setting an action to its own chord is not a conflict.
        assert_eq!(km.set(Action::Find, free), Ok(()));
    }

    #[test]
    fn force_set_unbinds_other() {
        let mut km = Keymap::defaults();
        let save_chord = km.chord(Action::Save).unwrap();
        km.force_set(Action::Find, save_chord);
        assert_eq!(km.chord(Action::Find), Some(save_chord));
        assert_eq!(km.chord(Action::Save), None);
        assert_eq!(km.lookup(&save_chord), Some(Action::Find));
    }

    #[test]
    fn overrides_only_store_differences() {
        let mut km = Keymap::defaults();
        assert!(km.to_overrides().is_empty());
        let free = KeyChord::parse_for("Ctrl+Alt+F", false).unwrap();
        km.set(Action::Find, free).unwrap();
        let o = km.to_overrides();
        assert_eq!(o.len(), 1);
        assert_eq!(
            o.get("find").map(String::as_str),
            Some(free.to_string().as_str())
        );
        let (back, warnings) = Keymap::from_overrides(&o);
        assert!(warnings.is_empty());
        assert_eq!(back.chord(Action::Find), Some(free));
    }

    #[test]
    fn unbound_action_roundtrips_through_overrides() {
        let mut km = Keymap::defaults();
        let save_chord = km.chord(Action::Save).unwrap();
        km.force_set(Action::Find, save_chord);
        let o = km.to_overrides();
        let (back, warnings) = Keymap::from_overrides(&o);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back.chord(Action::Save), None);
        assert_eq!(back.chord(Action::Find), Some(save_chord));
    }

    #[test]
    fn bad_override_warns_and_keeps_default() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("save".to_string(), "Ctrl+Bogus".to_string());
        m.insert("nonexistent".to_string(), "Ctrl+Q".to_string());
        let (km, warnings) = Keymap::from_overrides(&m);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("save")));
        assert!(warnings.iter().any(|w| w.contains("nonexistent")));
        assert_eq!(
            km.chord(Action::Save),
            Keymap::defaults().chord(Action::Save)
        );
    }

    #[test]
    fn reset_restores_defaults() {
        let mut km = Keymap::defaults();
        km.force_set(Action::Find, km.chord(Action::Save).unwrap());
        km.reset();
        assert_eq!(km.to_overrides().len(), 0);
    }
}
