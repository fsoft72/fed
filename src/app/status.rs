//! The status bar: tab status on the left, transient messages and notices on the right, and
//! the errors reported by the background threads that open clicked links.

use std::time::{Duration, Instant};

use eframe::egui;

use super::{FedApp, tab_path, vim};
use crate::buffer::LineEnding;
use crate::job::BackgroundJob;
use crate::markdown;
use crate::spell::SpellStatus;
use crate::tab::Tab;

/// How long a status bar message stays visible.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Status bar notice for large files (highlighting, spell check and word wrap disabled).
const LARGE_FILE_NOTICE: &str = "large file: highlighting, spell check and wrap off";
/// Status bar text while the dictionaries are downloading.
const SPELL_DOWNLOADING_NOTICE: &str = "downloading dictionaries...";
/// Suffix of the status text when the buffer has unsaved changes.
const MODIFIED_STATUS_SUFFIX: &str = "  [+]";

/// Returns the status bar text for `tab`: path (as opened, or "untitled"), 1-based
/// `line:col` of the caret, line ending, and `"  [+]"` when modified.
pub fn status_text(tab: &Tab) -> String {
    let rope = tab.buffer.rope();
    let head = tab.editor.selection.head.min(rope.len_chars());
    let line = rope.char_to_line(head);
    let col = head - rope.line_to_char(line);
    let ending = match tab.buffer.line_ending() {
        LineEnding::Lf => "LF",
        LineEnding::Crlf => "CRLF",
    };
    let mut text = format!("{}  {}:{}  {ending}", tab_path(tab), line + 1, col + 1);
    if tab.buffer.is_modified() {
        text.push_str(MODIFIED_STATUS_SUFFIX);
    }
    text
}

/// How a status bar message is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Severity {
    /// Normal text color.
    Info,
    /// Warning color (settings problems).
    Warning,
    /// Error color (failed file operations).
    Error,
}

/// A transient message on the right side of the status bar.
struct StatusMessage {
    /// Message text.
    text: String,
    /// Color class.
    severity: Severity,
    /// When the message appeared (it disappears after `STATUS_TIMEOUT`).
    shown_at: Instant,
}

/// The status bar message and the link opener jobs whose errors it reports.
pub(super) struct StatusBar {
    /// Transient message, if any.
    message: Option<StatusMessage>,
    /// Links being opened by the system opener; each ends with the error text, if any.
    link_jobs: Vec<BackgroundJob<Result<(), String>>>,
}

impl StatusBar {
    /// Creates a status bar without a message.
    pub(super) fn new() -> StatusBar {
        StatusBar {
            message: None,
            link_jobs: Vec::new(),
        }
    }

    /// Shows `text` for `STATUS_TIMEOUT`.
    pub(super) fn set(&mut self, text: String, severity: Severity) {
        self.message = Some(StatusMessage {
            text,
            severity,
            shown_at: Instant::now(),
        });
    }

    /// Drops the message once it timed out, or schedules a repaint for that moment.
    pub(super) fn expire(&mut self, ctx: &egui::Context) {
        let Some(msg) = &self.message else { return };
        let elapsed = msg.shown_at.elapsed();
        if elapsed >= STATUS_TIMEOUT {
            self.message = None;
            return;
        }
        ctx.request_repaint_after(STATUS_TIMEOUT - elapsed);
    }

    /// Shows the errors of the link opener jobs that finished since the last call (a
    /// crashed opener is an error too) and forgets the finished jobs.
    pub(super) fn poll_link_errors(&mut self) {
        let mut errors = Vec::new();
        self.link_jobs.retain(|job| match job.poll() {
            None => true,
            Some(Ok(Ok(()))) => false,
            Some(Ok(Err(error))) => {
                errors.push(error);
                false
            }
            Some(Err(crash)) => {
                errors.push(crash.to_string());
                false
            }
        });
        for error in errors {
            self.set(error, Severity::Error);
        }
    }

