//! System font discovery (background fontdb scans) and the editor font family.
//!
//! The editor and the vim command bar always draw with `FontFamily::Name("editor")`. That
//! family aliases egui's bundled monospace font until a system font is loaded and placed in
//! front of it, so the editor works before (and without) any font scan. The Markdown preview
//! has the same kind of family, `FontFamily::Name("preview")`, aliasing the proportional fonts.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId};

/// Name of the egui font family used by the editor and the vim command bar.
pub const EDITOR_FAMILY: &str = "editor";
/// Prefix of the `FontDefinitions::font_data` key of a loaded editor font.
const EDITOR_FONT_KEY_PREFIX: &str = "fed-editor:";

/// Returns the font id of the editor family at `size` points.
pub fn editor_font_id(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(EDITOR_FAMILY.into()))
}

/// Name of the egui font family used by the Markdown preview.
pub const PREVIEW_FAMILY: &str = "preview";
/// Prefix of the `FontDefinitions::font_data` key of a loaded preview font.
const PREVIEW_FONT_KEY_PREFIX: &str = "fed-preview:";

/// A loaded system font: the family name and its font data.
pub type LoadedFont = (String, FontData);

/// Returns the font id of the preview family at `size` points.
pub fn preview_font_id(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(PREVIEW_FAMILY.into()))
}

/// Registers the family `alias` in `defs`: the loaded `font` first (keyed with `key_prefix`),
/// then the `fallback` family of egui's defaults.
fn add_family(
    defs: &mut FontDefinitions,
    alias: &str,
    key_prefix: &str,
    font: Option<&LoadedFont>,
    fallback: FontFamily,
) {
    let base = defs.families.get(&fallback).cloned().unwrap_or_default();
    let mut list = Vec::with_capacity(base.len() + 1);
    if let Some((name, data)) = font {
        let key = format!("{key_prefix}{name}");
        defs.font_data.insert(key.clone(), Arc::new(data.clone()));
        list.push(key);
    }
    list.extend(base);
    defs.families.insert(FontFamily::Name(alias.into()), list);
}

/// Builds egui's default font definitions plus the editor and preview families. The editor
/// family is the loaded editor font in front of egui's monospace fonts; the preview family is
/// the loaded preview font in front of egui's proportional fonts. Without a font, a family
/// just aliases its defaults.
pub fn font_definitions(
    editor: Option<&LoadedFont>,
    preview: Option<&LoadedFont>,
) -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    add_family(
        &mut defs,
        EDITOR_FAMILY,
        EDITOR_FONT_KEY_PREFIX,
        editor,
        FontFamily::Monospace,
    );
    add_family(
        &mut defs,
        PREVIEW_FAMILY,
        PREVIEW_FONT_KEY_PREFIX,
        preview,
        FontFamily::Proportional,
    );
    defs
}

/// Installs the editor and preview families in `ctx` (see `font_definitions`).
pub fn apply_fonts(ctx: &egui::Context, editor: Option<&LoadedFont>, preview: Option<&LoadedFont>) {
    ctx.set_fonts(font_definitions(editor, preview));
}

/// Loads the system fonts into a new database (slow: call off the UI thread).
fn system_database() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    db
}

/// Returns every family name in `db` (its first, English, name), sorted case-insensitively
/// without duplicates, flagged true when any face of the family is monospaced.
fn family_list(db: &fontdb::Database) -> Vec<(String, bool)> {
    let mut map: BTreeMap<String, bool> = BTreeMap::new();
    for face in db.faces() {
        let Some((name, _)) = face.families.first() else {
            continue;
        };
        *map.entry(name.clone()).or_default() |= face.monospaced;
    }
    let mut list: Vec<(String, bool)> = map.into_iter().collect();
    list.sort_by_cached_key(|(name, _)| name.to_lowercase());
    list
}

/// Returns the regular (normal weight, style and stretch, or the closest) face of `family`
/// from `db`, or `None` when the family is unknown or its file cannot be read.
fn family_font(db: &fontdb::Database, family: &str) -> Option<FontData> {
    let families = [fontdb::Family::Name(family)];
    let query = fontdb::Query {
        families: &families,
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let id = db.query(&query)?;
    db.with_face_data(id, |data, index| {
        FontData::from_blob(Arc::new(data.to_vec()), index)
    })
}

/// Result of a system font scan.
struct Scan {
    /// Every system face.
    db: fontdb::Database,
    /// Family names with their monospace flag (see `family_list`), shared with callers.
    families: FamilyList,
}

/// Shared, read-only list of font family names with their monospace flag.
pub type FamilyList = Arc<[(String, bool)]>;

/// The system font families, scanned on a background thread.
pub struct FontCatalog {
    /// Filled by the scanner thread when the scan is done.
    slot: Arc<Mutex<Option<Arc<Scan>>>>,
}

impl FontCatalog {
    /// Starts scanning the system fonts in a thread and requests a repaint when done.
    pub fn spawn(ctx: egui::Context) -> FontCatalog {
        let slot = Arc::new(Mutex::new(None));
        let thread_slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            let db = system_database();
            let families = family_list(&db).into();
            let scan = Arc::new(Scan { db, families });
            *thread_slot.lock().expect("font catalog poisoned") = Some(scan);
            ctx.request_repaint();
        });
        FontCatalog { slot }
    }

    /// Returns the finished scan, without blocking on the scanner.
    fn scan(&self) -> Option<Arc<Scan>> {
        self.slot.lock().expect("font catalog poisoned").clone()
    }

    /// Returns the family names (sorted, monospace flag), or `None` while scanning. The
    /// list is shared, so calling this every frame does not copy it.
    pub fn families(&self) -> Option<FamilyList> {
        self.scan().map(|scan| Arc::clone(&scan.families))
    }

    /// Returns the regular face of `family` with its face index, or `None` while scanning or
    /// when the family is unknown.
    pub fn load_family_font(&self, family: &str) -> Option<FontData> {
        family_font(&self.scan()?.db, family)
    }
}

