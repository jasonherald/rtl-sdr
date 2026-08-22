//! JSON configuration persistence.
//!
//! Ports SDR++ `ConfigManager`. Provides thread-safe JSON configuration
//! with load, save, and auto-save functionality.

pub mod keyring_store;
pub use keyring_store::KeyringStore;

use sdr_types::ConfigError;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};
use std::thread;
use std::time::Duration;

/// Auto-save interval in milliseconds.
const AUTO_SAVE_INTERVAL_MS: u64 = 1000;

/// Process-wide counter so every in-flight atomic write owns its own
/// temp file (pid disambiguates processes, the counter threads).
static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(0);

/// Suffix of the backup a corrupt config is renamed to before the reset:
/// `config.json.corrupt-<unix seconds>`.
const CORRUPT_BACKUP_INFIX: &str = ".corrupt-";

/// Write `content` to `path` atomically: a per-call temp file in the same
/// directory, `sync_all` on it, `rename` over the target, then an fsync
/// of the directory so the rename itself is durable. If `path` is a
/// symlink the write lands on its target so dotfiles setups keep their
/// link (#760).
///
/// # Errors
///
/// Any I/O failure; the temp file is removed on error.
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    // Follow a symlink to its final target; rename over the link itself
    // would replace the link with a regular file.
    let target = resolve_link_target(path)?;
    let tmp_id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
    let tmp = target.with_extension(format!("tmp.{}.{tmp_id}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        // Keep the replaced file's mode (a 0600 config must not come
        // back as 0644 after a save).
        if let Ok(meta) = std::fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(content.as_bytes())?;
        // Data must be on disk before the rename publishes it, or a power
        // loss right after the rename can leave an empty config.
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &target)?;
        sync_parent_dir(&target);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The real file a config path refers to. A symlink is followed
/// (`canonicalize`); a *dangling* symlink resolves through `read_link`
/// so the target can be (re)created and the link kept. A plain path is
/// returned unchanged.
fn resolve_link_target(path: &Path) -> std::io::Result<PathBuf> {
    let is_symlink = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if !is_symlink {
        return Ok(path.to_path_buf());
    }
    if let Ok(real) = std::fs::canonicalize(path) {
        return Ok(real);
    }
    // Dangling: resolve the link text ourselves.
    let link = std::fs::read_link(path)?;
    Ok(if link.is_absolute() {
        link
    } else {
        path.parent()
            .map_or(link.clone(), |parent| parent.join(link))
    })
}

/// fsync the directory containing `path` so a completed `rename` survives
/// a crash. Best-effort: the data is already published by the rename, so
/// a filesystem that refuses a directory fsync must not turn a successful
/// save into an error (and an auto-save retry loop).
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::File::open(parent).and_then(|dir| dir.sync_all())
        {
            tracing::debug!("directory fsync skipped: {e}");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Serialize the config for disk. Called with the read lock held only for
/// the duration of the serialization — never across I/O (#763).
fn serialize_config(data: &RwLock<Value>) -> Result<String, ConfigError> {
    let d = data.read().unwrap_or_else(PoisonError::into_inner);
    serde_json::to_string_pretty(&*d).map_err(|e| ConfigError::Json(e.to_string()))
}

/// Move a corrupt config aside as `<name>.corrupt-<unix seconds>` so a bad
/// byte never silently destroys every setting (#761). Best-effort: a
/// failure to back up is logged and the reset proceeds.
fn back_up_corrupt_config(path: &Path) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Move the real file, not the link, so a symlinked config keeps its
    // link (the reset then writes through it).
    let target = resolve_link_target(path).unwrap_or_else(|_| path.to_path_buf());
    let mut backup = target.as_os_str().to_owned();
    backup.push(format!("{CORRUPT_BACKUP_INFIX}{secs}"));
    let backup = PathBuf::from(backup);
    match std::fs::rename(&target, &backup) {
        Ok(()) => tracing::warn!(backup = %backup.display(), "corrupt config backed up"),
        Err(e) => tracing::error!("could not back up corrupt config: {e}"),
    }
}

/// Thread-safe JSON configuration manager.
///
/// Ports SDR++ `ConfigManager`. Provides:
/// - Load from file with defaults merging
/// - Thread-safe read/write access via `RwLock`
/// - Auto-save to disk on a background thread when modified
pub struct ConfigManager {
    /// `None` for the in-memory fallback (nothing persists).
    path: Option<PathBuf>,
    data: Arc<RwLock<Value>>,
    modified: Arc<AtomicBool>,
    auto_save_handle: Option<AutoSaveHandle>,
}

struct AutoSaveHandle {
    stop_flag: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for AutoSaveHandle {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.stop_flag;
        {
            let mut stop = lock.lock().unwrap_or_else(PoisonError::into_inner);
            *stop = true;
        }
        cvar.notify_all();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl ConfigManager {
    /// Load configuration from a file, merging with defaults.
    ///
    /// If the file doesn't exist, it's created with the default values.
    /// If the file exists but is corrupt — unparsable, or parsable but not
    /// a JSON object at the root — it's backed up as
    /// `<name>.corrupt-<unix seconds>` and reset to defaults (#761).
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Io` on filesystem errors.
    pub fn load(path: &Path, defaults: &Value) -> Result<Self, ConfigError> {
        let path = path.to_path_buf();
        let mut should_save = false;

        let data = if path.exists() {
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(parsed) if parsed.is_object() => merge_defaults(parsed, defaults),
                    Ok(other) => {
                        // `[]`, `5`, `"x"`: parses, but every later
                        // `config.write(|v| v["key"] = …)` would panic in
                        // serde_json's IndexMut — treat as corrupt.
                        tracing::warn!(
                            "config root is not a JSON object ({}), resetting",
                            json_kind(&other)
                        );
                        back_up_corrupt_config(&path);
                        should_save = true;
                        defaults.clone()
                    }
                    Err(e) => {
                        tracing::warn!("config file corrupt, resetting: {e}");
                        back_up_corrupt_config(&path);
                        should_save = true;
                        defaults.clone()
                    }
                },
                Err(e) => {
                    // IO error on existing file — don't overwrite, propagate error
                    return Err(ConfigError::Io(e));
                }
            }
        } else {
            // Create parent directories if needed
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            should_save = true;
            defaults.clone()
        };

        let mgr = Self {
            path: Some(path),
            data: Arc::new(RwLock::new(data)),
            modified: Arc::new(AtomicBool::new(false)),
            auto_save_handle: None,
        };

        // Only save when creating new file or resetting corrupt config
        if should_save {
            mgr.save()?;
        }

        Ok(mgr)
    }

    /// Create an in-memory configuration that won't persist to disk.
    ///
    /// Used as a fallback when the config file can't be loaded (permissions,
    /// disk full, etc.). The app remains functional with default settings.
    pub fn in_memory(defaults: &Value) -> Self {
        Self {
            path: None,
            data: Arc::new(RwLock::new(defaults.clone())),
            modified: Arc::new(AtomicBool::new(false)),
            auto_save_handle: None,
        }
    }

    /// Save configuration to disk.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Io` on write failure.
    pub fn save(&self) -> Result<(), ConfigError> {
        // In-memory configs have no path — skip saving.
        let Some(path) = &self.path else {
            return Ok(());
        };
        // Serialize under the lock, then write without it (#763).
        let content = serialize_config(&self.data)?;
        write_atomic(path, &content)?;
        Ok(())
    }

    /// Read the configuration via a closure.
    ///
    /// The closure receives an immutable reference to the JSON value.
    pub fn read<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Value) -> R,
    {
        let data = self.data.read().unwrap_or_else(PoisonError::into_inner);
        f(&data)
    }

    /// Write to the configuration via a closure.
    ///
    /// The closure receives a mutable reference to the JSON value.
    /// Automatically marks the config as modified for auto-save.
    pub fn write<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Value) -> R,
    {
        let mut data = self.data.write().unwrap_or_else(PoisonError::into_inner);
        let result = f(&mut data);
        self.modified.store(true, Ordering::Release);
        result
    }

    /// Enable periodic auto-save on a background thread.
    ///
    /// Checks for modifications every second and saves if needed.
    pub fn enable_auto_save(&mut self) {
        // In-memory configs have no path — nothing to auto-save.
        let Some(path) = self.path.clone() else {
            return;
        };
        if self.auto_save_handle.is_some() {
            return;
        }

        let stop_flag = Arc::new((Mutex::new(false), Condvar::new()));
        let stop_clone = Arc::clone(&stop_flag);
        let data = Arc::clone(&self.data);
        let modified = Arc::clone(&self.modified);

        let thread = thread::spawn(move || {
            auto_save_worker(stop_clone, data, modified, path);
        });

        self.auto_save_handle = Some(AutoSaveHandle {
            stop_flag,
            thread: Some(thread),
        });
    }

    /// Disable auto-save and stop the background thread (flushing any
    /// pending change on the way out).
    pub fn disable_auto_save(&mut self) {
        self.auto_save_handle = None;
    }

    /// Returns the config file path (empty for the in-memory fallback).
    pub fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or_else(|| Path::new(""))
    }

    /// `true` for the in-memory fallback: nothing persists to disk, so
    /// the UI can tell the user their settings will not survive a restart.
    #[must_use]
    pub fn is_in_memory(&self) -> bool {
        self.path.is_none()
    }
}

