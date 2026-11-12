//! Settings and session persistence with lenient TOML loading.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Default editor font size in points.
const DEFAULT_FONT_SIZE: f32 = 14.0;
/// Allowed font size range in points.
const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 6.0..=72.0;
/// Default Markdown preview font size in points.
const DEFAULT_PREVIEW_FONT_SIZE: f32 = 14.0;
/// Default tab width in columns.
const DEFAULT_TAB_WIDTH: usize = 4;
/// Allowed tab width range in columns.
const TAB_WIDTH_RANGE: std::ops::RangeInclusive<usize> = 1..=16;
/// Default syntax theme for the light UI.
const DEFAULT_CODE_THEME_LIGHT: &str = "InspiredGitHub";
/// Default syntax theme for the dark UI.
const DEFAULT_CODE_THEME_DARK: &str = "base16-ocean.dark";

/// File name of the settings file inside the config directory.
const SETTINGS_FILE: &str = "settings.toml";
/// File name of the session file inside the config directory.
const SESSION_FILE: &str = "session.toml";
/// Directory name of the downloaded spelling dictionaries.
const DICTIONARIES_DIR: &str = "dictionaries";
/// File name of the personal spelling dictionary.
const PERSONAL_DICTIONARY_FILE: &str = "personal.dic";

/// Which UI theme to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiTheme {
    /// Follow the operating system.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// User-editable editor settings, persisted as `settings.toml`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Settings {
    /// Monospace font family name; `None` uses the built-in font.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Font size in points (6..=72).
    pub font_size: f32,
    /// Markdown preview font family name; `None` uses the built-in proportional font.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_font_family: Option<String>,
    /// Markdown preview base font size in points (6..=72).
    pub preview_font_size: f32,
    /// Tab width in columns (1..=16).
    pub tab_width: usize,
    /// Insert spaces instead of tab characters.
    pub insert_spaces: bool,
    /// Show the line number gutter.
    pub line_numbers: bool,
    /// Wrap long lines.
    pub word_wrap: bool,
    /// UI theme choice.
    pub ui_theme: UiTheme,
    /// Syntax theme used with the light UI.
    pub code_theme_light: String,
    /// Syntax theme used with the dark UI.
    pub code_theme_dark: String,
    /// Enable vim keybindings.
    pub vim_mode: bool,
    /// Enable spell checking.
    pub spell_check: bool,
    /// Reopen the previous session's files on startup.
    pub restore_session: bool,
    /// Keybinding overrides: action name to chord string. Empty means defaults.
    pub keybindings: BTreeMap<String, String>,
}

impl Default for Settings {
    /// Returns the factory default settings.
    fn default() -> Self {
        Settings {
            font_family: None,
            font_size: DEFAULT_FONT_SIZE,
            preview_font_family: None,
            preview_font_size: DEFAULT_PREVIEW_FONT_SIZE,
            tab_width: DEFAULT_TAB_WIDTH,
            insert_spaces: true,
            line_numbers: true,
            word_wrap: false,
            ui_theme: UiTheme::default(),
            code_theme_light: DEFAULT_CODE_THEME_LIGHT.to_string(),
            code_theme_dark: DEFAULT_CODE_THEME_DARK.to_string(),
            vim_mode: false,
            spell_check: true,
            restore_session: false,
            keybindings: BTreeMap::new(),
        }
    }
}

/// Converts a TOML value to `T`, returning a warning naming `key` on a type error.
fn convert<T: serde::de::DeserializeOwned>(key: &str, value: &toml::Value) -> Result<T, String> {
    value
        .clone()
        .try_into::<T>()
        .map_err(|e| format!("setting '{key}' ignored: {}", e.message()))
}

/// Reads a float setting, accepting TOML integers too, and checks its range.
fn convert_f32(
    key: &str,
    value: &toml::Value,
    range: &std::ops::RangeInclusive<f32>,
) -> Result<f32, String> {
    let n = match value {
        toml::Value::Float(f) => *f as f32,
        toml::Value::Integer(i) => *i as f32,
        _ => return Err(format!("setting '{key}' ignored: expected a number")),
    };
    if !range.contains(&n) {
        return Err(format!(
            "setting '{key}' ignored: {n} is outside {}..={}",
            range.start(),
            range.end()
        ));
    }
    Ok(n)
}

/// Reads an integer setting and checks its range.
fn convert_usize(
    key: &str,
    value: &toml::Value,
    range: &std::ops::RangeInclusive<usize>,
) -> Result<usize, String> {
    let n: usize = convert(key, value)?;
    if !range.contains(&n) {
        return Err(format!(
            "setting '{key}' ignored: {n} is outside {}..={}",
            range.start(),
            range.end()
        ));
    }
    Ok(n)
}

