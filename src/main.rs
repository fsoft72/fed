#![cfg_attr(windows, windows_subsystem = "windows")]
//! Entry point: collects command line paths and starts the editor.

use std::path::PathBuf;
use std::time::Instant;

/// Collects the file arguments and runs the editor, passing the start time for tracing.
fn main() -> eframe::Result<()> {
    let started = Instant::now();
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    fed::run(files, started)
}
