#![cfg_attr(windows, windows_subsystem = "windows")]
//! Entry point: collects command line paths and starts the editor.

use std::path::PathBuf;

fn main() -> eframe::Result<()> {
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    fed::run(files)
}
