#[cfg(any(target_os = "windows", test))]
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
const VERSION_FILE: &str = ".last-visible-version";

#[cfg(target_os = "windows")]
fn version_path() -> PathBuf {
    crate::config::config_path().with_file_name(VERSION_FILE)
}

#[cfg(any(target_os = "windows", test))]
fn should_show_at(path: &Path, current: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|saved| saved.trim() != current)
        .unwrap_or(true)
}

#[cfg(any(target_os = "windows", test))]
fn record_at(path: &Path, current: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, current.as_bytes())
}

#[cfg(target_os = "windows")]
pub(crate) fn should_show() -> bool {
    should_show_at(&version_path(), env!("CARGO_PKG_VERSION"))
}

#[cfg(target_os = "windows")]
pub(crate) fn record_visible() -> std::io::Result<()> {
    record_at(&version_path(), env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "trawler-launch-visibility-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn missing_or_different_version_requires_a_visible_launch() {
        let path = test_path("transition");
        let _ = std::fs::remove_file(&path);

        assert!(should_show_at(&path, "0.5.3"));
        record_at(&path, "0.5.2").unwrap();
        assert!(should_show_at(&path, "0.5.3"));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn recorded_current_version_preserves_normal_tray_startup() {
        let path = test_path("same-version");
        let _ = std::fs::remove_file(&path);

        record_at(&path, "0.5.3").unwrap();
        assert!(!should_show_at(&path, "0.5.3"));

        std::fs::remove_file(path).unwrap();
    }
}
