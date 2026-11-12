//! Translation of egui keyboard events into vim keys.

use eframe::egui::{self, Key, Modifiers};

use super::VimKey;

/// Translates a keyboard event into vim keys: text gives one `Char` per char; Escape, Enter,
/// Backspace, Tab and the arrows map to their keys; Ctrl plus a letter gives `Ctrl`. Other
/// events give nothing.
pub fn egui_event_to_vim_keys(ev: &egui::Event) -> Vec<VimKey> {
    match ev {
        egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => text
            .chars()
            .filter(|c| !c.is_control())
            .map(VimKey::Char)
            .collect(),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => key_to_vim_key(*key, *modifiers).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Translates a keyboard event that stands for exactly one vim key (see
/// `egui_event_to_vim_keys`); `None` for other events and for multi-char text.
pub fn egui_event_to_vim_key(ev: &egui::Event) -> Option<VimKey> {
    let mut keys = egui_event_to_vim_keys(ev);
    if keys.len() != 1 {
        return None;
    }
    keys.pop()
}

/// Maps a key press to a vim key; printable keys without Ctrl arrive as text events instead.
fn key_to_vim_key(key: Key, mods: Modifiers) -> Option<VimKey> {
    if mods.ctrl {
        let name = key.name();
        let mut chars = name.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => {
                Some(VimKey::Ctrl(c.to_ascii_lowercase()))
            }
            _ => None,
        };
    }
    let vim_key = match key {
        Key::Escape => VimKey::Esc,
        Key::Enter => VimKey::Enter,
        Key::Backspace => VimKey::Backspace,
        Key::Tab if !mods.shift => VimKey::Tab,
        Key::ArrowLeft => VimKey::Left,
        Key::ArrowRight => VimKey::Right,
        Key::ArrowUp => VimKey::Up,
        Key::ArrowDown => VimKey::Down,
        _ => return None,
    };
    Some(vim_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;
    const CTRL: Modifiers = Modifiers::CTRL;

    #[test]
    fn events_map_to_vim_keys() {
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        assert_eq!(
            egui_event_to_vim_key(&egui::Event::Text("n".into())),
            Some(VimKey::Char('n'))
        );
        assert_eq!(
            egui_event_to_vim_keys(&egui::Event::Text("ab".into())),
            vec![VimKey::Char('a'), VimKey::Char('b')]
        );
        assert_eq!(egui_event_to_vim_key(&egui::Event::Text("ab".into())), None);
        assert_eq!(
            egui_event_to_vim_key(&key(Key::Escape, NONE)),
            Some(VimKey::Esc)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::Enter, NONE)),
            Some(VimKey::Enter)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::Backspace, NONE)),
            Some(VimKey::Backspace)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::Tab, NONE)),
            Some(VimKey::Tab)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::ArrowUp, NONE)),
            Some(VimKey::Up)
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::R, CTRL)),
            Some(VimKey::Ctrl('r'))
        );
        assert_eq!(
            egui_event_to_vim_key(&key(Key::N, NONE)),
            None,
            "text event follows"
        );
        assert_eq!(egui_event_to_vim_key(&egui::Event::Copy), None);
    }
}