impl Settings {
    /// Parses settings leniently: bad fields keep their default and yield a warning,
    /// unknown keys yield a warning, and a syntax error yields all defaults plus one warning.
    pub fn from_toml_str(s: &str) -> (Settings, Vec<String>) {
        let table: toml::Table = match s.parse() {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("settings file has a syntax error, using defaults: {e}");
                return (Settings::default(), vec![msg]);
            }
        };
        let mut out = Settings::default();
        let mut warnings = Vec::new();
        for (key, value) in &table {
            if let Err(w) = out.apply(key, value) {
                warnings.push(w);
            }
        }
        (out, warnings)
    }

    /// Applies one parsed key to `self`, or returns a warning and leaves it unchanged.
    fn apply(&mut self, key: &str, value: &toml::Value) -> Result<(), String> {
        match key {
            "font_family" => self.font_family = Some(convert(key, value)?),
            "font_size" => self.font_size = convert_f32(key, value, &FONT_SIZE_RANGE)?,
            "preview_font_family" => self.preview_font_family = Some(convert(key, value)?),
            "preview_font_size" => {
                self.preview_font_size = convert_f32(key, value, &FONT_SIZE_RANGE)?
            }
            "tab_width" => self.tab_width = convert_usize(key, value, &TAB_WIDTH_RANGE)?,
            "insert_spaces" => self.insert_spaces = convert(key, value)?,
            "line_numbers" => self.line_numbers = convert(key, value)?,
            "word_wrap" => self.word_wrap = convert(key, value)?,
            "ui_theme" => self.ui_theme = convert(key, value)?,
            "code_theme_light" => self.code_theme_light = convert(key, value)?,
            "code_theme_dark" => self.code_theme_dark = convert(key, value)?,
            "vim_mode" => self.vim_mode = convert(key, value)?,
            "spell_check" => self.spell_check = convert(key, value)?,
            "restore_session" => self.restore_session = convert(key, value)?,
            "keybindings" => self.keybindings = convert(key, value)?,
            _ => return Err(format!("unknown setting '{key}' ignored")),
        }
        Ok(())
    }

    /// Serializes the settings to TOML text.
    pub fn to_toml_string(&self) -> String {
        toml::to_string(self).expect("settings are always serializable to TOML")
    }
}

/// Locations of fed's files inside its config directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    /// The config directory.
    pub dir: PathBuf,
}

impl ConfigPaths {
    /// Uses the platform config directory (`ProjectDirs` for "fed"); `None` if unavailable.
    pub fn detect() -> Option<ConfigPaths> {
        let dirs = ProjectDirs::from("", "", "fed")?;
        Some(ConfigPaths {
            dir: dirs.config_dir().to_path_buf(),
        })
    }

    /// Uses an explicit directory (for tests).
    pub fn new(dir: PathBuf) -> ConfigPaths {
        ConfigPaths { dir }
    }

    /// Path of `settings.toml`.
    pub fn settings_file(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    /// Path of `session.toml`.
    pub fn session_file(&self) -> PathBuf {
        self.dir.join(SESSION_FILE)
    }

    /// Directory holding downloaded dictionaries.
    pub fn dictionaries_dir(&self) -> PathBuf {
        self.dir.join(DICTIONARIES_DIR)
    }

    /// Path of the personal dictionary word list.
    pub fn personal_dictionary(&self) -> PathBuf {
        self.dir.join(PERSONAL_DICTIONARY_FILE)
    }
}

/// Loads settings; a missing file gives defaults without warnings.
pub fn load_settings(paths: &ConfigPaths) -> (Settings, Vec<String>) {
    match std::fs::read_to_string(paths.settings_file()) {
        Ok(text) => Settings::from_toml_str(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Settings::default(), Vec::new()),
        Err(e) => (
            Settings::default(),
            vec![format!("cannot read settings file, using defaults: {e}")],
        ),
    }
}

/// Writes settings to disk, creating the config directory if needed.
pub fn save_settings(paths: &ConfigPaths, settings: &Settings) -> io::Result<()> {
    write_in_config_dir(paths, &paths.settings_file(), &settings.to_toml_string())
}

/// The files open when the editor last closed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Paths of the open files, in tab order.
    pub files: Vec<PathBuf>,
}

/// Loads the session. A missing file is an empty session; a file that cannot be read or
/// parsed is an empty session plus a warning, like `load_settings`.
pub fn load_session(paths: &ConfigPaths) -> (Session, Vec<String>) {
    let file = paths.session_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (Session::default(), Vec::new()),
        Err(e) => {
            let warning = format!("cannot read session file {}: {e}", file.display());
            return (Session::default(), vec![warning]);
        }
    };
    match toml::from_str(&text) {
        Ok(session) => (session, Vec::new()),
        Err(e) => {
            let warning = format!("session file {} is invalid, ignored: {e}", file.display());
            (Session::default(), vec![warning])
        }
    }
}

/// Writes the session to disk, creating the config directory if needed.
pub fn save_session(paths: &ConfigPaths, session: &Session) -> io::Result<()> {
    let text = toml::to_string(session).map_err(io::Error::other)?;
    write_in_config_dir(paths, &paths.session_file(), &text)
}

