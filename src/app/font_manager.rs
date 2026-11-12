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
        let spawn = |family: &Option<String>| {
            family
                .as_ref()
                .map(|f| FontLoad::spawn(ctx.clone(), f.clone()))
        };
        FontManager {
            editor: SlotState {
                load: spawn(&settings.font_family),
                loaded: None,
            },
            preview: SlotState {
                load: spawn(&settings.preview_font_family),
                loaded: None,
            },
            catalog: None,
        }
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

    /// Applies the results of the startup font loads once they are ready. Returns a warning
    /// for every configured family that was not found.
    pub(super) fn poll(&mut self, ctx: &egui::Context) -> Vec<String> {
        let mut warnings = Vec::new();
        for slot in [FontSlot::Editor, FontSlot::Preview] {
            let state = self.slot_mut(slot);
            let Some(load) = &state.load else { continue };
            let Some(font) = load.take() else { continue };
            let family = load.family().to_string();
            state.load = None;
            warnings.extend(self.apply(ctx, slot, &family, font));
        }
        warnings
    }

    /// Applies `family` (`None`: the default font) to `slot` from the font catalog, cancelling
    /// a pending startup load. Returns a warning when the family was not found.
    pub(super) fn load(
        &mut self,
        ctx: &egui::Context,
        slot: FontSlot,
        family: Option<&str>,
    ) -> Option<String> {
        self.slot_mut(slot).load = None;
        let Some(family) = family else {
            return self.apply(ctx, slot, "", None);
        };
        let font = self
            .catalog
            .as_ref()
            .and_then(|c| c.load_family_font(family));
        self.apply(ctx, slot, family, font)
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
    fn default_font_needs_no_warning_and_unknown_family_warns() {
        let ctx = egui::Context::default();
        let mut fonts = FontManager::new(&ctx, &Settings::default());
        assert_eq!(fonts.load(&ctx, FontSlot::Editor, None), None);
        // Without a catalog scan nothing can be found.
        assert_eq!(
            fonts.load(&ctx, FontSlot::Preview, Some("No Such Font")),
            Some(not_found_warning("No Such Font"))
        );
        assert!(fonts.preview.loaded.is_none());
        assert!(fonts.poll(&ctx).is_empty());
    }
}