    /// Opens each allowed URL (see `markdown::is_openable_url`) with the system opener on a
    /// background thread; failures come back through `poll_link_errors`. Other URLs are
    /// ignored.
    pub(super) fn open_links(&mut self, ctx: &egui::Context, urls: Vec<String>) {
        let urls = urls
            .iter()
            .map(|u| u.trim().to_string())
            .filter(|u| markdown::is_openable_url(u));
        for url in urls {
            let job = BackgroundJob::spawn(Some(ctx), move || {
                open::that(&url).map_err(|e| format!("cannot open {url}: {e}"))
            });
            self.link_jobs.push(job);
        }
    }

    /// Draws the message, colored by severity, in a right-to-left layout.
    fn show_message(&self, ui: &mut egui::Ui) {
        let Some(msg) = &self.message else { return };
        let color = match msg.severity {
            Severity::Info => ui.visuals().text_color(),
            Severity::Warning => ui.visuals().warn_fg_color,
            Severity::Error => ui.visuals().error_fg_color,
        };
        ui.colored_label(color, &msg.text);
    }
}

impl FedApp {
    /// Shows a message in the status bar for `STATUS_TIMEOUT`.
    pub(super) fn set_status(&mut self, text: String, severity: Severity) {
        self.status.set(text, severity);
    }

    /// Draws the status bar: tab status on the left, notices and messages on the right.
    pub(super) fn status_bar(&mut self, ui: &mut egui::Ui) {
        let spell_status = self.spell_status();
        let mut retry = false;
        let vim = self.settings.vim_mode;
        let tab = &self.tabs[self.active];
        let status = &self.status;
        egui::Panel::bottom("fed_status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(status_text(tab));
                if vim {
                    ui.label(vim::vim_status_text(&tab.vim));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    status.show_message(ui);
                    if tab.buffer.is_large() {
                        ui.label(LARGE_FILE_NOTICE);
                    }
                    match &spell_status {
                        SpellStatus::Downloading => {
                            ui.label(SPELL_DOWNLOADING_NOTICE);
                        }
                        SpellStatus::Failed(message) => {
                            retry = ui.small_button("retry").clicked();
                            ui.colored_label(ui.visuals().warn_fg_color, message);
                        }
                        _ => {}
                    }
                });
            });
        });
        if retry && let Some(service) = &mut self.spell {
            service.retry();
        }
    }

    /// Takes this frame's link open requests (clicked links in the Markdown preview or the
    /// About dialog) from egui's output and opens the allowed ones in the background.
    /// Removing the commands keeps the backend from trying (and failing) to open them itself.
    pub(super) fn open_links(&mut self, ctx: &egui::Context) {
        let urls: Vec<String> = ctx.output_mut(|o| {
            let mut urls = Vec::new();
            o.commands.retain(|cmd| match cmd {
                egui::OutputCommand::OpenUrl(open) => {
                    urls.push(open.url.clone());
                    false
                }
                _ => true,
            });
            urls
        });
        self.status.open_links(ctx, urls);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use crate::editing::{EditCommand, EditSettings, Selection};
    use std::path::PathBuf;

    const SET: EditSettings = EditSettings {
        tab_width: 4,
        insert_spaces: true,
    };

    #[test]
    fn status_text_format() {
        let tab = Tab::new(Buffer::new());
        assert_eq!(status_text(&tab), "untitled  1:1  LF");

        let mut tab = Tab::new(Buffer::from_text("a\r\nbc"));
        tab.buffer.set_path(PathBuf::from("src/lib.rs"));
        tab.editor.selection = Selection::caret(3);
        assert_eq!(status_text(&tab), "src/lib.rs  2:2  CRLF");

        tab.apply(EditCommand::InsertText("x".into()), &SET);
        assert_eq!(status_text(&tab), "src/lib.rs  2:3  CRLF  [+]");
    }

    #[test]
    fn link_errors_become_error_messages() {
        let mut status = StatusBar::new();
        let failing = BackgroundJob::spawn(None, || Err("cannot open x".to_string()));
        status.link_jobs.push(failing);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !status.link_jobs.is_empty() {
            assert!(Instant::now() < deadline, "the opener job never finished");
            status.poll_link_errors();
        }
        let msg = status.message.as_ref().expect("message set");
        assert_eq!(msg.text, "cannot open x");
        assert_eq!(msg.severity, Severity::Error);
    }
}
