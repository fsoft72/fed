//! The "hamburger" application menu at the far right of the tab bar, and the About dialog.

use eframe::egui;

use crate::keymap::{Action, Keymap};

/// Side of the square hamburger button.
pub(crate) const MENU_BUTTON_SIZE: f32 = 28.0;
/// Corner radius of the hamburger button.
const MENU_BUTTON_RADIUS: f32 = 4.0;
/// Width of the three hamburger lines.
const MENU_LINE_WIDTH: f32 = 14.0;
/// Vertical distance between two hamburger lines.
const MENU_LINE_GAP: f32 = 5.0;
/// Thickness of the hamburger lines.
const MENU_LINE_THICKNESS: f32 = 1.5;
/// Minimum width of the opened menu.
const MENU_MIN_WIDTH: f32 = 180.0;
/// Id of the About modal.
const ABOUT_MODAL_ID: &str = "fed_about_modal";
/// Program name and tagline shown in the About dialog.
const ABOUT_TITLE: &str = "fed - Slick and fast text editor";
/// Author's GitHub profile.
const AUTHOR_URL: &str = "https://github.com/fsoft72";
/// Project repository.
const REPO_URL: &str = "https://github.com/fsoft72/fed";

/// File actions offered at the top of the menu, in order.
const FILE_ITEMS: [Action; 5] = [
    Action::NewTab,
    Action::OpenFile,
    Action::CloseTab,
    Action::Save,
    Action::SaveAs,
];

/// An entry the user picked in the application menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuChoice {
    /// Run a regular editor action.
    Action(Action),
    /// Show the About dialog.
    About,
    /// Quit the application (asks first when files are unsaved).
    Quit,
}

/// Menu label of `action` (overrides the settings labels where the menu wants other words).
fn menu_label(action: Action) -> &'static str {
    match action {
        Action::CloseTab => "Close file",
        Action::SaveAs => "Save As",
        other => other.label(),
    }
}

/// Draws the hamburger button and, while open, its menu. Returns the entry clicked this frame.
pub(crate) fn hamburger_menu(ui: &mut egui::Ui, keymap: &Keymap) -> Option<MenuChoice> {
    let resp = draw_hamburger_button(ui);
    let mut choice = None;
    egui::Popup::menu(&resp).show(|ui| {
        ui.set_min_width(MENU_MIN_WIDTH);
        for action in FILE_ITEMS {
            let shortcut = keymap
                .chord(action)
                .map(|c| c.to_string())
                .unwrap_or_default();
            let button = egui::Button::new(menu_label(action)).shortcut_text(shortcut);
            if ui.add(button).clicked() {
                choice = Some(MenuChoice::Action(action));
            }
        }
        ui.separator();
        if ui.button("About").clicked() {
            choice = Some(MenuChoice::About);
        }
        ui.separator();
        if ui.button("Quit").clicked() {
            choice = Some(MenuChoice::Quit);
        }
    });
    choice
}

/// Paints the square three-line button; returns its response.
fn draw_hamburger_button(ui: &mut egui::Ui) -> egui::Response {
    let size = egui::vec2(MENU_BUTTON_SIZE, MENU_BUTTON_SIZE);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = ui.visuals();
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let (bg, color) = if open {
        (
            visuals.widgets.active.weak_bg_fill,
            visuals.strong_text_color(),
        )
    } else if resp.hovered() {
        (
            visuals.widgets.hovered.weak_bg_fill,
            visuals.strong_text_color(),
        )
    } else {
        (egui::Color32::TRANSPARENT, visuals.text_color())
    };
    let painter = ui.painter();
    painter.rect_filled(rect, MENU_BUTTON_RADIUS, bg);
    let stroke = egui::Stroke::new(MENU_LINE_THICKNESS, color);
    let half = MENU_LINE_WIDTH / 2.0;
    for step in [-1.0, 0.0, 1.0] {
        let y = rect.center().y + step * MENU_LINE_GAP;
        painter.line_segment(
            [
                egui::pos2(rect.center().x - half, y),
                egui::pos2(rect.center().x + half, y),
            ],
            stroke,
        );
    }
    resp.on_hover_text("Menu")
}

/// Shows the About dialog. Returns true when it should close (button, Escape or outside
/// click). The links emit open-URL commands that `FedApp::open_links` carries out.
pub(crate) fn show_about(ctx: &egui::Context) -> bool {
    let modal = egui::Modal::new(egui::Id::unique(ABOUT_MODAL_ID)).show(ctx, |ui| {
        ui.heading(ABOUT_TITLE);
        ui.label(format!("v {}", env!("CARGO_PKG_VERSION")));
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.label("by Fabio Rotondo (");
            ui.hyperlink_to(AUTHOR_URL, AUTHOR_URL);
            ui.label(")");
        });
        ui.label("and Claude AI");
        ui.add_space(8.0);
        ui.label("This software is MIT licensed");
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.label("Github: (");
            ui.hyperlink_to(REPO_URL, REPO_URL);
            ui.label(")");
        });
        ui.add_space(12.0);
        ui.vertical_centered(|ui| ui.button("Close").clicked())
            .inner
    });
    modal.inner || modal.should_close()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_labels_match_the_requested_wording() {
        let labels: Vec<&str> = FILE_ITEMS.iter().map(|&a| menu_label(a)).collect();
        assert_eq!(
            labels,
            ["New tab", "Open file", "Close file", "Save", "Save As"]
        );
    }

    #[test]
    fn about_links_can_be_opened() {
        assert!(crate::markdown::is_openable_url(AUTHOR_URL));
        assert!(crate::markdown::is_openable_url(REPO_URL));
    }
}
