//! Shared error type for the live image viewers (NOAA APT
//! [`crate::apt_viewer`] and Meteor-M LRPT [`crate::lrpt_viewer`]).
//!
//! Both viewers hit the same Cairo + filesystem error categories
//! when rendering and writing PNGs — this module collects the
//! discriminants in one place so callers can match on `Cairo` /
//! `Io` / `EmptyChannel` etc. without parsing strings.
//!
//! ## Why one shared type instead of per-viewer
//!
//! Pre-existing `Result<_, String>` returns (closed by issue
//! [#545](https://github.com/jasonherald/rtl-sdr/issues/545))
//! erased source context — callers couldn't distinguish a
//! transient Cairo failure from a missing-channel domain
//! condition without substring matching. CR's PR #543 review
//! flagged the project guideline that library crates use
//! `thiserror`. The two viewers' error sets overlap completely
//! (both use Cairo, both write PNGs, both have empty / missing
//! domain conditions), so a single `ViewerError` keeps the
//! conversion cheap and avoids duplication. New variants land
//! here when either viewer needs them.
//!
//! ## How to construct a Cairo error
//!
//! Cairo's `cairo::Error` is the common case — many ops can
//! return it (`paint`, `save`, `restore`, `set_source_surface`,
//! `fill`, `ImageSurface::create`, `Context::new`, …).
//! Identify the failing op as a `&'static str` so the
//! `Display` output points the reader at the call site:
//!
//! ```ignore
//! cr.paint().map_err(|e| ViewerError::Cairo {
//!     op: "background paint",
//!     source: e,
//! })?;
//! ```

use std::path::PathBuf;

