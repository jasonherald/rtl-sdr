use super::*;
use serde_json::json;
use std::fs;

/// One randomized, 0700 `tempfile` root per test process — no fixed
/// names in the shared system temp dir.
fn test_tmp_root() -> &'static tempfile::TempDir {
    static TEST_TMP_ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    TEST_TMP_ROOT.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("sdr-config-tests-")
            .tempdir()
            .expect("test temp root")
    })
}

fn temp_path(name: &str) -> PathBuf {
    test_tmp_root().path().join(name)
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
    let dir = test_tmp_root().path().join(format!(
        "{name}-{}",
        NEXT_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// #760 — `save()`, the auto-save worker and a second manager all used
/// one fixed `config.tmp`, so concurrent writes interleaved and could
/// publish torn JSON (next launch: full reset). Every writer must own
/// its own temp file and every save must succeed — across two
/// independent managers on the same path (no shared write lock) with
/// the auto-save worker of one of them running.
#[test]
fn concurrent_saves_never_publish_torn_json() {
    const WRITERS_PER_MANAGER: usize = 4;
    const SAVES_PER_WRITER: usize = 40;
    let dir = temp_dir("concurrent");
    let path = dir.join("config.json");
    let mut first = ConfigManager::load(&path, &json!({"n": 0})).unwrap();
    first.enable_auto_save();
    let managers = [
        Arc::new(first),
        Arc::new(ConfigManager::load(&path, &json!({"n": 0})).unwrap()),
    ];
    let mut handles = Vec::new();
    for (m, mgr) in managers.iter().enumerate() {
        for w in 0..WRITERS_PER_MANAGER {
            let mgr = Arc::clone(mgr);
            handles.push(thread::spawn(move || {
                for i in 0..SAVES_PER_WRITER {
                    mgr.write(|v| v["n"] = json!((m * 10 + w) * SAVES_PER_WRITER + i));
                    mgr.save().expect("every concurrent save succeeds");
                }
            }));
        }
    }
    for h in handles {
        h.join().unwrap();
    }
    drop(managers); // stops the auto-save worker (final flush)
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

/// #760 (CR round 2 on PR #794) — a chain of symlinks whose final
/// target is missing must be written through to that target; no
/// intermediate link may be replaced by a regular file.
#[cfg(unix)]
#[test]
fn save_follows_a_chained_dangling_symlink() {
    let dir = temp_dir("chained");
    let real = dir.join("real.json");
    let mid = dir.join("mid.json");
    let link = dir.join("config.json");
    std::os::unix::fs::symlink(&real, &mid).unwrap();
    std::os::unix::fs::symlink(&mid, &link).unwrap();

    let mgr = ConfigManager::load(&link, &json!({"volume": 0.5})).unwrap();
    mgr.save().unwrap();
    for l in [&link, &mid] {
        assert!(
            fs::symlink_metadata(l).unwrap().file_type().is_symlink(),
            "{l:?} kept"
        );
    }
    let on_disk: Value = serde_json::from_slice(&fs::read(&real).unwrap()).unwrap();
    assert_eq!(on_disk["volume"], 0.5);
    let _ = fs::remove_dir_all(&dir);
}

/// #760 (CR round 2 on PR #794) — a symlink cycle is an error, not a
/// hang or a replaced link.
#[cfg(unix)]
#[test]
fn symlink_cycle_is_an_error() {
    let dir = temp_dir("cycle");
    let a = dir.join("a.json");
    let b = dir.join("b.json");
    std::os::unix::fs::symlink(&b, &a).unwrap();
    std::os::unix::fs::symlink(&a, &b).unwrap();
    let err = resolve_link_target(&a).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
    assert!(err.to_string().contains("cycle"));
    let _ = fs::remove_dir_all(&dir);
}

/// #761 (CR round 2 on PR #794) — if the corrupt file cannot be backed
/// up, the reset must not run: the corrupt file is the only copy.
/// Linux-only: the root check below reads `/proc/self`, and root
/// ignores directory permissions.
#[cfg(target_os = "linux")]
#[test]
fn failed_backup_aborts_the_reset() {
    use std::os::unix::fs::PermissionsExt;
    /// Directory mode with no write bit: the rename must fail.
    const READ_EXEC_ONLY: u32 = 0o500;
    /// Restored afterwards so the temp dir can be removed.
    const OWNER_FULL: u32 = 0o700;
    if is_root() {
        return; // root ignores directory permissions
    }
    let dir = temp_dir("backup-fails");
    let path = dir.join("config.json");
    fs::write(&path, "garbage").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(READ_EXEC_ONLY)).unwrap();

    let result = ConfigManager::load(&path, &json!({"volume": 0.5}));
    fs::set_permissions(&dir, fs::Permissions::from_mode(OWNER_FULL)).unwrap();
    assert!(result.is_err(), "load must fail rather than overwrite");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "garbage",
        "the corrupt file is untouched"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

/// #761 (CR round 2 on PR #794) — two recoveries in the same second
/// produce two backups.
#[test]
fn repeated_recoveries_keep_every_backup() {
    let dir = temp_dir("two-backups");
    let path = dir.join("config.json");
    for _ in 0..2 {
        fs::write(&path, "garbage").unwrap();
        let _ = ConfigManager::load(&path, &json!({"volume": 0.5})).unwrap();
    }
    let backups = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
        .count();
    assert_eq!(backups, 2);
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

/// #761 (CR round 4 on PR #794) — every kind mismatch is replaced, not
/// only scalar-where-object.
#[test]
fn scalar_kind_mismatches_are_replaced_by_the_default() {
    let loaded = json!({"volume": "loud", "audio": {"x": 1}, "name": "keep", "opt": 3});
    let defaults = json!({"volume": 0.5, "audio": 5, "name": "default", "opt": null});
    let merged = merge_defaults(loaded, &defaults);
    assert_eq!(merged["volume"], 0.5, "string where a number is expected");
    assert_eq!(merged["audio"], 5, "object where a scalar is expected");
    assert_eq!(merged["name"], "keep", "same kind: the loaded value wins");
    assert_eq!(merged["opt"], 3, "a null default never overrides a value");
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

/// #762 — quit paths need a synchronous flush instead of relying on
/// the auto-save handle's `Drop` (which needs every `Arc` clone in
/// GTK closures to die first).
#[test]
fn flush_writes_pending_changes_synchronously() {
    let dir = temp_dir("flush");
    let path = dir.join("config.json");
    let mgr = ConfigManager::load(&path, &json!({"volume": 0.5})).unwrap();
    mgr.write(|v| v["volume"] = json!(0.9));
    mgr.flush().unwrap();
    let on_disk: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(on_disk["volume"], 0.9);
    assert!(
        !mgr.modified.load(Ordering::Acquire),
        "flush clears the dirty flag"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// #762 — a clean config does not touch the disk on flush.
#[test]
fn flush_is_a_noop_when_clean() {
    let dir = temp_dir("flush-clean");
    let path = dir.join("config.json");
    let mgr = ConfigManager::load(&path, &json!({"volume": 0.5})).unwrap();
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    thread::sleep(Duration::from_millis(20));
    mgr.flush().unwrap();
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    assert!(ConfigManager::in_memory(&json!({})).flush().is_ok());
    let _ = fs::remove_dir_all(&dir);
}

/// #762 (CR round 1 on PR #795) — a failed flush keeps the dirty flag
/// set so the next flush retries instead of skipping the save.
#[cfg(target_os = "linux")]
#[test]
fn failed_flush_keeps_the_config_dirty() {
    use std::os::unix::fs::PermissionsExt;
    /// Directory mode with no write bit: the atomic temp-file
    /// create must fail.
    const READ_EXEC_ONLY: u32 = 0o500;
    /// Restored afterwards so the temp dir can be removed.
    const OWNER_FULL: u32 = 0o700;
    if is_root() {
        return; // root ignores directory permissions
    }
    let dir = temp_dir("flush-fails");
    let path = dir.join("config.json");
    let mgr = ConfigManager::load(&path, &json!({"volume": 0.5})).unwrap();
    mgr.write(|c| c["volume"] = json!(0.9));
    fs::set_permissions(&dir, fs::Permissions::from_mode(READ_EXEC_ONLY)).unwrap();

    let result = mgr.flush();
    assert!(result.is_err(), "flush must report the failed save");
    assert!(
        mgr.modified.load(Ordering::Acquire),
        "a failed flush must leave the config dirty for a retry"
    );

    fs::set_permissions(&dir, fs::Permissions::from_mode(OWNER_FULL)).unwrap();
    mgr.flush().unwrap();
    assert!(!mgr.modified.load(Ordering::Acquire));
    let on_disk: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        on_disk["volume"],
        json!(0.9),
        "the retry persists the change"
    );
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
