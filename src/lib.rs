//! fed: a fast text editor built with Rust and egui.

pub mod app;
pub mod buffer;

use std::path::PathBuf;

use app::FedApp;

/// Initial window width in logical pixels.
const INITIAL_WIDTH: f32 = 1000.0;
/// Initial window height in logical pixels.
const INITIAL_HEIGHT: f32 = 700.0;
/// Window title.
const WINDOW_TITLE: &str = "fed";

/// Starts the editor window and opens `files` in tabs. Blocks until the window closes.
pub fn run(files: Vec<PathBuf>) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([INITIAL_WIDTH, INITIAL_HEIGHT]),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(FedApp::new(cc, files)))),
    )
}
