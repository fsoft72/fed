//! fed: a fast text editor built with Rust and egui.

pub mod app;
pub mod buffer;
pub mod config;
pub mod editing;
pub mod find_bar;
pub mod fonts;
pub mod highlight;
pub mod keymap;
pub mod markdown;
pub mod search;
pub mod settings_ui;
pub mod spell;
pub mod tab;
pub mod text_nav;
pub mod view;
pub mod vim;

use std::path::PathBuf;
use std::time::Instant;

use app::FedApp;

/// Initial window width in logical pixels.
const INITIAL_WIDTH: f32 = 1000.0;
/// Initial window height in logical pixels.
const INITIAL_HEIGHT: f32 = 700.0;
/// Window title.
const WINDOW_TITLE: &str = "fed";
/// Environment variable that turns on startup tracing when set to `1`.
const TRACE_STARTUP_VAR: &str = "FED_TRACE_STARTUP";

/// Starts the editor window and opens `files` in tabs. Blocks until the window closes.
/// `started` is the process start time: with `FED_TRACE_STARTUP=1` the time from it to the
/// end of the first frame is printed to stderr.
pub fn run(files: Vec<PathBuf>, started: Instant) -> eframe::Result<()> {
    let trace = std::env::var_os(TRACE_STARTUP_VAR).is_some_and(|v| v == "1");
    let startup_trace = trace.then_some(started);
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
        Box::new(move |cc| Ok(Box::new(FedApp::new(cc, files, startup_trace)))),
    )
}
