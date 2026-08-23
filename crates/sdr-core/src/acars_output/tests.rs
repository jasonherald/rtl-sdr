use std::fs::File;
use std::io::{BufRead, BufReader};
use std::net::UdpSocket;
use std::time::{Duration, UNIX_EPOCH};

use arrayvec::ArrayString;
use sdr_acars::AcarsMessage;
use serde_json::Value;
use tempfile::tempdir;

use std::sync::{Arc, RwLock, mpsc};

use super::*;

fn make_msg(channel: u8) -> AcarsMessage {
    AcarsMessage {
        timestamp: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        channel_idx: channel,
        freq_hz: 131_550_000.0,
        level_db: 10.0,
        error_count: 0,
        mode: b'2',
        label: *b"H1",
        block_id: 0,
        ack: 0x15,
        aircraft: ArrayString::from(".N12345").unwrap(),
        flight_id: None,
        message_no: None,
        text: String::new(),
        end_of_message: true,
        reassembled_block_count: 1,
        parsed: None,
    }
}

#[test]
fn jsonl_writer_round_trip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("acars.jsonl");
    let mut writer = JsonlWriter::open(&path).unwrap();
    writer.write(&make_msg(2), Some("STN1")).unwrap();
    writer.flush().unwrap();
    drop(writer);

    let f = File::open(&path).unwrap();
    let mut lines = BufReader::new(f).lines();
    let line = lines.next().unwrap().unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["channel"].as_u64().unwrap(), 2);
    assert_eq!(v["station_id"].as_str().unwrap(), "STN1");
    assert!(lines.next().is_none());
}

#[test]
fn jsonl_writer_appends_across_writes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("acars.jsonl");
    let mut writer = JsonlWriter::open(&path).unwrap();
    writer.write(&make_msg(0), None).unwrap();
    writer.write(&make_msg(1), None).unwrap();
    writer.write(&make_msg(2), None).unwrap();
    writer.flush().unwrap();
    drop(writer);

    let f = File::open(&path).unwrap();
    let lines: Vec<_> = BufReader::new(f).lines().collect::<Result<_, _>>().unwrap();
    assert_eq!(lines.len(), 3);
    for (i, line) in lines.iter().enumerate() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["channel"].as_u64().unwrap(), i as u64);
    }
}

#[test]
fn jsonl_writer_open_creates_parent_dirs() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("nested").join("subdir").join("acars.jsonl");
    let writer = JsonlWriter::open(&path).unwrap();
    assert_eq!(writer.path(), path);
    assert!(path.exists());
}

#[test]
fn udp_feeder_round_trip() {
    // Bind a listener on loopback ephemeral port, open a
    // feeder pointed at it, send one message, recv it,
    // parse the JSON.
    let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
    let listener_addr = listener.local_addr().unwrap();
    let addr_str = format!("127.0.0.1:{}", listener_addr.port());

    let feeder = UdpFeeder::open(&addr_str).unwrap();
    feeder.send(&make_msg(2), Some("STN1")).unwrap();

    let mut buf = vec![0u8; 4096];
    let (n, _from) = listener.recv_from(&mut buf).unwrap();
    let payload = std::str::from_utf8(&buf[..n]).unwrap();
    // Strip trailing newline.
    let json_str = payload.trim_end_matches('\n');
    let v: Value = serde_json::from_str(json_str).unwrap();
    assert_eq!(v["channel"].as_u64().unwrap(), 2);
    assert_eq!(v["station_id"].as_str().unwrap(), "STN1");
    assert_eq!(feeder.addr_str(), &addr_str);
}

#[test]
fn udp_feeder_open_invalid_addr_errors() {
    // Missing port.
    assert!(UdpFeeder::open("not-a-host").is_err());
    // Invalid port.
    assert!(UdpFeeder::open("127.0.0.1:notaport").is_err());
    // Unresolvable host.
    // Use .invalid TLD per RFC 6761 — guaranteed to never resolve.
    assert!(UdpFeeder::open("nonexistent.invalid:5550").is_err());
}

