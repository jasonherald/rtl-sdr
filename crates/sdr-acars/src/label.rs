//! Label name lookup. Each ACARS message carries a 2-byte
//! label code identifying its category (Q0 = link test, H1 =
//! crew message, B1 = weather, etc.).
//!
//! v1 ships an EMPTY table — `acarsdec` itself doesn't include a
//! code→name dictionary, so there's nothing to verbatim-port for
//! the e2e diff against the reference. Population is queued for
//! the UI sub-project (where names actually get displayed) and
//! tracked in issue #577 (which also covers per-label
//! structured-field parsing). Sources to draw from when the
//! table lands: ARINC 618 spec, sigidwiki ACARS labels page,
//! community wikis.
//!
//! The public API ([`lookup`]) is kept stable now so downstream
//! tasks (Task 8 re-exports, Task 9 CLI, sub-project 3 UI) can
//! call it without code churn when names are added later.

/// Look up the human-readable name for a 2-byte label code.
/// Currently always returns `None` — see module-level docs.
#[must_use]
pub fn lookup(_code: [u8; 2]) -> Option<&'static str> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_none_in_v1() {
        // Pin the v1 stub. Once #577 populates the table this
        // test gets replaced with positive `assert_eq!` checks
        // for known labels (H1, Q0, _d, B1, etc.).
        assert_eq!(lookup(*b"H1"), None);
        assert_eq!(lookup(*b"Q0"), None);
        assert_eq!(lookup([0xFF, 0xFF]), None);
    }
}
