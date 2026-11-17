//! A small transient toast shown above the status bar (e.g. "Copied to clipboard").

use std::time::{Duration, Instant};

use eframe::egui;

/// How long a toast stays fully visible.
const TOAST_VISIBLE: Duration = Duration::from_millis(1500);
/// How long the toast fades out after `TOAST_VISIBLE`.
const TOAST_FADE: Duration = Duration::from_millis(300);
/// Distance of the toast from the bottom edge of the window, in points.
const TOAST_BOTTOM_OFFSET: f32 = 48.0;
/// Text of the toast shown after a copy.
pub(super) const COPIED_TOAST: &str = "Copied to clipboard";

/// The toast being shown, if any.
#[derive(Default)]
pub(super) struct Toast {
    /// Text and the time it appeared.
    current: Option<(String, Instant)>,
}

impl Toast {
    /// Shows `text`, replacing the current toast and restarting its timer.
    pub(super) fn show(&mut self, text: &str) {
        self.current = Some((text.to_string(), Instant::now()));
    }

    /// Draws the toast (fading it out at the end) and drops it once expired.
    pub(super) fn draw(&mut self, ctx: &egui::Context) {
        let Some((text, shown_at)) = &self.current else {
            return;
        };
        let elapsed = shown_at.elapsed();
        if elapsed >= TOAST_VISIBLE + TOAST_FADE {
            self.current = None;
            return;
        }
        let opacity = Self::opacity(elapsed);
        egui::Area::new(egui::Id::unique("fed_toast"))
            .order(egui::Order::Tooltip)
            .anchor(
                egui::Align2::CENTER_BOTTOM,
                egui::vec2(0.0, -TOAST_BOTTOM_OFFSET),
            )
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(opacity);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(text);
                });
            });
        // Keep animating the fade, then wake up once more to remove the toast.
        match elapsed < TOAST_VISIBLE {
            true => ctx.request_repaint_after(TOAST_VISIBLE - elapsed),
            false => ctx.request_repaint(),
        }
    }

    /// Opacity (0.0 to 1.0) of a toast shown `elapsed` ago.
    fn opacity(elapsed: Duration) -> f32 {
        if elapsed <= TOAST_VISIBLE {
            return 1.0;
        }
        let fading = (elapsed - TOAST_VISIBLE).as_secs_f32() / TOAST_FADE.as_secs_f32();
        (1.0 - fading).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_is_full_then_fades_to_zero() {
        assert_eq!(Toast::opacity(Duration::ZERO), 1.0);
        assert_eq!(Toast::opacity(TOAST_VISIBLE), 1.0);
        let half = Toast::opacity(TOAST_VISIBLE + TOAST_FADE / 2);
        assert!((half - 0.5).abs() < 0.01);
        assert_eq!(Toast::opacity(TOAST_VISIBLE + TOAST_FADE * 2), 0.0);
    }

    #[test]
    fn show_replaces_the_current_toast() {
        let mut toast = Toast::default();
        toast.show("a");
        toast.show("b");
        assert_eq!(toast.current.as_ref().map(|c| c.0.as_str()), Some("b"));
    }
}
