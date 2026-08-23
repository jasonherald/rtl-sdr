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
    // Exclusive creation (`create_new`): never follow a pre-existing
    // symlink / hard link planted at the temp name in a directory another
    // user can write to. A collision just moves on to the next id.
    let (tmp, file) = loop {
        let tmp_id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let tmp = target.with_extension(format!("tmp.{}.{tmp_id}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        let mut file = file;
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

/// Upper bound on symlink hops while resolving a dangling chain (matches
/// the kernel's `MAXSYMLINKS`).
const MAX_SYMLINK_HOPS: usize = 40;

/// The real file a config path refers to. A symlink is followed
/// (`canonicalize`); a *dangling* chain is walked link by link with
/// `read_link` until the missing final target, with cycle detection, so
/// the target can be (re)created and every link kept. A plain path is
/// returned unchanged.
fn resolve_link_target(path: &Path) -> std::io::Result<PathBuf> {
    let is_symlink =
        |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    if !is_symlink(path) {
        return Ok(path.to_path_buf());
    }
    if let Ok(real) = std::fs::canonicalize(path) {
        return Ok(real);
    }
    // Dangling somewhere along the chain: resolve the link text hop by
    // hop until the first path that is not itself a symlink.
    let mut current = path.to_path_buf();
    let mut visited = std::collections::HashSet::new();
    for _ in 0..MAX_SYMLINK_HOPS {
        if !visited.insert(current.clone()) {
            return Err(std::io::Error::other(format!(
                "symlink cycle while resolving {}",
                path.display()
            )));
        }
        let link = std::fs::read_link(&current)?;
        current = if link.is_absolute() {
            link
        } else {
            current
                .parent()
                .map_or(link.clone(), |parent| parent.join(link))
        };
        if !is_symlink(&current) {
            return Ok(current);
        }
    }
    Err(std::io::Error::other(format!(
        "too many symlink hops while resolving {}",
        path.display()
    )))
}

/// fsync the directory containing `path` so a completed `rename` survives
/// a crash. Best-effort: the data is already published by the rename, so
/// a filesystem that refuses a directory fsync must not turn a successful
/// save into an error (and an auto-save retry loop).
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        // A bare relative path has an empty parent; that is the cwd.
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        if let Err(e) = std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
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

/// Move a corrupt config aside as `<name>.corrupt-<unix seconds>-<pid>-<n>`
/// so a bad byte never silently destroys every setting (#761). The name
/// carries the pid and a counter so two recoveries in the same second
/// cannot overwrite each other.
///
/// # Errors
///
/// The rename's I/O error. The caller must NOT reset the config in that
/// case — the corrupt file is the only recoverable copy.
fn back_up_corrupt_config(path: &Path) -> std::io::Result<PathBuf> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Move the real file, not the link, so a symlinked config keeps its
    // link (the reset then writes through it).
    let target = resolve_link_target(path)?;
    let serial = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut backup = target.as_os_str().to_owned();
    backup.push(format!(
        "{CORRUPT_BACKUP_INFIX}{secs}-{}-{serial}",
        std::process::id()
    ));
    let backup = PathBuf::from(backup);
    std::fs::rename(&target, &backup)?;
    // Make the rename durable before the reset writes a new file, or a
    // crash in between could lose both.
    sync_parent_dir(&target);
    tracing::warn!(backup = %backup.display(), "corrupt config backed up");
    Ok(backup)
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
    /// Held across snapshot + `write_atomic` by `save()` and the auto-save
    /// worker, so two writers can't take snapshots in one order and
    /// rename them in the other (an older snapshot winning the file).
    write_lock: Arc<Mutex<()>>,
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
    /// `<name>.corrupt-<unix seconds>-<pid>-<serial>` and reset to defaults
    /// (#761). If the backup cannot be made, the load fails instead of
    /// overwriting the only copy.
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
                        // No backup → no reset: never overwrite the only copy.
                        back_up_corrupt_config(&path)?;
                        should_save = true;
                        defaults.clone()
                    }
                    Err(e) => {
                        tracing::warn!("config file corrupt, resetting: {e}");
                        back_up_corrupt_config(&path)?;
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
            write_lock: Arc::new(Mutex::new(())),
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
            write_lock: Arc::new(Mutex::new(())),
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
        // Serialize under the data lock, then write without it (#763) —
        // but hold the write lock across both so snapshot order and
        // publish order agree (CR round 3 on PR #794).
        let _writing = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
        // Mark dirty BEFORE the callback: if it mutates and then panics,
        // the change must still reach the next flush.
        self.modified.store(true, Ordering::Release);
        f(&mut data)
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
        let write_lock = Arc::clone(&self.write_lock);

        let thread = thread::spawn(move || {
            auto_save_worker(stop_clone, data, modified, write_lock, path);
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

    /// Write pending changes to disk now, synchronously. Quit paths call
    /// this instead of relying on the auto-save handle's `Drop`, which
    /// only runs once every `Arc` clone held by GTK closures and timers
    /// has died (#762). A no-op when nothing changed or for the
    /// in-memory fallback.
    ///
    /// # Errors
    ///
    /// The save error; the config stays marked dirty so a later flush
    /// retries.
    pub fn flush(&self) -> Result<(), ConfigError> {
        if self.path.is_none() || !self.modified.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        if let Err(e) = self.save() {
            self.modified.store(true, Ordering::Release);
            return Err(e);
        }
        Ok(())
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
                } else if !default_val.is_null()
                    && std::mem::discriminant(&*existing) != std::mem::discriminant(default_val)
                {
                    // Any JSON-kind mismatch (a string where a number is
                    // expected, a scalar where a subtree is, an object
                    // where a scalar is) would make later typed reads or
                    // `v["a"]["b"]` indexing misbehave — the default wins
                    // (#761). A `null` default carries no type and never
                    // overrides a real value.
                    tracing::warn!(key, "config value has the wrong type, using the default");
                    *existing = default_val.clone();
                }
                // Otherwise the existing value of the same kind takes precedence
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
    write_lock: Arc<Mutex<()>>,
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
                flush_if_modified(&modified, &data, &write_lock, &path);
                break;
            }
        }

        flush_if_modified(&modified, &data, &write_lock, &path);
    }
}

/// Clear the modified flag and flush to disk if it was set. Re-marks
/// dirty on failure so the next cycle retries.
fn flush_if_modified(
    modified: &AtomicBool,
    data: &RwLock<Value>,
    write_lock: &Mutex<()>,
    path: &Path,
) {
    if modified.swap(false, Ordering::AcqRel) && !flush_to_disk(data, write_lock, path) {
        modified.store(true, Ordering::Release);
    }
}

/// Write data to disk, logging any errors. Returns true on success. The
/// read lock is held only while serializing, never across the I/O (#763);
/// the write lock is held across both so publish order matches snapshot
/// order with `save()`.
fn flush_to_disk(data: &RwLock<Value>, write_lock: &Mutex<()>, path: &Path) -> bool {
    let _writing = write_lock.lock().unwrap_or_else(PoisonError::into_inner);
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
mod tests;