/// Loads one family's regular face on a background thread (startup font, no full catalog).
pub struct FontLoad {
    /// Requested family name.
    family: String,
    /// `Some(result)` once the thread finished; `result` is `None` when the family was not found.
    slot: Arc<Mutex<Option<Option<FontData>>>>,
}

impl FontLoad {
    /// Starts scanning the system fonts for `family` and requests a repaint when done.
    pub fn spawn(ctx: egui::Context, family: String) -> FontLoad {
        let slot = Arc::new(Mutex::new(None));
        let thread_slot = Arc::clone(&slot);
        let wanted = family.clone();
        std::thread::spawn(move || {
            let font = family_font(&system_database(), &wanted);
            *thread_slot.lock().expect("font load poisoned") = Some(font);
            ctx.request_repaint();
        });
        FontLoad { family, slot }
    }

    /// The requested family name.
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Takes the result once the thread finished: `Some(Some(font))` when found,
    /// `Some(None)` when the family does not exist, `None` while still loading.
    pub fn take(&self) -> Option<Option<FontData>> {
        self.slot.lock().expect("font load poisoned").take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Name of egui's bundled monospace font, used as test data.
    const HACK: &str = "Hack";

    /// A database holding only egui's bundled Hack font.
    fn hack_database() -> fontdb::Database {
        let defs = FontDefinitions::default();
        let hack = defs
            .font_data
            .get(HACK)
            .expect("default fonts include Hack");
        let mut db = fontdb::Database::new();
        db.load_font_data(hack.bytes().to_vec());
        db
    }

    #[test]
    fn families_alias_defaults_without_fonts() {
        let defs = font_definitions(None, None);
        let editor = &defs.families[&FontFamily::Name(EDITOR_FAMILY.into())];
        assert_eq!(editor, &defs.families[&FontFamily::Monospace]);
        let preview = &defs.families[&FontFamily::Name(PREVIEW_FAMILY.into())];
        assert_eq!(preview, &defs.families[&FontFamily::Proportional]);
    }

    #[test]
    fn loaded_fonts_come_first_with_default_fallback() {
        let font = family_font(&hack_database(), HACK).unwrap();
        let editor_font = ("My Mono".to_string(), font.clone());
        let preview_font = ("My Serif".to_string(), font);
        let defs = font_definitions(Some(&editor_font), Some(&preview_font));
        let editor = &defs.families[&FontFamily::Name(EDITOR_FAMILY.into())];
        assert_eq!(editor[0], "fed-editor:My Mono");
        assert!(defs.font_data.contains_key("fed-editor:My Mono"));
        assert_eq!(&editor[1..], &defs.families[&FontFamily::Monospace][..]);
        let preview = &defs.families[&FontFamily::Name(PREVIEW_FAMILY.into())];
        assert_eq!(preview[0], "fed-preview:My Serif");
        assert!(defs.font_data.contains_key("fed-preview:My Serif"));
        assert_eq!(&preview[1..], &defs.families[&FontFamily::Proportional][..]);
    }

    #[test]
    fn family_list_flags_monospace_and_face_lookup_by_name() {
        let db = hack_database();
        assert_eq!(family_list(&db), vec![(HACK.to_string(), true)]);
        let font = family_font(&db, HACK).expect("Hack is in the database");
        assert!(!font.bytes().is_empty());
        assert_eq!(font.index, 0);
        assert!(family_font(&db, "No Such Family").is_none());
    }

    #[test]
    fn catalog_families_are_shared_not_copied() {
        let db = hack_database();
        let families: FamilyList = family_list(&db).into();
        let scan = Arc::new(Scan { db, families });
        let catalog = FontCatalog {
            slot: Arc::new(Mutex::new(Some(scan))),
        };
        let first = catalog.families().expect("scan finished");
        let second = catalog.families().expect("scan finished");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(&first[..], &[(HACK.to_string(), true)]);
    }

    #[test]
    fn editor_font_id_uses_the_editor_family() {
        let id = editor_font_id(13.0);
        assert_eq!(id.size, 13.0);
        assert_eq!(id.family, FontFamily::Name(EDITOR_FAMILY.into()));
    }
}
