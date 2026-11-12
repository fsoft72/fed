//! The configurable editor and preview fonts: startup loads, the system font catalog and
//! installing the loaded faces in the egui context.

use eframe::egui;

use crate::config::Settings;
use crate::fonts::{self, FontCatalog, FontLoad, LoadedFont};

/// Which configurable font a load or apply refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FontSlot {
    /// The editor font.
    Editor,
    /// The Markdown preview font.
    Preview,
}

/// State of one configurable font.
#[derive(Default)]
struct SlotState {
    /// Startup load of the configured family, until it finishes.
    load: Option<FontLoad>,
    /// The loaded font, if any.
    loaded: Option<LoadedFont>,
}

/// Owns the editor and preview fonts and the system font catalog.
pub(super) struct FontManager {
    /// The editor font.
    editor: SlotState,
    /// The Markdown preview font.
    preview: SlotState,
    /// System font families, scanned the first time they are needed.
    catalog: Option<FontCatalog>,
}

/// Status bar warning for a configured family that does not exist.
fn not_found_warning(family: &str) -> String {
    format!("font '{family}' not found, using the default font")
}

impl FontManager {
    /// Installs the default fonts and starts loading the families configured in `settings`
    /// on background threads.
    pub(super) fn new(ctx: &egui::Context, settings: &Settings) -> FontManager {
        fonts::apply_fonts(ctx, None, None);
        let mut manager = FontManager {
            editor: SlotState::default(),
            preview: SlotState::default(),
            catalog: None,
        };
        // Both configured fonts come from one system scan, started only when needed.
        for (slot, family) in [
            (FontSlot::Editor, &settings.font_family),
            (FontSlot::Preview, &settings.preview_font_family),
        ] {
            if let Some(family) = family {
                let load = manager.catalog(ctx).load(family.clone());
                manager.slot_mut(slot).load = Some(load);
            }
        }
        manager
    }

    /// Returns the state of `slot`.
    fn slot_mut(&mut self, slot: FontSlot) -> &mut SlotState {
        match slot {
            FontSlot::Editor => &mut self.editor,
            FontSlot::Preview => &mut self.preview,
        }
    }

    /// Returns the system font catalog, starting the scan the first time.
    pub(super) fn catalog(&mut self, ctx: &egui::Context) -> &FontCatalog {
        self.catalog
            .get_or_insert_with(|| FontCatalog::spawn(ctx.clone()))
    }

    /// Applies the results of the font loads once they are ready. Returns a warning for
    /// every requested family that was not found or whose load crashed (the default font is
    /// used then).
    pub(super) fn poll(&mut self, ctx: &egui::Context) -> Vec<String> {
        let mut warnings = Vec::new();
        for slot in [FontSlot::Editor, FontSlot::Preview] {
            let state = self.slot_mut(slot);
            let Some(load) = &state.load else { continue };
            let Some(result) = load.take() else { continue };
            let family = load.family().to_string();
            state.load = None;
            match result {
                Ok(font) => warnings.extend(self.apply(ctx, slot, &family, font)),
                Err(e) => {
                    warnings.push(format!("font '{family}' could not be loaded: {e}"));
                    self.apply(ctx, slot, "", None);
                }
            }
        }
        warnings
    }

    /// Switches `slot` to `family`, replacing any load in progress. `None` installs the
    /// default font at once; a family is loaded in the background and applied by `poll`
    /// (which reports a family that does not exist).
    pub(super) fn load(&mut self, ctx: &egui::Context, slot: FontSlot, family: Option<&str>) {
        self.slot_mut(slot).load = None;
        let Some(family) = family else {
            self.apply(ctx, slot, "", None);
            return;
        };
        let load = self.catalog(ctx).load(family.to_string());
        self.slot_mut(slot).load = Some(load);
    }

    /// Installs `font` in `slot`, or the default font when `font` is `None`. Returns a
    /// warning when a non-empty `family` was not found (an empty one means "default").
    fn apply(
        &mut self,
        ctx: &egui::Context,
        slot: FontSlot,
        family: &str,
        font: Option<egui::FontData>,
    ) -> Option<String> {
        let warning = (font.is_none() && !family.is_empty()).then(|| not_found_warning(family));
        self.slot_mut(slot).loaded = font.map(|f| (family.to_string(), f));
        fonts::apply_fonts(
            ctx,
            self.editor.loaded.as_ref(),
            self.preview.loaded.as_ref(),
        );
        warning
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_font_needs_no_scan_and_unknown_family_warns() {
        let ctx = egui::Context::default();
        let mut fonts = FontManager::new(&ctx, &Settings::default());
        assert!(fonts.catalog.is_none(), "no configured family: no scan");
        fonts.load(&ctx, FontSlot::Editor, None);
        assert!(fonts.catalog.is_none());
        assert!(fonts.poll(&ctx).is_empty());
        let warning = fonts.apply(&ctx, FontSlot::Preview, "No Such Font", None);
        assert_eq!(warning, Some(not_found_warning("No Such Font")));
        assert!(fonts.preview.loaded.is_none());
    }
}
