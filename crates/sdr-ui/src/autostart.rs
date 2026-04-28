//! Autostart-on-login support — generate / remove
//! `$XDG_CONFIG_HOME/autostart/com.sdr.rs.desktop`.
//!
//! XDG Autostart spec:
//! <https://specifications.freedesktop.org/autostart-spec/latest/>
//!
//! The source of truth is the `.desktop` file on disk; the config
//! `autostart` boolean is a cached read-fast mirror. Startup
//! reconciles by trusting the filesystem.

use std::io;
use std::path::{Path, PathBuf};

const DESKTOP_FILE_NAME: &str = "com.sdr.rs.desktop";

const DESKTOP_FILE_BODY: &str = "\
[Desktop Entry]
Type=Application
Name=SDR-RS
Comment=Software-defined radio (auto-started in tray)
Exec=sdr-rs --start-hidden
Icon=com.sdr.rs
Hidden=false
X-GNOME-Autostart-enabled=true
";

fn desktop_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("autostart").join(DESKTOP_FILE_NAME)
}

fn default_desktop_path() -> PathBuf {
    let config_dir = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        // Pathological: no HOME. Write into CWD as a last resort --
        // the user will see autostart fail to launch and that's a
        // clearer signal than a silent drop.
        PathBuf::from(".").join(".config")
    };
    desktop_path_in(&config_dir)
}

/// `true` if the autostart `.desktop` exists at the default location.
#[must_use]
pub fn is_enabled() -> bool {
    is_enabled_at(&default_desktop_path())
}

fn is_enabled_at(path: &Path) -> bool {
    path.exists()
}

/// Write the autostart `.desktop` file at the default location.
///
/// # Errors
///
/// Returns the underlying `io::Error` from `create_dir_all` or
/// `write` -- typically permission denied or disk full.
pub fn enable() -> io::Result<()> {
    enable_at(&default_desktop_path())
}

fn enable_at(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DESKTOP_FILE_BODY)
}

/// Remove the autostart `.desktop` file. Idempotent -- missing file is `Ok(())`.
///
/// # Errors
///
/// Returns the underlying `io::Error` if the file exists but cannot be
/// removed (permission denied, etc.).
pub fn disable() -> io::Result<()> {
    disable_at(&default_desktop_path())
}

fn disable_at(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = desktop_path_in(dir.path());
        (dir, path)
    }

    #[test]
    fn is_enabled_at_returns_false_for_missing_file() {
        let (_dir, path) = tmp();
        assert!(!is_enabled_at(&path));
    }

    #[test]
    fn enable_at_writes_desktop_file_with_start_hidden_exec() {
        let (_dir, path) = tmp();
        enable_at(&path).expect("enable");
        assert!(is_enabled_at(&path));
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            body.contains("Exec=sdr-rs --start-hidden"),
            "Exec line must include --start-hidden -- got: {body}",
        );
        assert!(body.contains("Type=Application"));
        assert!(body.contains("Name=SDR-RS"));
    }

    #[test]
    fn enable_at_creates_parent_directory() {
        let dir = TempDir::new().expect("tempdir");
        let path = desktop_path_in(dir.path());
        assert!(!path.parent().expect("path has parent").exists());
        enable_at(&path).expect("enable");
        assert!(path.exists());
    }

    #[test]
    fn enable_at_is_idempotent() {
        let (_dir, path) = tmp();
        enable_at(&path).expect("first enable");
        enable_at(&path).expect("second enable");
        assert!(is_enabled_at(&path));
    }

    #[test]
    fn disable_at_removes_existing_file() {
        let (_dir, path) = tmp();
        enable_at(&path).expect("enable before disable");
        assert!(is_enabled_at(&path));
        disable_at(&path).expect("disable");
        assert!(!is_enabled_at(&path));
    }

    #[test]
    fn disable_at_is_ok_on_missing_file() {
        let (_dir, path) = tmp();
        assert!(!is_enabled_at(&path));
        disable_at(&path).expect("disable on missing must be Ok");
    }
}