/// Short description of a JSON value's kind for log lines.
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Recursively merge loaded config with defaults — adds missing keys at all levels.
fn merge_defaults(mut loaded: Value, defaults: &Value) -> Value {
    if let (Some(loaded_obj), Some(defaults_obj)) = (loaded.as_object_mut(), defaults.as_object()) {
        for (key, default_val) in defaults_obj {
            if let Some(existing) = loaded_obj.get_mut(key) {
                if existing.is_object() && default_val.is_object() {
                    // Recursively merge nested objects
                    let merged = merge_defaults(existing.take(), default_val);
                    *existing = merged;
                } else if default_val.is_object() {
                    // A non-object where a subtree is expected would make
                    // later `v["a"]["b"]` indexing panic — the default
                    // subtree wins (#761).
                    tracing::warn!(key, "config subtree has the wrong type, using defaults");
                    *existing = default_val.clone();
                }
                // Otherwise the existing scalar value takes precedence
            } else {
                loaded_obj.insert(key.clone(), default_val.clone());
            }
        }
    }
    loaded
}

/// Background auto-save worker thread.
#[allow(clippy::needless_pass_by_value)]
fn auto_save_worker(
    stop_flag: Arc<(Mutex<bool>, Condvar)>,
    data: Arc<RwLock<Value>>,
    modified: Arc<AtomicBool>,
    path: PathBuf,
) {
    let (lock, cvar) = &*stop_flag;
    loop {
        // Wait for interval or stop signal (predicate avoids missed-notify stall)
        {
            let stop = lock.lock().unwrap_or_else(PoisonError::into_inner);
            let (guard, _timeout) = cvar
                .wait_timeout_while(stop, Duration::from_millis(AUTO_SAVE_INTERVAL_MS), |s| !*s)
                .unwrap_or_else(PoisonError::into_inner);
            if *guard {
                // Flush any pending changes before exiting
                flush_if_modified(&modified, &data, &path);
                break;
            }
        }

        flush_if_modified(&modified, &data, &path);
    }
}

