//! The tab bar: one flat tab per document, the Markdown "Preview" toggle and the
//! application menu.

use eframe::egui;

use super::menu::{self, MenuChoice};
use super::{FedApp, tab_path};
use crate::markdown;
use crate::tab::Tab;

/// Height of a tab.
const TAB_HEIGHT: f32 = 32.0;
/// Horizontal padding inside a tab.
const TAB_PADDING: f32 = 14.0;
/// Size of the close button hit area of a tab.
const TAB_CLOSE_SIZE: f32 = 20.0;
/// Accent line drawn on top of the selected tab.
pub(super) const TAB_ACCENT: egui::Color32 = egui::Color32::from_rgb(0, 122, 204);
/// Width of the "Preview" button in the tab bar.
const PREVIEW_BUTTON_WIDTH: f32 = 110.0;
/// Corner radius of the "Preview" button.
const PREVIEW_BUTTON_RADIUS: f32 = 4.0;
/// Thickness of the accent line.
const TAB_ACCENT_HEIGHT: f32 = 2.0;

/// Something the user did in the tab bar.
pub(super) enum TabBarAction {
    /// Switch to the tab at this index.
    Activate(usize),
    /// Close the tab at this index.
    Close(usize),
    /// Toggle the Markdown preview of the active tab.
    TogglePreview,
    /// An entry of the application menu was picked.
    Menu(MenuChoice),
}

/// What the user did on a single tab.
enum TabClick {
    /// The tab was clicked.
    Activate,
    /// The close button was clicked, or the tab middle-clicked.
    Close,
}

/// Draws one tab (label, close button, accent line when selected) and returns the click, if any.
fn draw_tab(ui: &mut egui::Ui, tab: &Tab, selected: bool) -> Option<TabClick> {
    let visuals = ui.visuals().clone();
    let text_color = if selected {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley =
        ui.painter()
            .layout_no_wrap(tab.buffer.display_name().to_string(), font, text_color);
    let width =
        TAB_PADDING + galley.size().x + TAB_PADDING / 2.0 + TAB_CLOSE_SIZE + TAB_PADDING / 2.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, TAB_HEIGHT), egui::Sense::click());
    let resp = resp.on_hover_text(tab_path(tab));
    let mut result = None;
    if resp.clicked() {
        result = Some(TabClick::Activate);
    }
    if resp.middle_clicked() {
        result = Some(TabClick::Close);
    }
    let bg = if selected {
        visuals.extreme_bg_color
    } else {
        visuals.panel_fill
    };
    let bg = if !selected && resp.hovered() {
        visuals.faint_bg_color
    } else {
        bg
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, bg);
    if selected {
        let line = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), TAB_ACCENT_HEIGHT));
        painter.rect_filled(line, 0.0, TAB_ACCENT);
    }
    painter.vline(
        rect.right(),
        rect.y_range(),
        egui::Stroke::new(1.0, visuals.window_stroke.color),
    );
    painter.galley(
        egui::pos2(
            rect.left() + TAB_PADDING,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        text_color,
    );
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(
            rect.right() - TAB_PADDING / 2.0 - TAB_CLOSE_SIZE / 2.0,
            rect.center().y,
        ),
        egui::Vec2::splat(TAB_CLOSE_SIZE),
    );
    let close_resp = ui
        .interact(
            close_rect,
            egui::Id::unique(("fed_tab_close", tab.id)),
            egui::Sense::click(),
        )
        .on_hover_text("Close tab");
    if close_resp.clicked() {
        result = Some(TabClick::Close);
    }
    let modified = tab.buffer.is_modified();
    let show_x = close_resp.hovered() || (!modified && (selected || resp.hovered()));
    let painter = ui.painter();
    if close_resp.hovered() {
        painter.rect_filled(close_rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
    }
    if show_x {
        let half = 4.0;
        let c = close_rect.center();
        let stroke = egui::Stroke::new(1.3, visuals.strong_text_color());
        painter.line_segment(
            [c + egui::vec2(-half, -half), c + egui::vec2(half, half)],
            stroke,
        );
        painter.line_segment(
            [c + egui::vec2(-half, half), c + egui::vec2(half, -half)],
            stroke,
        );
    } else if modified {
        painter.circle_filled(close_rect.center(), 3.5, text_color);
    }
    result
}

/// Draws the wide "Preview" toggle at the right of the tab bar; returns true when clicked.
fn draw_preview_button(ui: &mut egui::Ui, open: bool) -> bool {
    let visuals = ui.visuals().clone();
    let size = egui::vec2(PREVIEW_BUTTON_WIDTH, TAB_HEIGHT - 8.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let bg = if open {
        visuals.widgets.active.weak_bg_fill
    } else if resp.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.widgets.inactive.weak_bg_fill
    };
    let text_color = if open {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let painter = ui.painter();
    painter.rect_filled(rect, PREVIEW_BUTTON_RADIUS, bg);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Preview",
        egui::TextStyle::Body.resolve(ui.style()),
        text_color,
    );
    resp.clicked()
}

impl FedApp {
    /// Draws the tab bar and returns what the user clicked.
    pub(super) fn tab_bar(&self, ui: &mut egui::Ui) -> Option<TabBarAction> {
        let preview_state = self
            .tabs
            .get(self.active)
            .filter(|t| markdown::is_markdown_path(t.buffer.path()))
            .map(Tab::is_preview_open);
        let mut action = None;
        egui::Panel::top("fed_tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Reserve room for the Preview and menu buttons on the right; tabs scroll in
                // the rest.
                let spacing = ui.spacing().item_spacing.x;
                let mut reserved = menu::MENU_BUTTON_SIZE + spacing;
                if preview_state.is_some() {
                    reserved += PREVIEW_BUTTON_WIDTH + spacing;
                }
                let tabs_width = (ui.available_width() - reserved).max(0.0);
                egui::ScrollArea::horizontal()
                    .max_width(tabs_width)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            for (i, tab) in self.tabs.iter().enumerate() {
                                if let Some(tab_action) = draw_tab(ui, tab, i == self.active) {
                                    action = Some(match tab_action {
                                        TabClick::Activate => TabBarAction::Activate(i),
                                        TabClick::Close => TabBarAction::Close(i),
                                    });
                                }
                            }
                        });
                    });
                if let Some(open) = preview_state
                    && draw_preview_button(ui, open)
                {
                    action = Some(TabBarAction::TogglePreview);
                }
                if let Some(choice) = menu::hamburger_menu(ui, &self.keymap) {
                    action = Some(TabBarAction::Menu(choice));
                }
            });
        });
        action
    }
}
