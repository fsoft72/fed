//! Locating and downloading Hunspell dictionaries.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Languages that are spell checked.
pub const LANGS: [&str; 2] = ["it_IT", "en_US"];
/// Base URL of the public LibreOffice dictionaries repository.
pub const DICT_BASE_URL: &str = "https://raw.githubusercontent.com/LibreOffice/dictionaries/master";
/// Timeout of one download request.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest accepted dictionary file in bytes.
const MAX_DICT_BYTES: u64 = 64 * 1024 * 1024;

/// Returns the path of `lang` inside the LibreOffice repository, without extension.
fn remote_path(lang: &str) -> &'static str {
    match lang {
        "it_IT" => "it_IT/it_IT",
        _ => "en/en_US",
    }
}

/// Returns the system directories that may hold Hunspell dictionaries.
pub fn system_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "linux") {
        [
            "/usr/share/hunspell",
            "/usr/share/myspell",
            "/usr/share/myspell/dicts",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    } else {
        Vec::new()
    }
}

/// Returns the `(aff, dic)` paths for `lang` from the first directory that has both,
/// looking in `dict_dir` first and then in `system_dirs`.
pub fn locate(lang: &str, dict_dir: &Path, system_dirs: &[PathBuf]) -> Option<(PathBuf, PathBuf)> {
    std::iter::once(dict_dir)
        .chain(system_dirs.iter().map(PathBuf::as_path))
        .map(|dir| {
            (
                dir.join(format!("{lang}.aff")),
                dir.join(format!("{lang}.dic")),
            )
        })
        .find(|(aff, dic)| aff.is_file() && dic.is_file())
}

/// Downloads `url` into memory.
fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .into();
    let mut response = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_DICT_BYTES)
        .read_to_vec()
        .map_err(|e| format!("{url}: {e}"))
}

/// Writes `bytes` to `path` through a temporary file so readers never see a partial file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("part");
    fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Downloads the `.aff` and `.dic` files of `lang` into `dict_dir` and returns their paths.
pub fn download(lang: &str, dict_dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    fs::create_dir_all(dict_dir).map_err(|e| format!("{}: {e}", dict_dir.display()))?;
    let remote = remote_path(lang);
    let mut paths = Vec::with_capacity(2);
    for ext in ["aff", "dic"] {
        let bytes = fetch(&format!("{DICT_BASE_URL}/{remote}.{ext}"))?;
        let path = dict_dir.join(format!("{lang}.{ext}"));
        write_atomic(&path, &bytes)?;
        paths.push(path);
    }
    let dic = paths.pop().expect("two paths");
    let aff = paths.pop().expect("two paths");
    Ok((aff, dic))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates fake `lang` dictionary files in `dir`.
    fn fake(dir: &Path, lang: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{lang}.aff")), "SET UTF-8\n").unwrap();
        fs::write(dir.join(format!("{lang}.dic")), "0\n").unwrap();
    }

    /// Returns a fresh scratch directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fed_dict_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn locate_prefers_config_dir() {
        let root = scratch("prefer");
        let (cfg, sys) = (root.join("cfg"), root.join("sys"));
        fake(&cfg, "it_IT");
        fake(&sys, "it_IT");
        fake(&sys, "en_US");
        let sys_dirs = vec![sys.clone()];
        let (aff, _) = locate("it_IT", &cfg, &sys_dirs).unwrap();
        assert_eq!(aff, cfg.join("it_IT.aff"));
        let (aff, dic) = locate("en_US", &cfg, &sys_dirs).unwrap();
        assert_eq!((aff, dic), (sys.join("en_US.aff"), sys.join("en_US.dic")));
        assert!(locate("fr_FR", &cfg, &sys_dirs).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn locate_needs_both_files() {
        let root = scratch("both");
        fs::write(root.join("it_IT.aff"), "").unwrap();
        assert!(locate("it_IT", &root, &[]).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_paths_match_repository_layout() {
        assert_eq!(remote_path("it_IT"), "it_IT/it_IT");
        assert_eq!(remote_path("en_US"), "en/en_US");
    }
}
