//! Keyboard input of the editor widget: this frame's key, text, clipboard and IME events
//! become editing commands (or go to the vim key handler).

use eframe::egui::{self, Key, Modifiers, Ui};

use super::{KeyHandler, ViewOptions, ViewOutput};
use crate::editing::EditCommand;
use crate::tab::Tab;

/// Translates this frame's keyboard events into commands on `tab`.
pub(super) fn handle_keyboard(
    ui: &Ui,
    tab: &mut Tab,
    opts: &ViewOptions,
    page: usize,
    key_handler: &mut Option<&mut KeyHandler<'_>>,
    out: &mut ViewOutput,
) {
    // Only the keyboard events are copied (none at all in most frames).
    let events = ui.input(|i| keyboard_events(&i.events));
    for ev in &events {
        if let Some(up) = page_key(ev) {
            tab.view.page_anchor = Some(tab.view.caret_offset.unwrap_or(0.0));
            if let Some(handler) = key_handler.as_mut() {
                // Vim mode: move the caret one line at a time so every mode clamps it.
                let arrow = if up { Key::ArrowUp } else { Key::ArrowDown };
                for _ in 0..page {
                    handler(&key_event(arrow), tab);
                }
                continue;
            }
        }
        if let Some(handler) = key_handler.as_mut()
            && handler(ev, tab)
        {
            tab.scroll_to_cursor = true;
            continue;
        }
        let Some(cmd) = event_to_command(ev, page) else {
            continue;
        };
        if let Some(text) = tab.apply(cmd, &opts.edit) {
            out.clipboard = Some(text);
        }
    }
}

/// Returns `Some(true)` for a PageUp press and `Some(false)` for a PageDown press (Shift
/// allowed, to extend the selection), `None` for any other event.
fn page_key(ev: &egui::Event) -> Option<bool> {
    let egui::Event::Key {
        key,
        pressed: true,
        modifiers,
        ..
    } = ev
    else {
        return None;
    };
    if modifiers.ctrl || modifiers.alt || modifiers.mac_cmd {
        return None;
    }
    match key {
        Key::PageUp => Some(true),
        Key::PageDown => Some(false),
        _ => None,
    }
}

/// A plain press of `key` without modifiers.
pub(super) fn key_event(key: Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}

/// Copies the events of `events` the editor consumes from the keyboard.
fn keyboard_events(events: &[egui::Event]) -> Vec<egui::Event> {
    events
        .iter()
        .filter(|e| is_keyboard_event(e))
        .cloned()
        .collect()
}

/// True for events the editor (or its key handler) consumes from the keyboard.
fn is_keyboard_event(ev: &egui::Event) -> bool {
    matches!(
        ev,
        egui::Event::Text(_)
            | egui::Event::Ime(egui::ImeEvent::Commit(_))
            | egui::Event::Copy
            | egui::Event::Cut
            | egui::Event::Paste(_)
            | egui::Event::Key { pressed: true, .. }
    )
}

/// Maps one keyboard event to an editing command; `page` is the PageUp/PageDown size.
fn event_to_command(ev: &egui::Event, page: usize) -> Option<EditCommand> {
    match ev {
        egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
            let text: String = text.chars().filter(|c| !c.is_control()).collect();
            (!text.is_empty()).then_some(EditCommand::InsertText(text))
        }
        egui::Event::Copy => Some(EditCommand::Copy),
        egui::Event::Cut => Some(EditCommand::Cut),
        egui::Event::Paste(text) => (!text.is_empty()).then(|| EditCommand::Paste(text.clone())),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => key_to_command(*key, *modifiers, page),
        _ => None,
    }
}

/// Maps a key press to an editing command using the platform conventions
/// (`Cmd`/`Alt` arrows on macOS, `Ctrl` arrows elsewhere). `page` is the page size in lines.
pub fn key_to_command(key: Key, mods: Modifiers, page: usize) -> Option<EditCommand> {
    key_to_command_for(key, mods, page, cfg!(target_os = "macos"))
}