/// Suffix of the temporary file written next to a config file before it is renamed.
const TMP_SUFFIX: &str = ".fed-tmp";

/// Creates the config directory and writes `text` to `file` atomically: the content goes to
/// a temporary file in the same directory which is then renamed over `file`, so a crash or
/// a full disk never leaves a truncated config behind.
fn write_in_config_dir(paths: &ConfigPaths, file: &Path, text: &str) -> io::Result<()> {
    std::fs::create_dir_all(&paths.dir)?;
    let mut tmp_name = file.as_os_str().to_os_string();
    tmp_name.push(TMP_SUFFIX);
    let tmp = PathBuf::from(tmp_name);
    let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, file));
    if result.is_err() {
        // Best effort cleanup; the write or rename error is the one worth reporting.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Creates a fresh unique temp directory for a test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fed-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn defaults_roundtrip_through_toml() {
        let mut s = Settings::default();
        s.keybindings.insert("save".into(), "Ctrl+Alt+S".into());
        s.font_family = Some("Fira Code".into());
        let text = s.to_toml_string();
        let (back, warnings) = Settings::from_toml_str(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back, s);
        let (def, warnings) = Settings::from_toml_str(&Settings::default().to_toml_string());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(def, Settings::default());
    }

    #[test]
    fn preview_font_settings_roundtrip_and_validate() {
        let mut s = Settings::default();
        s.preview_font_family = Some("Georgia".into());
        s.preview_font_size = 18.0;
        let (back, warnings) = Settings::from_toml_str(&s.to_toml_string());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back, s);
        let (d, warnings) = Settings::from_toml_str("preview_font_size = 500\n");
        assert_eq!(d.preview_font_size, 14.0);
        assert!(warnings.iter().any(|w| w.contains("preview_font_size")));
    }

    #[test]
    fn invalid_field_falls_back_with_warning() {
        let (s, warnings) =
            Settings::from_toml_str("font_size = \"big\"\ntab_width = 0\nword_wrap = true\n");
        assert_eq!(s.font_size, 14.0);
        assert_eq!(s.tab_width, 4);
        assert!(s.word_wrap);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("font_size")));
        assert!(warnings.iter().any(|w| w.contains("tab_width")));
    }

    #[test]
    fn unknown_key_warns() {
        let (s, warnings) = Settings::from_toml_str("bogus = 1\n");
        assert_eq!(s, Settings::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bogus"));
    }

    #[test]
    fn syntax_error_gives_defaults() {
        let (s, warnings) = Settings::from_toml_str("font_size = = 3");
        assert_eq!(s, Settings::default());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn session_roundtrip() {
        let paths = ConfigPaths::new(temp_dir("session"));
        assert_eq!(load_session(&paths), (Session::default(), Vec::new()));
        let session = Session {
            files: vec![PathBuf::from("/a/b.txt"), PathBuf::from("c.md")],
        };
        save_session(&paths, &session).unwrap();
        assert_eq!(load_session(&paths), (session, Vec::new()));
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn broken_session_file_warns() {
        let paths = ConfigPaths::new(temp_dir("session-broken"));
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::write(paths.session_file(), "files = = [").unwrap();
        let (session, warnings) = load_session(&paths);
        assert_eq!(session, Session::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("invalid"), "{warnings:?}");
        // A directory in place of the file cannot be read.
        std::fs::remove_file(paths.session_file()).unwrap();
        std::fs::create_dir_all(paths.session_file()).unwrap();
        let (_, warnings) = load_session(&paths);
        assert!(warnings[0].contains("cannot read"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn settings_file_roundtrip_and_missing() {
        let paths = ConfigPaths::new(temp_dir("settings"));
        let (s, w) = load_settings(&paths);
        assert_eq!(s, Settings::default());
        assert!(w.is_empty());
        let custom = Settings {
            tab_width: 2,
            ..Settings::default()
        };
        save_settings(&paths, &custom).unwrap();
        assert_eq!(load_settings(&paths).0, custom);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    #[test]
    fn settings_save_is_atomic_and_leaves_no_temp_file() {
        let paths = ConfigPaths::new(temp_dir("atomic"));
        save_settings(&paths, &Settings::default()).unwrap();
        let custom = Settings {
            tab_width: 8,
            ..Settings::default()
        };
        save_settings(&paths, &custom).unwrap();
        assert_eq!(load_settings(&paths).0, custom);
        let names: Vec<_> = std::fs::read_dir(&paths.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![paths.settings_file().file_name().unwrap()]);
        // A failing rename (target is a directory) reports the error and cleans up.
        std::fs::remove_file(paths.settings_file()).unwrap();
        std::fs::create_dir(paths.settings_file()).unwrap();
        assert!(save_settings(&paths, &custom).is_err());
        assert_eq!(std::fs::read_dir(&paths.dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }
}
