//! Unsaved-changes confirmations: closing tabs and the window, and their modal dialogs.

use eframe::egui;

use super::{FedApp, close_requires_confirmation, tab_path, tabs_needing_confirmation};
use crate::tab::Tab;

/// Id of the close-tab confirmation modal.
const CLOSE_TAB_MODAL_ID: &str = "fed_close_tab_modal";
/// Id of the close-window confirmation modal.
const CLOSE_WINDOW_MODAL_ID: &str = "fed_close_window_modal";

/// A confirmation the application is waiting for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pending {
    /// Closing the tab with this id (ids stay valid when other tabs close).
    CloseTab(u64),
    /// Closing the window while the tabs listed in `names` (paths, computed once when the
    /// dialog opens) have unsaved changes.
    CloseWindow { names: Vec<String> },
}

/// Answer to the close-tab confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseTabChoice {
    /// Save the tab, then close it.
    Save,
    /// Close the tab dropping its changes.
    Discard,
    /// Keep the tab open.
    Cancel,
}

/// Answer to the close-window confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseWindowChoice {
    /// Save every modified tab, then close the window.
    SaveAll,
    /// Close the window dropping all changes.
    DiscardAll,
    /// Keep the window open.
    Cancel,
}

/// Space between the message and the buttons of a confirmation modal.
const MODAL_BUTTON_GAP: f32 = 8.0;
/// Title of the unsaved-changes modals.
const UNSAVED_TITLE: &str = "Unsaved changes";

/// Shows a modal with `title`, the content drawn by `body` and one button per `buttons`
/// entry. Returns the value of the clicked button in the frame it is clicked; Escape or a
/// click outside the modal returns `cancel`.
fn confirm_modal<T: Copy>(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    body: impl FnOnce(&mut egui::Ui),
    buttons: &[(&str, T)],
    cancel: T,
) -> Option<T> {
    let modal = egui::Modal::new(egui::Id::unique(id)).show(ctx, |ui| {
        ui.heading(title);
        body(ui);
        ui.add_space(MODAL_BUTTON_GAP);
        let mut choice = None;
        ui.horizontal(|ui| {
            for (label, value) in buttons {
                if ui.button(*label).clicked() {
                    choice = Some(*value);
                }
            }
        });
        choice
    });
    if modal.inner.is_none() && modal.should_close() {
        return Some(cancel);
    }
    modal.inner
}

/// Shows the "unsaved changes" modal for the tab named `name`. Returns the user's answer in
/// the frame it is given; Escape or a click outside the modal counts as Cancel.
pub(crate) fn confirm_close_tab(ctx: &egui::Context, name: &str) -> Option<CloseTabChoice> {
    let body = |ui: &mut egui::Ui| {
        ui.label(format!("Save changes to \"{name}\" before closing?"));
    };
    let buttons = [
        ("Save", CloseTabChoice::Save),
        ("Discard", CloseTabChoice::Discard),
        ("Cancel", CloseTabChoice::Cancel),
    ];
    confirm_modal(
        ctx,
        CLOSE_TAB_MODAL_ID,
        UNSAVED_TITLE,
        body,
        &buttons,
        CloseTabChoice::Cancel,
    )
}

/// Shows the "unsaved changes" modal listing the modified files `names`. Returns the user's
/// answer in the frame it is given; Escape or a click outside the modal counts as Cancel.
pub(crate) fn confirm_close_window(
    ctx: &egui::Context,
    names: &[String],
) -> Option<CloseWindowChoice> {
    let body = |ui: &mut egui::Ui| {
        ui.label("These files have unsaved changes:");
        for name in names {
            ui.label(format!("  {name}"));
        }
    };
    let buttons = [
        ("Save all", CloseWindowChoice::SaveAll),
        ("Discard all", CloseWindowChoice::DiscardAll),
        ("Cancel", CloseWindowChoice::Cancel),
    ];
    let cancel = CloseWindowChoice::Cancel;
    confirm_modal(
        ctx,
        CLOSE_WINDOW_MODAL_ID,
        UNSAVED_TITLE,
        body,
        &buttons,
        cancel,
    )
}

impl FedApp {
    /// Saves every modified tab; stops and returns false at the first failure or cancel.
    fn save_all_modified(&mut self) -> bool {
        tabs_needing_confirmation(&self.tabs)
            .into_iter()
            .all(|idx| self.save_tab(idx))
    }

    /// Closes the tab at `idx`, asking first when it has unsaved changes.
    pub(super) fn request_close_tab(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        tab.end_typing();
        if close_requires_confirmation(tab) {
            self.pending = Some(Pending::CloseTab(tab.id));
            return;
        }
        self.remove_tab(ctx, idx);
    }

    /// Removes the tab at `idx` without asking. Closing the last tab closes the window.
    pub(super) fn remove_tab(&mut self, ctx: &egui::Context, idx: usize) {
        if self.tabs.len() <= 1 {
            self.close_window(ctx);
            return;
        }
        self.tabs.remove(idx);
        self.session_dirty = true;
        if self.active > idx || self.active >= self.tabs.len() {
            self.active -= 1;
        }
        self.tabs[self.active].focus_editor();
    }

    /// Closes the window without further confirmation.
    fn close_window(&mut self, ctx: &egui::Context) {
        self.allow_close = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Intercepts a window close request while tabs have unsaved changes.
    pub(super) fn handle_close_request(&mut self, ctx: &egui::Context) {
        if self.allow_close || !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        self.tabs.iter_mut().for_each(Tab::end_typing);
        if tabs_needing_confirmation(&self.tabs).is_empty() {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        let names = tabs_needing_confirmation(&self.tabs)
            .into_iter()
            .map(|i| tab_path(&self.tabs[i]))
            .collect();
        self.pending = Some(Pending::CloseWindow { names });
    }

    /// Shows the pending confirmation dialog and carries out the answer.
    pub(super) fn show_dialogs(&mut self, ctx: &egui::Context) {
        match &self.pending {
            Some(Pending::CloseTab(id)) => self.close_tab_dialog(ctx, *id),
            Some(Pending::CloseWindow { .. }) => self.close_window_dialog(ctx),
            None => {}
        }
    }

    /// Runs the close-tab confirmation for the tab with `id`.
    fn close_tab_dialog(&mut self, ctx: &egui::Context, id: u64) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            self.pending = None;
            return;
        };
        let name = self.tabs[idx].buffer.display_name();
        let Some(choice) = confirm_close_tab(ctx, &name) else {
            return;
        };
        self.pending = None;
        self.tabs[self.active].focus_editor();
        match choice {
            CloseTabChoice::Save if self.save_tab(idx) => self.remove_tab(ctx, idx),
            CloseTabChoice::Discard => self.remove_tab(ctx, idx),
            CloseTabChoice::Save | CloseTabChoice::Cancel => {}
        }
    }

    /// Runs the close-window confirmation listing the modified files.
    fn close_window_dialog(&mut self, ctx: &egui::Context) {
        let Some(Pending::CloseWindow { names }) = &self.pending else {
            return;
        };
        let Some(choice) = confirm_close_window(ctx, names) else {
            return;
        };
        self.pending = None;
        self.tabs[self.active].focus_editor();
        match choice {
            CloseWindowChoice::SaveAll if self.save_all_modified() => self.close_window(ctx),
            CloseWindowChoice::DiscardAll => self.close_window(ctx),
            CloseWindowChoice::SaveAll | CloseWindowChoice::Cancel => {}
        }
    }
}