/// `key_to_command` with an explicit platform flag (testable on every OS).
fn key_to_command_for(key: Key, mods: Modifiers, page: usize, mac: bool) -> Option<EditCommand> {
    let select = mods.shift;
    let word = if mac { mods.alt } else { mods.ctrl };
    let cmd_arrow = mac && mods.mac_cmd;
    let doc_jump = if mac { mods.mac_cmd } else { mods.ctrl };
    let plain = !mods.ctrl && !mods.alt && !mods.mac_cmd;
    let cmd = match key {
        Key::ArrowLeft if cmd_arrow => EditCommand::Home { select },
        Key::ArrowLeft if word => EditCommand::WordLeft { select },
        Key::ArrowLeft => EditCommand::Left { select },
        Key::ArrowRight if cmd_arrow => EditCommand::End { select },
        Key::ArrowRight if word => EditCommand::WordRight { select },
        Key::ArrowRight => EditCommand::Right { select },
        Key::ArrowUp if cmd_arrow => EditCommand::DocStart { select },
        Key::ArrowUp => EditCommand::Up { select },
        Key::ArrowDown if cmd_arrow => EditCommand::DocEnd { select },
        Key::ArrowDown => EditCommand::Down { select },
        Key::Home if doc_jump => EditCommand::DocStart { select },
        Key::Home => EditCommand::Home { select },
        Key::End if doc_jump => EditCommand::DocEnd { select },
        Key::End => EditCommand::End { select },
        Key::PageUp => EditCommand::PageUp {
            select,
            lines: page,
        },
        Key::PageDown => EditCommand::PageDown {
            select,
            lines: page,
        },
        Key::Enter if plain => EditCommand::Newline,
        Key::Backspace => EditCommand::Backspace,
        Key::Delete => EditCommand::Delete,
        Key::Tab if plain && mods.shift => EditCommand::Outdent,
        Key::Tab if plain => EditCommand::Indent,
        _ => return None,
    };
    Some(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Pos2;

    const NONE: Modifiers = Modifiers::NONE;
    const SHIFT: Modifiers = Modifiers::SHIFT;
    const CTRL: Modifiers = Modifiers::CTRL;

    #[test]
    fn arrows_map_to_moves() {
        assert_eq!(
            key_to_command_for(Key::ArrowLeft, NONE, 10, false),
            Some(EditCommand::Left { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowRight, SHIFT, 10, false),
            Some(EditCommand::Right { select: true })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowLeft, CTRL, 10, false),
            Some(EditCommand::WordLeft { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowRight, CTRL | SHIFT, 10, false),
            Some(EditCommand::WordRight { select: true })
        );
    }

    #[test]
    fn mac_uses_alt_for_words_and_cmd_for_lines() {
        assert_eq!(
            key_to_command_for(Key::ArrowLeft, Modifiers::ALT, 10, true),
            Some(EditCommand::WordLeft { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowRight, Modifiers::MAC_CMD, 10, true),
            Some(EditCommand::End { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::ArrowUp, Modifiers::MAC_CMD, 10, true),
            Some(EditCommand::DocStart { select: false })
        );
    }

    #[test]
    fn home_end_and_doc_jumps() {
        assert_eq!(
            key_to_command_for(Key::Home, NONE, 10, false),
            Some(EditCommand::Home { select: false })
        );
        assert_eq!(
            key_to_command_for(Key::End, CTRL | SHIFT, 10, false),
            Some(EditCommand::DocEnd { select: true })
        );
    }

    #[test]
    fn page_keys_carry_page_size() {
        assert_eq!(
            key_to_command_for(Key::PageDown, SHIFT, 7, false),
            Some(EditCommand::PageDown {
                select: true,
                lines: 7
            })
        );
    }

    #[test]
    fn editing_keys() {
        assert_eq!(
            key_to_command_for(Key::Enter, NONE, 1, false),
            Some(EditCommand::Newline)
        );
        assert_eq!(key_to_command_for(Key::Enter, CTRL, 1, false), None);
        assert_eq!(
            key_to_command_for(Key::Tab, NONE, 1, false),
            Some(EditCommand::Indent)
        );
        assert_eq!(
            key_to_command_for(Key::Tab, SHIFT, 1, false),
            Some(EditCommand::Outdent)
        );
        assert_eq!(key_to_command_for(Key::Tab, CTRL, 1, false), None);
        assert_eq!(
            key_to_command_for(Key::Backspace, NONE, 1, false),
            Some(EditCommand::Backspace)
        );
        assert_eq!(key_to_command_for(Key::A, CTRL, 1, false), None);
    }

    #[test]
    fn text_events_become_inserts() {
        let ev = egui::Event::Text("é".into());
        assert_eq!(
            event_to_command(&ev, 1),
            Some(EditCommand::InsertText("é".into()))
        );
        let ime = egui::Event::Ime(egui::ImeEvent::Commit("ö".into()));
        assert_eq!(
            event_to_command(&ime, 1),
            Some(EditCommand::InsertText("ö".into()))
        );
        assert_eq!(
            event_to_command(&egui::Event::Text("\u{8}".into()), 1),
            None
        );
        assert_eq!(
            event_to_command(&egui::Event::Paste("x".into()), 1),
            Some(EditCommand::Paste("x".into()))
        );
    }

    #[test]
    fn only_keyboard_events_are_copied() {
        let events = vec![
            egui::Event::PointerMoved(Pos2::ZERO),
            egui::Event::Text("a".into()),
            egui::Event::Copy,
            egui::Event::Key {
                key: Key::A,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: NONE,
            },
        ];
        let kept = keyboard_events(&events);
        assert_eq!(kept, vec![egui::Event::Text("a".into()), egui::Event::Copy]);
        assert!(keyboard_events(&[]).is_empty());
    }

    #[test]
    fn page_key_ignores_modified_presses() {
        assert_eq!(page_key(&key_event(Key::PageUp)), Some(true));
        assert_eq!(page_key(&key_event(Key::PageDown)), Some(false));
        let ctrl = egui::Event::Key {
            key: Key::PageDown,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: CTRL,
        };
        assert_eq!(page_key(&ctrl), None);
        assert_eq!(page_key(&key_event(Key::ArrowDown)), None);
    }
}