/// #701 — a poisoned warn-rate-limit mutex must not panic the DSP
/// thread; recover the guard and keep going.
#[test]
#[allow(clippy::panic)] // the panic is the poison injection
fn try_send_survives_poisoned_warn_lock() {
    let outputs = AcarsOutputs::with_capacity_for_test(1);
    let lock = Arc::clone(&outputs.last_drop_warn_at);
    let poisoner = std::thread::spawn(move || {
        let _guard = lock.lock().unwrap();
        panic!("poison the warn lock on purpose");
    });
    assert!(
        poisoner.join().is_err(),
        "test premise: the lock is poisoned"
    );

    assert!(outputs.try_send(make_msg(0)));
    // Channel full → drop path → maybe_warn_full takes the poisoned lock.
    assert!(!outputs.try_send(make_msg(0)));
    assert_eq!(outputs.drop_count(), 1);
}

/// #701 — a poisoned writer config lock must not kill the writer
/// thread; it recovers the inner value and keeps serving messages.
#[test]
#[allow(clippy::panic)] // the panic is the poison injection
fn writer_thread_survives_poisoned_config_lock() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("poisoned.jsonl");
    let config = Arc::new(RwLock::new(AcarsWriterConfig {
        jsonl_path: Some(path.clone()),
        network_addr: None,
        station_id: None,
    }));
    let poison = Arc::clone(&config);
    let poisoner = std::thread::spawn(move || {
        let _guard = poison.write().unwrap();
        panic!("poison the config lock on purpose");
    });
    assert!(
        poisoner.join().is_err(),
        "test premise: the lock is poisoned"
    );

    let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(8);
    let (dsp_tx, _dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
    let worker = Arc::clone(&config);
    let handle = std::thread::spawn(move || run_writer_loop(rx, worker, dsp_tx));
    tx.send(AcarsOutputMessage::Decoded(make_msg(0))).unwrap();
    tx.send(AcarsOutputMessage::Shutdown).unwrap();
    handle
        .join()
        .expect("writer thread must survive a poisoned config lock");
    let lines: Vec<_> = BufReader::new(File::open(&path).unwrap())
        .lines()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        lines.len(),
        1,
        "the message after the poison is still written"
    );
}

