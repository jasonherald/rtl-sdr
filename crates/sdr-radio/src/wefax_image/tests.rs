use super::*;

/// Width used by all tests — matches IOC 576 WEFAX at the
/// conventional 1809-pixel line width (real charts vary; the
/// buffer doesn't care as long as writes are consistent).
const W: u32 = 1809;

fn row(value: u8) -> Vec<u8> {
    vec![value; W as usize]
}

#[test]
fn write_then_snapshot_roundtrips_rows() {
    let img = WefaxImage::new();
    let h = img.handle();
    let r = row(128);
    h.write_line(0, W, &r);
    h.write_line(1, W, &r);
    let snap = h.snapshot().expect("snapshot after writes");
    assert_eq!(snap.width, W);
    assert!(snap.height >= 2);
}

#[test]
fn take_completed_swaps_and_resets() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, W, &row(200));
    let done = h.take_completed().expect("completed image");
    assert_eq!(done.width, W);
    assert!(h.take_completed().is_none(), "reset after take");
}

#[test]
fn snapshot_returns_none_before_any_write() {
    let img = WefaxImage::new();
    let h = img.handle();
    assert!(h.snapshot().is_none());
}

#[test]
fn take_completed_returns_none_before_any_write() {
    let img = WefaxImage::new();
    let h = img.handle();
    assert!(h.take_completed().is_none());
}

#[test]
fn write_line_content_is_preserved() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, W, &row(42));
    let snap = h.snapshot().expect("snapshot present");
    assert_eq!(snap.pixels[0], 42);
    assert_eq!(snap.pixels[W as usize - 1], 42);
}

#[test]
fn take_completed_drains_full_pixel_buffer() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, W, &row(10));
    h.write_line(1, W, &row(20));
    let done = h.take_completed().expect("completed image");
    assert_eq!(done.width, W);
    assert_eq!(done.height, 2);
    assert_eq!(done.pixels.len(), (W * 2) as usize);
    assert_eq!(done.pixels[0], 10);
    assert_eq!(done.pixels[W as usize], 20);
}

#[test]
fn clones_share_same_buffer() {
    let img = WefaxImage::new();
    let a = img.handle();
    let b = a.clone();
    a.write_line(0, W, &row(77));
    let snap = b.snapshot().expect("clone sees write");
    assert_eq!(snap.pixels[0], 77);
}

#[test]
fn clear_resets_without_returning_pixels() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, W, &row(5));
    h.clear();
    assert!(h.snapshot().is_none());
}

#[test]
fn owner_clear_convenience_wrapper_resets_handle() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, W, &row(5));
    img.clear();
    assert!(h.snapshot().is_none());
}

/// Duplicate writes for the same row must not advance the
/// written-line counter — idempotent pixel write, mirroring the
/// SSTV image handle's regression coverage (PR #599).
#[test]
fn duplicate_write_does_not_advance_counter() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, W, &row(9));
    h.write_line(0, W, &row(9));
    let snap = h.snapshot().expect("snapshot present");
    assert_eq!(snap.height, 1, "duplicate write must not grow height twice");
}

/// Out-of-order writes must grow the buffer to the highest row
/// index seen and fill any skipped rows rather than panicking.
#[test]
fn out_of_order_writes_grow_to_highest_row() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(5, W, &row(1));
    h.write_line(2, W, &row(2));
    let snap = h.snapshot().expect("snapshot present");
    assert_eq!(snap.height, 6, "height must reach highest row index + 1");
    assert_eq!(snap.pixels[2 * W as usize], 2);
    assert_eq!(snap.pixels[5 * W as usize], 1);
}

#[test]
fn to_flat_gray_has_expected_length() {
    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, W, &row(3));
    h.write_line(1, W, &row(4));
    let done = h.take_completed().expect("completed image");
    let flat = done.to_flat_gray();
    assert_eq!(flat.len(), (W * 2) as usize);
    assert_eq!(flat[0], 3);
    assert_eq!(flat[W as usize], 4);
}

#[test]
#[allow(
    clippy::panic,
    reason = "test deliberately panics on a worker thread to poison the mutex; the panic is the test fixture"
)]
fn recovers_from_poisoned_mutex() {
    use std::sync::Arc;
    use std::thread;

    let img = WefaxImage::new();
    let h = img.handle();
    h.write_line(0, W, &row(11));

    let inner = Arc::clone(&h.inner);
    let _ = thread::spawn(move || {
        let _guard = inner.lock().expect("first lock");
        panic!("intentional panic to poison the mutex");
    })
    .join();

    assert!(h.inner.is_poisoned(), "test setup: mutex must be poisoned");

    let snap = h.snapshot().expect("snapshot post-poison");
    assert_eq!(snap.pixels[0], 11);
    h.write_line(1, W, &row(22));
    let snap2 = h.snapshot().expect("write post-poison");
    assert_eq!(snap2.pixels[W as usize], 22);
}
