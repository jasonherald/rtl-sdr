//! JSON configuration persistence.
//!
//! Ports SDR++ `ConfigManager`. Provides thread-safe JSON configuration
//! with load, save, and auto-save functionality.

use sdr_types::ConfigError;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};
use std::thread;
use std::time::Duration;

/// Auto-save interval in milliseconds.
const AUTO_SAVE_INTERVAL_MS: u64 = 1000;

/// Thread-safe JSON configuration manager.
///
/// Ports SDR++ `ConfigManager`. Provides:
/// - Load from file with defaults merging
/// - Thread-safe read/write access via `RwLock`
/// - Auto-save to disk on a background thread when modified
pub struct ConfigManager {
    path: PathBuf,
    data: Arc<RwLock<Value>>,
    modified: Arc<Mutex<bool>>,
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
    /// If the file exists but is corrupt, it's reset to defaults.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Io` on filesystem errors.
    pub fn load(path: &Path, defaults: &Value) -> Result<Self, ConfigError> {
        let path = path.to_path_buf();
        let mut should_save = false;

        let data = if path.exists() {
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(parsed) => merge_defaults(parsed, defaults),
                    Err(e) => {
                        tracing::warn!("config file corrupt, resetting: {e}");
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
            path,
            data: Arc::new(RwLock::new(data)),
            modified: Arc::new(Mutex::new(false)),
            auto_save_handle: None,
        };

        // Only save when creating new file or resetting corrupt config
        if should_save {
            mgr.save()?;
        }

        Ok(mgr)
    }

    /// Save configuration to disk.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Io` on write failure.
    pub fn save(&self) -> Result<(), ConfigError> {
        let data = self.data.read().unwrap_or_else(PoisonError::into_inner);
        let content =
            serde_json::to_string_pretty(&*data).map_err(|e| ConfigError::Json(e.to_string()))?;
        // Atomic write: write to temp file, then rename over original
        let tmp_path = self.path.with_extension("tmp");
        std::fs::write(&tmp_path, &content)?;
        std::fs::rename(&tmp_path, &self.path)?;
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
        let mut m = self.modified.lock().unwrap_or_else(PoisonError::into_inner);
        *m = true;
        result
    }

    /// Enable periodic auto-save on a background thread.
    ///
    /// Checks for modifications every second and saves if needed.
    pub fn enable_auto_save(&mut self) {
        if self.auto_save_handle.is_some() {
            return;
        }

        let stop_flag = Arc::new((Mutex::new(false), Condvar::new()));
        let stop_clone = Arc::clone(&stop_flag);
        let data = Arc::clone(&self.data);
        let modified = Arc::clone(&self.modified);
        let path = self.path.clone();

        let thread = thread::spawn(move || {
            auto_save_worker(stop_clone, data, modified, path);
        });

        self.auto_save_handle = Some(AutoSaveHandle {
            stop_flag,
            thread: Some(thread),
        });
    }

    /// Disable auto-save and stop the background thread.
    pub fn disable_auto_save(&mut self) {
        self.auto_save_handle = None;
    }

    /// Returns the config file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Recursively merge loaded config with defaults — adds missing keys at all levels.
fn merge_defaults(mut loaded: Value, defaults: &Value) -> Value {
    if let (Some(loaded_obj), Some(defaults_obj)) = (loaded.as_object_mut(), defaults.as_object()) {
        for (key, default_val) in defaults_obj {
            if let Some(existing) = loaded_obj.get_mut(key) {
                // Recursively merge nested objects
                if existing.is_object() && default_val.is_object() {
                    let merged = merge_defaults(existing.take(), default_val);
                    *existing = merged;
                }
                // Existing non-object value takes precedence
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
    modified: Arc<Mutex<bool>>,
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

        // Check if modified
        let should_save = {
            let mut m = modified.lock().unwrap_or_else(PoisonError::into_inner);
            if *m {
                *m = false;
                true
            } else {
                false
            }
        };

        if should_save && !flush_to_disk(&data, &path) {
            // Re-mark dirty so next cycle retries
            let mut m = modified.lock().unwrap_or_else(PoisonError::into_inner);
            *m = true;
        }
    }
}

/// Check modified flag and flush to disk if set. Re-marks dirty on failure.
fn flush_if_modified(modified: &Arc<Mutex<bool>>, data: &Arc<RwLock<Value>>, path: &Path) {
    let mut m = modified.lock().unwrap_or_else(PoisonError::into_inner);
    if *m {
        *m = false;
        drop(m);
        if !flush_to_disk(data, path) {
            // Re-mark dirty so next cycle retries
            let mut m = modified.lock().unwrap_or_else(PoisonError::into_inner);
            *m = true;
        }
    }
}

/// Write data to disk, logging any errors. Returns true on success.
fn flush_to_disk(data: &Arc<RwLock<Value>>, path: &Path) -> bool {
    let d = data.read().unwrap_or_else(PoisonError::into_inner);
    match serde_json::to_string_pretty(&*d) {
        Ok(content) => {
            if let Err(e) = std::fs::write(path, &content) {
                tracing::error!("auto-save write failed: {e}");
                false
            } else {
                true
            }
        }
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
}
