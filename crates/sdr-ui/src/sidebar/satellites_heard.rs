//! "Heard via Orbcomm" session model (issue #865, Task 12).
//!
//! Pure, GTK-free tracker of every spacecraft `sat_id` seen on the
//! Orbcomm downlink this session: when it was last heard, and its
//! last-known position (if any Ephemeris packet has ever decoded
//! for it — a Sync beacon carries no position of its own). Mirrors
//! `satellites_recorder`'s split: state lives here so it stays
//! unit-testable without a GTK harness; the Satellites panel's
//! `AdwPreferencesGroup` rebuild lives in
//! `window/satellites/heard.rs`.
//!
//! `record` is called from `window/dsp_events/orbcomm_events.rs` on
//! every `Sync` / `Ephemeris` packet; `rows` is read by the wiring
//! layer's 5 s tick (and on-event rebuild) to repaint the group.

use std::collections::HashMap;
use std::time::Instant;

/// A satellite drops off the "Heard via Orbcomm" list once it's been
/// silent this long. 20 minutes covers one Orbcomm pass plus margin
/// (LEO passes over a fixed ground station run a few minutes; several
/// satellites are usually in view across a 20-minute window), so an
/// entry surviving the whole window without a repeat hit is genuinely
/// gone rather than just between packets.
pub const HEARD_EXPIRY_SECS: u64 = 1200;

/// One row of the "Heard via Orbcomm" panel group: a display label,
/// how long ago the satellite was last heard, and its last-known
/// position (`None` until an Ephemeris packet has decoded for it).
#[derive(Debug, Clone, PartialEq)]
pub struct HeardRow {
    /// Display label — always [`sdr_orbcomm::sat_names::sat_label`]'s
    /// output ("Sat 0xNN"). No spacecraft-name table exists yet
    /// (see that function's docs).
    pub label: String,
    /// Seconds since this satellite was last heard, relative to the
    /// `now` passed to [`HeardSatellites::rows`].
    pub age_secs: u64,
    /// Last-known `(lat_deg, lon_deg, alt_m)`, or `None` if only
    /// Sync beacons (no position) have been heard so far.
    pub position: Option<(f64, f64, f64)>,
    /// Last-known ground-track velocity in m/s from an Ephemeris
    /// packet, or `None` if none has decoded yet. Preserved across
    /// subsequent Sync-only beacons, same as `position`.
    pub vel_ms: Option<f64>,
    /// Last-known satellite-reported clock, as a Unix timestamp, from
    /// an Ephemeris packet, or `None` if none has decoded yet.
    /// Preserved across subsequent Sync-only beacons, same as
    /// `position`.
    pub sat_time_unix: Option<i64>,
}

/// Per-satellite tracking state. Not `pub` — callers only see the
/// [`HeardRow`] projection through [`HeardSatellites::rows`].
struct Entry {
    position: Option<(f64, f64, f64)>,
    vel_ms: Option<f64>,
    sat_time_unix: Option<i64>,
    last_heard: Instant,
}

/// Session-scoped "heard via Orbcomm" tracker, keyed by `sat_id`.
/// `sat_id` is a `u8`, so the table is naturally bounded to 256
/// entries even without pruning; [`HeardSatellites::record`] also
/// opportunistically drops expired entries so a long-running session
/// doesn't carry stale rows forward indefinitely between rebuilds.
#[derive(Default)]
pub struct HeardSatellites {
    entries: HashMap<u8, Entry>,
}

impl HeardSatellites {
    /// New, empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a packet heard from `sat_id` at `now`. `position`,
    /// `vel_ms`, and `sat_time_unix` are `Some` for an Ephemeris
    /// packet (which carries a fix, ground-track velocity, and the
    /// satellite's own clock) and `None` for a Sync beacon (which
    /// carries none of the three) — a `None` field always keeps
    /// whatever value was already known while still refreshing the
    /// last-heard time, so a satellite doesn't lose its last fix just
    /// because the next packet happened to be a Sync beacon.
    pub fn record(
        &mut self,
        sat_id: u8,
        position: Option<(f64, f64, f64)>,
        vel_ms: Option<f64>,
        sat_time_unix: Option<i64>,
        now: Instant,
    ) {
        // Opportunistic prune on every write, so a long session
        // doesn't carry expired entries forward between rebuilds.
        // `rows` also filters by age, so this is a memory-bound
        // optimization rather than a correctness requirement.
        self.entries.retain(|_, entry| {
            now.saturating_duration_since(entry.last_heard).as_secs() < HEARD_EXPIRY_SECS
        });

        let entry = self.entries.entry(sat_id).or_insert(Entry {
            position: None,
            vel_ms: None,
            sat_time_unix: None,
            last_heard: now,
        });
        if position.is_some() {
            entry.position = position;
        }
        if vel_ms.is_some() {
            entry.vel_ms = vel_ms;
        }
        if sat_time_unix.is_some() {
            entry.sat_time_unix = sat_time_unix;
        }
        entry.last_heard = now;
    }

    /// Snapshot the currently-heard (not yet expired) satellites,
    /// most-recently-heard first.
    #[must_use]
    pub fn rows(&self, now: Instant) -> Vec<HeardRow> {
        let mut entries: Vec<(u8, &Entry)> = self
            .entries
            .iter()
            .map(|(&sat_id, entry)| (sat_id, entry))
            .filter(|(_, entry)| {
                now.saturating_duration_since(entry.last_heard).as_secs() < HEARD_EXPIRY_SECS
            })
            .collect();
        entries.sort_by_key(|(_, entry)| std::cmp::Reverse(entry.last_heard));

        entries
            .into_iter()
            .map(|(sat_id, entry)| HeardRow {
                label: sdr_orbcomm::sat_names::sat_label(sat_id),
                age_secs: now.saturating_duration_since(entry.last_heard).as_secs(),
                position: entry.position,
                vel_ms: entry.vel_ms,
                sat_time_unix: entry.sat_time_unix,
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests;
