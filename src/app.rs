//! Application shell: the `eframe::App` implementation.

use std::path::PathBuf;

use eframe::egui;

use crate::buffer::Buffer;
use crate::config::Settings;
use crate::editing::EditSettings;
use crate::tab::Tab;
use crate::view::{self, PlainDecorator, ViewOptions};

/// Top-level editor application state.
pub struct FedApp {
    /// Open documents, one per tab.
    tabs: Vec<Tab>,
    /// Index of the tab shown in the editor area.
    active: usize,
    /// Errors from opening files, shown at the bottom of the window.
    errors: Vec<String>,
    /// Editor settings.
    settings: Settings,
}

impl FedApp {
    /// Creates the application; `files` are the paths passed on the command line.
    /// Opens one tab per readable file, or one empty tab.
    pub fn new(_cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> FedApp {
        let mut tabs = Vec::new();
        let mut errors = Vec::new();
        for path in &files {
            match Buffer::load(path) {
                Ok(buffer) => tabs.push(Tab::new(buffer)),
                Err(e) => errors.push(e.to_string()),
            }
        }
        if tabs.is_empty() {
            tabs.push(Tab::new(Buffer::new()));
        }
        FedApp {
            tabs,
            active: 0,
            errors,
            settings: Settings::default(),
        }
    }

    /// Builds the editor widget options from the current settings.
    fn view_options(&self) -> ViewOptions {
        ViewOptions {
            font: egui::FontId::monospace(self.settings.font_size),
            line_numbers: self.settings.line_numbers,
            word_wrap: self.settings.word_wrap,
            edit: EditSettings {
                tab_width: self.settings.tab_width,
                insert_spaces: self.settings.insert_spaces,
            },
            keys_enabled: true,
        }
    }
}

impl eframe::App for FedApp {
    /// Draws one frame.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.errors.is_empty() {
            egui::Panel::bottom("errors").show(ui, |ui| {
                let color = ui.visuals().error_fg_color;
                for err in &self.errors {
                    ui.colored_label(color, err);
                }
            });
        }
        let opts = self.view_options();
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(tab) = self.tabs.get_mut(self.active) else {
                return;
            };
            let out = view::show_editor(ui, tab, &opts, &mut PlainDecorator, None);
            if let Some(text) = out.clipboard {
                ui.ctx().copy_text(text);
            }
        });
    }
}