/// Errors returned by the APT and LRPT image viewers' renderer
/// and PNG-export paths. Constructed at the failing call site
/// so the `Display` output identifies which step failed.
///
/// Marked `#[non_exhaustive]` because the module docs explicitly
/// say new variants land here as either viewer needs them — a
/// future addition would otherwise break exhaustive matches in
/// downstream callers. Per CR on PR #550.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ViewerError {
    /// A Cairo drawing or surface operation failed. `op` names
    /// the failing call (`"paint"`, `"save"`, `"restore"`,
    /// `"set_source_surface"`, `"fill"`, `"export surface"`,
    /// `"export context"`, etc.) so callers and the user-facing
    /// toast can both show the failure site without parsing
    /// stringly-typed messages.
    #[error("Cairo {op} failed: {source}")]
    Cairo {
        /// Identifier of the failing Cairo operation.
        op: &'static str,
        /// Underlying Cairo error.
        #[source]
        source: cairo::Error,
    },

    /// PNG encoder failure — Cairo's [`cairo::IoError`] from
    /// `surface.write_to_png(...)`. Distinct from
    /// [`Self::Io`] because Cairo's PNG IO has its own error
    /// type that wraps both Cairo state errors and `std::io`
    /// errors internally.
    #[error("PNG encoder failed: {0}")]
    PngEncode(#[from] cairo::IoError),

    /// Filesystem operation failed — `std::fs::File::create`
    /// or `std::fs::create_dir_all`. `op` identifies which.
    /// Includes the failing path so the user-facing toast can
    /// show what the export was targeting.
    #[error("filesystem {op} failed for {path:?}: {source}")]
    Io {
        /// Identifier of the failing filesystem operation
        /// (`"create_dir_all"`, `"file create"`, ...).
        op: &'static str,
        /// Path the failing operation was acting on.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Cairo refused to lend the surface's backing data
    /// (`ImageSurface::data()` returned `BorrowError`). Rare
    /// in practice — usually means another borrow is live;
    /// reported separately so the failure mode is greppable.
    #[error("Cairo surface-data lock failed: {0}")]
    SurfaceDataLock(#[from] cairo::BorrowError),

    /// Surface stride couldn't be converted to `usize`. Cairo
    /// returns an `i32` stride; structurally it should always
    /// be non-negative for ARGB32 surfaces but the conversion
    /// is fallible so we surface the failure rather than
    /// `as`-cast.
    #[error("invalid surface stride: {0}")]
    InvalidStride(#[from] std::num::TryFromIntError),

    /// Caller-supplied pixel buffer doesn't match the declared
    /// `width × height`. Carries a description rather than the
    /// raw mismatched lengths because the formatting varies by
    /// caller (greyscale tiles, ARGB32 surfaces, etc.).
    #[error("invalid pixel buffer: {0}")]
    InvalidBuffer(String),

    /// Export requested but the renderer has no active channel
    /// selected (e.g. the LRPT viewer dropdown hasn't picked
    /// one yet). Distinct from [`Self::EmptyChannel`] —
    /// nothing to even point at.
    #[error("no active channel selected for export")]
    NoActiveChannel,

    /// Active channel exists but has no decoded scan lines
    /// yet. `apid` is `Some` for LRPT (per-APID error message),
    /// `None` for APT (single-image protocol). The custom
    /// `Display` formatting elides the parenthetical when
    /// `apid` is `None`.
    #[error(
        "active channel has no data to export{}",
        apid.map(|a| format!(" (APID {a})")).unwrap_or_default()
    )]
    EmptyChannel {
        /// CCSDS APID (the per-channel identifier) for LRPT
        /// exports; `None` for APT (single-image protocol with
        /// no APID concept).
        apid: Option<u16>,
    },

    /// Export dimension exceeds Cairo's `i32` API limit.
    /// `dim` identifies which axis (`"width"`, `"height"`,
    /// `"width × height"` for the multiply-overflow case).
    #[error("export dimension {dim} = {value} exceeds i32::MAX")]
    DimensionTooLarge {
        /// Axis identifier.
        dim: &'static str,
        /// The over-large value.
        value: usize,
    },

    /// Caller passed a zero-sized export (width or height = 0).
    /// Reported separately because Cairo's zero-size error is
    /// distinct from a generic `InvalidBuffer` mismatch and is
    /// easy to match on for the "nothing to export yet" UX.
    #[error("export size is zero (width × height = 0)")]
    ZeroSized,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn empty_channel_with_apid_renders_apid_in_message() {
        let err = ViewerError::EmptyChannel { apid: Some(64) };
        let msg = format!("{err}");
        assert!(msg.contains("APID 64"), "got {msg}");
        assert!(msg.contains("no data to export"), "got {msg}");
    }

    #[test]
    fn empty_channel_without_apid_skips_parenthetical() {
        let err = ViewerError::EmptyChannel { apid: None };
        let msg = format!("{err}");
        assert!(!msg.contains("APID"), "got {msg}");
        assert!(msg.contains("no data to export"), "got {msg}");
    }

    #[test]
    fn cairo_error_display_includes_op_identifier() {
        // Pins the user-facing `Display` contract — the `op`
        // identifier must surface in the `#[error(...)]` string
        // so toasts and log lines name the failing call site.
        // `Debug` is too lenient (always prints field names
        // verbatim) and would mask a future `#[error(...)]`
        // regression. Per CR on PR #550.
        let msg = format!(
            "{}",
            ViewerError::Cairo {
                op: "test paint",
                source: cairo::Error::NoMemory,
            }
        );
        assert!(msg.contains("test paint"), "got {msg}");
    }

    #[test]
    fn no_active_channel_message_is_stable() {
        let err = ViewerError::NoActiveChannel;
        assert_eq!(format!("{err}"), "no active channel selected for export");
    }

    #[test]
    fn dimension_too_large_includes_axis_and_value() {
        let err = ViewerError::DimensionTooLarge {
            dim: "width",
            value: 3_000_000_000,
        };
        let msg = format!("{err}");
        assert!(msg.contains("width"));
        assert!(msg.contains("3000000000"));
    }
}
