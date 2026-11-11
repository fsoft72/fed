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
/// Program name shown in the About dialog.
const ABOUT_NAME: &str = "fed";
/// Tagline under the program name.
const ABOUT_TAGLINE: &str = "Slick and fast text editor";
/// Author shown in the About dialog.
const AUTHOR_NAME: &str = "Fabio Rotondo";
/// Co-author shown in the About dialog.
const CO_AUTHOR: &str = "Claude AI";
/// License shown in the About dialog.
const LICENSE: &str = "MIT";
/// Short text of the repository link.
const REPO_LABEL: &str = "github.com/fsoft72/fed";
/// Content width of the About dialog.
const ABOUT_WIDTH: f32 = 300.0;
/// Inner margin of the About dialog.
const ABOUT_MARGIN: i8 = 24;
/// Corner radius of the About dialog.
const ABOUT_RADIUS: f32 = 12.0;
/// Side of the logo square.
const ABOUT_LOGO_SIZE: f32 = 72.0;
/// Corner radius of the logo square.
const ABOUT_LOGO_RADIUS: f32 = 16.0;
/// Size of the "f" in the logo.
const ABOUT_LOGO_FONT_SIZE: f32 = 42.0;
/// Size of the program name.
const ABOUT_NAME_SIZE: f32 = 28.0;
/// Left indent that roughly centers the details grid.
const ABOUT_DETAILS_INDENT: f32 = 50.0;
/// Minimum size of the Close button.
const ABOUT_BUTTON_SIZE: egui::Vec2 = egui::vec2(120.0, 30.0);
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
    let frame = egui::Frame::popup(&ctx.style_of(ctx.theme()))
        .inner_margin(egui::Margin::same(ABOUT_MARGIN))
        .corner_radius(ABOUT_RADIUS);
    let modal = egui::Modal::new(egui::Id::unique(ABOUT_MODAL_ID))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(ABOUT_WIDTH);
            ui.vertical_centered(|ui| {
                draw_logo(ui);
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new(ABOUT_NAME)
                        .size(ABOUT_NAME_SIZE)
                        .strong(),
                );
                ui.label(egui::RichText::new(ABOUT_TAGLINE).color(ui.visuals().text_color()));
                ui.add_space(8.0);
                draw_version_pill(ui, &format!("v{}", env!("CARGO_PKG_VERSION")));
                ui.add_space(18.0);
                ui.separator();
                ui.add_space(10.0);
            });
            about_details(ui);
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                let button = egui::Button::new("Close").min_size(ABOUT_BUTTON_SIZE);
                ui.add(button).clicked()
            })
            .inner
        });
    modal.inner || modal.should_close()
}

/// Paints the app logo: an accent rounded square with an "f" and a text caret.
fn draw_logo(ui: &mut egui::Ui) {
    let size = egui::vec2(ABOUT_LOGO_SIZE, ABOUT_LOGO_SIZE);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, ABOUT_LOGO_RADIUS, super::TAB_ACCENT);
    // A lighter top half gives the flat square a little depth.
    let top = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.center().y));
    let top_radius = egui::CornerRadius {
        nw: ABOUT_LOGO_RADIUS as u8,
        ne: ABOUT_LOGO_RADIUS as u8,
        sw: 0,
        se: 0,
    };
    painter.rect_filled(top, top_radius, egui::Color32::from_white_alpha(18));
    let center = rect.center();
    painter.text(
        center - egui::vec2(6.0, 0.0),
        egui::Align2::CENTER_CENTER,
        "f",
        egui::FontId::monospace(ABOUT_LOGO_FONT_SIZE),
        egui::Color32::WHITE,
    );
    let caret = egui::Rect::from_center_size(
        center + egui::vec2(14.0, 0.0),
        egui::vec2(3.0, ABOUT_LOGO_SIZE * 0.45),
    );
    painter.rect_filled(caret, 1.0, egui::Color32::from_white_alpha(190));
}

/// Paints `text` as a small rounded "pill" centered in the current layout.
fn draw_version_pill(ui: &mut egui::Ui, text: &str) {
    let visuals = ui.visuals();
    let color = visuals.text_color();
    let fill = visuals.widgets.inactive.weak_bg_fill;
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_string(), egui::FontId::monospace(12.0), color);
    let padding = egui::vec2(10.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + padding * 2.0, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, rect.height() / 2.0, fill);
    painter.galley(rect.min + padding, galley, color);
}

/// Shows the author, license and source rows as a two-column grid with weak captions.
fn about_details(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.add_space(ABOUT_DETAILS_INDENT);
        egui::Grid::new("fed_about_details")
            .num_columns(2)
            .spacing(egui::vec2(16.0, 8.0))
            .show(ui, |ui| {
                caption(ui, "Author");
                ui.hyperlink_to(AUTHOR_NAME, AUTHOR_URL);
                ui.end_row();
                caption(ui, "With");
                ui.label(CO_AUTHOR);
                ui.end_row();
                caption(ui, "License");
                ui.label(LICENSE);
                ui.end_row();
                caption(ui, "Source");
                ui.hyperlink_to(REPO_LABEL, REPO_URL);
                ui.end_row();
            });
    });
}

/// A weak, right-aligned caption cell for the details grid.
fn caption(ui: &mut egui::Ui, text: &str) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(egui::RichText::new(text).color(ui.visuals().weak_text_color()));
    });
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
