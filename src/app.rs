//! Application shell: the `eframe::App` implementation.

use std::path::PathBuf;

/// Top-level editor application state.
pub struct FedApp {
    /// Files requested on the command line, not opened yet.
    #[allow(dead_code)]
    files: Vec<PathBuf>,
}

impl FedApp {
    /// Creates the application; `files` are the paths passed on the command line.
    pub fn new(_cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> FedApp {
        FedApp { files }
    }
}

impl eframe::App for FedApp {
    /// Draws one frame.
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ui, |ui| {
            ui.label("fed");
        });
    }
}