/// Clear the modified flag and flush to disk if it was set. Re-marks
/// dirty on failure so the next cycle retries.
fn flush_if_modified(modified: &AtomicBool, data: &RwLock<Value>, path: &Path) {
    if modified.swap(false, Ordering::AcqRel) && !flush_to_disk(data, path) {
        modified.store(true, Ordering::Release);
    }
}

/// Write data to disk, logging any errors. Returns true on success. The
/// read lock is held only while serializing, never across the I/O (#763).
fn flush_to_disk(data: &RwLock<Value>, path: &Path) -> bool {
    match serialize_config(data) {
        Ok(content) => match write_atomic(path, &content) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!("auto-save write failed: {e}");
                false
            }
        },
        Err(e) => {
            tracing::error!("auto-save serialization failed: {e}");
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("sdr-config-test");
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn test_load_creates_new_file() {
        let path = temp_path("test_new.json");
        let _ = fs::remove_file(&path);

        let defaults = json!({"volume": 0.5, "frequency": 100_000_000});
        let mgr = ConfigManager::load(&path, &defaults).unwrap();

        assert!(path.exists());
        mgr.read(|v| {
            assert_eq!(v["volume"], 0.5);
            assert_eq!(v["frequency"], 100_000_000);
        });

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_load_existing_file() {
        let path = temp_path("test_existing.json");
        fs::write(&path, r#"{"volume": 0.8}"#).unwrap();

        let defaults = json!({"volume": 0.5, "frequency": 100_000_000});
        let mgr = ConfigManager::load(&path, &defaults).unwrap();

        mgr.read(|v| {
            assert_eq!(v["volume"], 0.8);
            assert_eq!(v["frequency"], 100_000_000);
        });

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_load_corrupt_file() {
        let path = temp_path("test_corrupt.json");
        fs::write(&path, "not valid json!!!").unwrap();

        let defaults = json!({"volume": 0.5});
        let mgr = ConfigManager::load(&path, &defaults).unwrap();

        mgr.read(|v| {
            assert_eq!(v["volume"], 0.5);
        });

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_write_and_save() {
        let path = temp_path("test_write.json");
        let _ = fs::remove_file(&path);

        let defaults = json!({"volume": 0.5});
        let mgr = ConfigManager::load(&path, &defaults).unwrap();

        mgr.write(|v| {
            v["volume"] = json!(0.9);
            v["new_key"] = json!("hello");
        });
        mgr.save().unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let on_disk: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(on_disk["volume"], 0.9);
        assert_eq!(on_disk["new_key"], "hello");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_auto_save() {
        let path = temp_path("test_autosave.json");
        let _ = fs::remove_file(&path);

        let defaults = json!({"volume": 0.5});
        let mut mgr = ConfigManager::load(&path, &defaults).unwrap();
        mgr.enable_auto_save();

        mgr.write(|v| {
            v["volume"] = json!(0.75);
        });

        thread::sleep(Duration::from_millis(1500));

        let content = fs::read_to_string(&path).unwrap();
        let on_disk: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(on_disk["volume"], 0.75);

        mgr.disable_auto_save();
        let _ = fs::remove_file(&path);
    }

    /// Unique temp dir per test so parallel tests never share files.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sdr-config-test-{name}-{}-{}",
            std::process::id(),
            NEXT_TMP_ID.load(std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// #760 — `save()`, the auto-save worker and a second manager all used
    /// one fixed `config.tmp`, so concurrent writes interleaved and could
    /// publish torn JSON (next launch: full reset). Every writer must own
    /// its own temp file and every save must succeed.
    #[test]
    fn concurrent_saves_never_publish_torn_json() {
        const WRITERS: usize = 8;
        const SAVES_PER_WRITER: usize = 40;
        let dir = temp_dir("concurrent");
        let path = dir.join("config.json");
        let mgr = Arc::new(ConfigManager::load(&path, &json!({"n": 0})).unwrap());
        let handles: Vec<_> = (0..WRITERS)
            .map(|w| {
                let mgr = Arc::clone(&mgr);
                thread::spawn(move || {
                    for i in 0..SAVES_PER_WRITER {
                        mgr.write(|v| v["n"] = json!(w * SAVES_PER_WRITER + i));
                        mgr.save().expect("every concurrent save succeeds");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let on_disk: Value = serde_json::from_slice(&fs::read(&path).unwrap())
            .expect("the published file is always complete JSON");
        assert!(on_disk["n"].is_number());
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp files left behind: {leftovers:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// #760 — renaming over a symlinked config replaced the link and
    /// detached dotfiles setups; the write must land on the link target.
    #[cfg(unix)]
    #[test]
    fn save_writes_through_a_symlinked_config() {
        let dir = temp_dir("symlink");
        let real = dir.join("real.json");
        let link = dir.join("config.json");
        fs::write(&real, r#"{"volume": 0.1}"#).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mgr = ConfigManager::load(&link, &json!({"volume": 0.5})).unwrap();
        mgr.write(|v| v["volume"] = json!(0.9));
        mgr.save().unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "link preserved"
        );
        let on_disk: Value = serde_json::from_slice(&fs::read(&real).unwrap()).unwrap();
        assert_eq!(on_disk["volume"], 0.9, "the target received the write");
        let _ = fs::remove_dir_all(&dir);
    }

    /// #760 (CR round 1 on PR #794) — a dangling symlink must be written
    /// through (target recreated, link kept), not fail the save.
    #[cfg(unix)]
    #[test]
    fn save_recreates_the_target_of_a_dangling_symlink() {
        let dir = temp_dir("dangling");
        let real = dir.join("real.json");
        let link = dir.join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(!real.exists(), "test premise: dangling");

        let mgr = ConfigManager::load(&link, &json!({"volume": 0.5})).unwrap();
        mgr.save().unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let on_disk: Value = serde_json::from_slice(&fs::read(&real).unwrap()).unwrap();
        assert_eq!(on_disk["volume"], 0.5);
        let _ = fs::remove_dir_all(&dir);
    }

    /// #761 (CR round 1 on PR #794) — backing up a corrupt config behind a
    /// symlink moves the real file and keeps the link.
    #[cfg(unix)]
    #[test]
    fn corrupt_backup_keeps_a_symlinked_config_linked() {
        let dir = temp_dir("corrupt-symlink");
        let real = dir.join("real.json");
        let link = dir.join("config.json");
        fs::write(&real, "garbage").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let _mgr = ConfigManager::load(&link, &json!({"volume": 0.5})).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "link kept"
        );
        assert!(
            fs::read_dir(&dir).unwrap().filter_map(Result::ok).any(|e| e
                .file_name()
                .to_string_lossy()
                .starts_with("real.json.corrupt-")),
            "the real file was backed up"
        );
        let on_disk: Value = serde_json::from_slice(&fs::read(&real).unwrap()).unwrap();
        assert_eq!(
            on_disk["volume"], 0.5,
            "the reset was written through the link"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// #760 (CR round 1 on PR #794) — a save must not loosen the file mode.
    #[cfg(unix)]
    #[test]
    fn save_preserves_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        const PRIVATE_MODE: u32 = 0o600;
        let dir = temp_dir("mode");
        let path = dir.join("config.json");
        let mgr = ConfigManager::load(&path, &json!({"volume": 0.5})).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_MODE)).unwrap();

        mgr.write(|v| v["volume"] = json!(0.9));
        mgr.save().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, PRIVATE_MODE, "mode must survive the atomic replace");
        let _ = fs::remove_dir_all(&dir);
    }

    /// #761 — one bad byte used to destroy every setting with only a
    /// journal warning; the bad file must be kept as a backup first.
    #[test]
    fn corrupt_config_is_backed_up_before_reset() {
        let dir = temp_dir("corrupt-backup");
        let path = dir.join("config.json");
        fs::write(&path, "not valid json!!!").unwrap();

        let mgr = ConfigManager::load(&path, &json!({"volume": 0.5})).unwrap();
        mgr.read(|v| assert_eq!(v["volume"], 0.5));

        let backup = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.json.corrupt-")
            })
            .expect("a .corrupt-<timestamp> backup exists");
        assert_eq!(
            fs::read_to_string(backup.path()).unwrap(),
            "not valid json!!!"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// #761 — a valid-but-non-object root (`[]`, `5`, `"x"`) parsed fine,
    /// skipped the corrupt branch, and the first `write` panicked in a GTK
    /// signal handler. It must be treated as corrupt.
    #[test]
    fn non_object_root_is_treated_as_corrupt() {
        let dir = temp_dir("non-object");
        let path = dir.join("config.json");
        fs::write(&path, "[]").unwrap();

        let mgr = ConfigManager::load(&path, &json!({"volume": 0.5})).unwrap();
        mgr.read(|v| assert!(v.is_object(), "root must be an object"));
        mgr.write(|v| v["volume"] = json!(0.7)); // must not panic
        mgr.read(|v| assert_eq!(v["volume"], 0.7));
        assert!(
            fs::read_dir(&dir).unwrap().filter_map(Result::ok).any(|e| e
                .file_name()
                .to_string_lossy()
                .starts_with("config.json.corrupt-")),
            "the non-object file is kept as a backup"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// #761 — a subtree whose type doesn't match the default (a number
    /// where an object is expected) would make later indexing panic; the
    /// default subtree wins.
    #[test]
    fn mismatched_subtree_is_replaced_by_the_default() {
        let loaded = json!({"audio": 5});
        let defaults = json!({"audio": {"volume": 0.5}});
        let merged = merge_defaults(loaded, &defaults);
        assert_eq!(merged["audio"]["volume"], 0.5);
    }

    /// #761 — callers can tell an in-memory fallback apart so the UI can
    /// say that settings will not persist.
    #[test]
    fn in_memory_is_reported() {
        assert!(ConfigManager::in_memory(&json!({})).is_in_memory());
        let dir = temp_dir("in-memory-flag");
        let mgr = ConfigManager::load(&dir.join("config.json"), &json!({})).unwrap();
        assert!(!mgr.is_in_memory());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_merge_defaults() {
        let loaded = json!({"a": 1, "b": 2});
        let defaults = json!({"b": 99, "c": 3});
        let merged = merge_defaults(loaded, &defaults);
        assert_eq!(merged["a"], 1);
        assert_eq!(merged["b"], 2);
        assert_eq!(merged["c"], 3);
    }

    #[test]
    fn test_merge_defaults_recursive() {
        let loaded = json!({"audio": {"volume": 0.8}});
        let defaults = json!({"audio": {"volume": 0.5, "device": "default"}, "freq": 100});
        let merged = merge_defaults(loaded, &defaults);
        assert_eq!(merged["audio"]["volume"], 0.8); // loaded wins
        assert_eq!(merged["audio"]["device"], "default"); // merged from defaults
        assert_eq!(merged["freq"], 100); // top-level default
    }

    #[test]
    fn test_path() {
        let path = temp_path("test_path.json");
        let _ = fs::remove_file(&path);
        let mgr = ConfigManager::load(&path, &json!({})).unwrap();
        assert_eq!(mgr.path(), path);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn in_memory_save_is_noop() {
        let mgr = ConfigManager::in_memory(&json!({"key": "value"}));
        // save() should succeed without creating any file.
        mgr.save().unwrap();
        assert!(mgr.path().as_os_str().is_empty());
    }

    #[test]
    fn in_memory_enable_auto_save_is_noop() {
        let mut mgr = ConfigManager::in_memory(&json!({}));
        mgr.enable_auto_save();
        // No auto-save handle should be created for in-memory configs.
        assert!(mgr.auto_save_handle.is_none());
    }

    #[test]
    fn in_memory_read_write_works() {
        let mgr = ConfigManager::in_memory(&json!({"volume": 0.5}));
        mgr.read(|v| assert_eq!(v["volume"], 0.5));
        mgr.write(|v| v["volume"] = json!(0.8));
        mgr.read(|v| assert_eq!(v["volume"], 0.8));
    }
}