/// #702 — a failing open must not be retried (with a warn + toast)
/// on every message; it backs off until the interval elapses or the
/// target changes.
#[test]
fn ensure_jsonl_backs_off_after_a_failed_open() {
    let dir = tempdir().unwrap();
    // A path *under a regular file* can never be created.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let bad = blocker.join("out.jsonl");

    let (dsp_tx, dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
    let mut slot = None;
    let mut backoff = OpenBackoff::default();
    ensure_jsonl(&mut slot, Some(&bad), &mut backoff, &dsp_tx);
    ensure_jsonl(&mut slot, Some(&bad), &mut backoff, &dsp_tx);
    ensure_jsonl(&mut slot, Some(&bad), &mut backoff, &dsp_tx);
    let toasts = dsp_rx.try_iter().count();
    assert_eq!(
        toasts, 1,
        "one toast per backoff window, not one per message"
    );
    assert!(slot.is_none());

    // A different target retries immediately.
    let good = dir.path().join("ok.jsonl");
    ensure_jsonl(&mut slot, Some(&good), &mut backoff, &dsp_tx);
    assert!(slot.is_some(), "a new target is tried right away");
    assert_eq!(dsp_rx.try_iter().count(), 0);
}

/// #702 — an explicit config change resets the backoff so the user's
/// action gets an immediate retry.
#[test]
fn open_backoff_reset_forces_a_retry() {
    let dir = tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let bad = blocker.join("out.jsonl");
    let (dsp_tx, dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
    let mut slot = None;
    let mut backoff = OpenBackoff::default();
    ensure_jsonl(&mut slot, Some(&bad), &mut backoff, &dsp_tx);
    assert_eq!(dsp_rx.try_iter().count(), 1);
    backoff.reset();
    ensure_jsonl(&mut slot, Some(&bad), &mut backoff, &dsp_tx);
    assert_eq!(
        dsp_rx.try_iter().count(),
        1,
        "retry after reset surfaces again"
    );
}

#[test]
fn writer_thread_exits_on_disconnect() {
    // Spawn a writer thread, drop the sender, assert the
    // thread joins within a short timeout. Exercises the
    // recv() returning Err(Disconnected) → loop break path.
    let config = Arc::new(RwLock::new(AcarsWriterConfig::default()));
    let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(8);
    let (dummy_dsp_tx, _dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
    let handle = std::thread::spawn(move || {
        run_writer_loop(rx, Arc::clone(&config), dummy_dsp_tx);
    });
    drop(tx);
    // Loop should exit promptly. Allow up to 500 ms for
    // schedulability under loaded test workers.
    let start = std::time::Instant::now();
    while !handle.is_finished() && start.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        handle.is_finished(),
        "writer thread did not exit within 500ms of tx drop"
    );
    handle.join().expect("writer thread panicked");
}

#[test]
fn try_send_drops_when_channel_full() {
    // Build an AcarsOutputs against a tiny channel cap (8)
    // by spawning *no* worker — leave the receiver dangling
    // so the channel fills from the first send. The 9th
    // try_send should drop.
    //
    // `AcarsOutputs::with_capacity` is a test-visible
    // constructor that lets tests use a smaller cap than
    // the production 256.
    let outputs = AcarsOutputs::with_capacity_for_test(8);

    for _ in 0..8 {
        assert!(outputs.try_send(make_msg(0)));
    }
    // 9th send: channel full, drop returns false, counter
    // increments.
    assert!(!outputs.try_send(make_msg(0)));
    assert_eq!(outputs.drop_count(), 1);
}

#[test]
fn writer_reopens_on_path_change() {
    // Pump message → path A; switch config to path B; pump
    // message → path B. Assert both files exist with the
    // expected line count.
    let dir = tempdir().unwrap();
    let path_a = dir.path().join("a.jsonl");
    let path_b = dir.path().join("b.jsonl");

    let config = Arc::new(RwLock::new(AcarsWriterConfig {
        jsonl_path: Some(path_a.clone()),
        network_addr: None,
        station_id: None,
    }));
    let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(8);
    let (dummy_dsp_tx, _dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
    let handle = {
        let config = Arc::clone(&config);
        std::thread::spawn(move || run_writer_loop(rx, config, dummy_dsp_tx))
    };

    tx.send(AcarsOutputMessage::Decoded(make_msg(0))).unwrap();

    // Spin briefly to let the writer process the first
    // message before we mutate the path.
    std::thread::sleep(Duration::from_millis(50));

    config.write().unwrap().jsonl_path = Some(path_b.clone());
    tx.send(AcarsOutputMessage::Decoded(make_msg(1))).unwrap();

    // Drop tx → thread exits; flush on Drop ensures the
    // BufWriter contents land on disk before we read.
    drop(tx);
    handle.join().expect("writer thread panicked");

    let read_lines = |p: &Path| -> Vec<String> {
        let f = File::open(p).unwrap();
        BufReader::new(f).lines().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(read_lines(&path_a).len(), 1, "path A got the first message");
    assert_eq!(
        read_lines(&path_b).len(),
        1,
        "path B got the second message"
    );
}

#[test]
fn config_changed_signal_wakes_idle_writer() {
    // Verifies the CR round 1 fix on PR #598: send
    // ConfigChanged with no preceding Decoded; the worker
    // re-snapshots config and calls ensure_jsonl, which
    // opens the file in append mode and creates it. Without
    // the fix the worker would only wake on Decoded and the
    // file would never appear.
    let dir = tempdir().unwrap();
    let path_a = dir.path().join("idle_open.jsonl");

    let config = Arc::new(RwLock::new(AcarsWriterConfig {
        jsonl_path: Some(path_a.clone()),
        network_addr: None,
        station_id: None,
    }));
    let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(8);
    let (dummy_dsp_tx, _dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
    let handle = {
        let config = Arc::clone(&config);
        std::thread::spawn(move || run_writer_loop(rx, config, dummy_dsp_tx))
    };

    // No Decoded — only ConfigChanged. The worker must wake,
    // resnap config, and open path_a. JsonlWriter::open in
    // append-mode creates the file even with no writes.
    tx.send(AcarsOutputMessage::ConfigChanged).unwrap();

    // Spin briefly to let the worker process ConfigChanged.
    let start = std::time::Instant::now();
    while !path_a.exists() && start.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        path_a.exists(),
        "ConfigChanged should have caused the writer to open path A even with no Decoded messages"
    );

    // Clean shutdown via Shutdown sentinel — exercises the
    // explicit-shutdown arm.
    tx.send(AcarsOutputMessage::Shutdown).unwrap();
    drop(tx);
    handle.join().expect("writer thread panicked");
}
