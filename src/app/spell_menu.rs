//! Spell checking in the application: starting the dictionary service and the suggestions
//! popup opened by a right click on a misspelled word.

use eframe::egui;

use super::{FedApp, Severity};
use crate::spell::{SpellService, SpellStatus};

/// The spelling suggestions popup opened by a right click on a misspelled word.
pub(super) struct SpellMenu {
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

impl FedApp {
    /// Starts the spell checker service when spell checking is on and none runs yet.
    pub(super) fn ensure_spell(&mut self, ctx: &egui::Context) {
        if !self.settings.spell_check || self.spell.is_some() {
            return;
        }
        let Some(paths) = self.paths.clone() else {
            return;
        };
        self.spell = Some(SpellService::spawn(ctx.clone(), paths, true));
    }

    /// Returns the spell checking status (`Disabled` when off or not started).
    pub(super) fn spell_status(&self) -> SpellStatus {
        match &self.spell {
            Some(service) if self.settings.spell_check => service.status(),
            _ => SpellStatus::Disabled,
        }
    }

    /// Opens the suggestions popup for the misspelled word under a right click, if any.
    pub(super) fn open_spell_menu(&mut self, pos: egui::Pos2, clicked: std::ops::Range<usize>) {
        if !self.settings.spell_check {
            return;
        }
        let Some(checker) = self.spell.as_ref().and_then(SpellService::checker) else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(range) = tab.spell_mark_in(&clicked) else {
            return;
        };
        let word = tab.buffer.rope().slice(range.clone()).to_string();
        let suggestions = crate::sync::read(&checker).suggest(&word);
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
    pub(super) fn show_spell_menu(&mut self, ctx: &egui::Context) {
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
    pub(super) fn apply_spell_choice(&mut self, replacement: Option<String>) {
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
