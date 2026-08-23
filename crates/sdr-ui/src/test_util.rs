//! Shared helpers for the crate's unit tests.

use std::io::Read;
use std::path::PathBuf;

/// A file path inside a fresh private temp directory; keep the
/// returned `TempDir` alive for the test's duration (the directory —
/// and anything written under it — is removed when it drops).
pub(crate) fn test_output_path(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("sdr-ui-test-")
        .tempdir()
        .expect("test temp dir");
    let path = dir.path().join(name);
    (dir, path)
}

/// Assert `path` holds a non-empty file that starts with the 8-byte
/// PNG signature.
pub(crate) fn assert_png_file(path: &std::path::Path) {
    let metadata = std::fs::metadata(path).expect("exported file exists");
    assert!(metadata.len() > 0, "PNG file shouldn't be empty");
    let mut header = [0_u8; 8];
    let mut f = std::fs::File::open(path).expect("open exported file");
    f.read_exact(&mut header).expect("read PNG header");
    assert_eq!(
        &header, b"\x89PNG\r\n\x1a\n",
        "exported file isn't a valid PNG (header mismatch)",
    );
}
