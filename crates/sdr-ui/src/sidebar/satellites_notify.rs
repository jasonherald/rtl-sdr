//! Per-satellite "notify me" scheduler — pure state machine.
//!
//! Companion to [`satellites_recorder`](super::satellites_recorder)
//! that fires a desktop notification at T-`lead_min` for any pass
//! whose satellite is in the watched set. Same separation as the
//! recorder: this module is pure (no GTK, no I/O), `tick()` returns
//! a `Vec<Action>` that the wiring layer in
//! `window.rs::connect_satellites_panel` interprets.
//!
//! The shape mirrors the recorder for two reasons:
//!
//! 1. **Unit-testable.** Pass synthesis + a `chrono::DateTime`
//!    advance trick lets every threshold case live in
//!    `#[cfg(test)] mod tests` without spinning up a GTK harness
//!    or a notification daemon.
//! 2. **One firing per pass.** The same dedup discipline the
//!    recorder uses for AOS / LOS — a `HashSet<NotifyKey>` keyed by
//!    `(norad_id, pass.start)` — keeps "in window for 5 min" from
//!    spamming five notifications.
//!
//! Per #510.
//!
//! ## Notification semantics
//!
//! For each watched satellite whose next pass crosses
//! `(start - lead) <= now < start`, fire **once** per pass.
//! The wiring layer is responsible for translating each
//! `Action::Fire` into a desktop notification via
//! [`crate::notify`]; this module only decides *when* and
//! *what content* (passing the `Pass` and a copy of the
//! catalog name through unchanged).
//!
//! ## GC
//!
//! `fired` is pruned every [`tick`](NotifyScheduler::tick): an
//! entry is dropped once `pass.start + GC_GRACE` is in the past.
//! `GC_GRACE` is sized to outlast any realistic LEO pass plus a
//! margin (30 min today) so an in-window notification can't get a
//! duplicate fire from the GC racing the next tick. The set's
//! steady-state size is bounded by "concurrently in-flight or
//! upcoming passes" — typically a handful at most. Without GC the
//! set would grow unbounded over a long-running session.

use std::collections::HashSet;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sdr_sat::Pass;

/// Default lead-time before AOS at which the notification fires.
/// Per #510: "T-5 minutes is a reasonable default. Worth making it
/// a single config knob".
pub const DEFAULT_NOTIFY_LEAD_MIN: u32 = 5;

/// Lower / upper bounds on the user-configurable lead time (minutes).
/// Lower bound `1` keeps the notification meaningful (anything less
/// gives the user no time to react). Upper bound `60` exceeds any
/// realistic LEO pre-pass prep window.
pub const NOTIFY_LEAD_MIN_LOWER: u32 = 1;
pub const NOTIFY_LEAD_MIN_UPPER: u32 = 60;

/// Dedup key for the `fired` set. `(norad_id, pass_start_unix)`
/// uniquely identifies a pass — two passes of the same satellite
/// can't share a start time (they're sorted in the SGP4 output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NotifyKey {
    norad_id: u32,
    pass_start_unix: i64,
}

/// Action returned by [`NotifyScheduler::tick`]. The wiring layer
/// translates each action into a `gio::Notification`. Not
/// `PartialEq` because [`sdr_sat::Pass`] isn't — tests pattern-match
/// on individual fields instead of comparing whole actions.
#[derive(Debug, Clone)]
pub enum Action {
    /// Fire a "satellite overhead in N minutes" notification. The
    /// satellite display name is carried on `pass.satellite` — no
    /// separate `sat_name` field, since duplicating it would
    /// allocate the same string twice per fire (once on the Pass
    /// clone, once on the field) for no payoff. Per CR round 2 on
    /// PR #568.
    Fire {
        /// NORAD catalog id — passed through to the notification's
        /// "Tune" action target so the action handler can look the
        /// satellite up in `KNOWN_SATELLITES` for downlink / mode /
        /// bandwidth.
        norad_id: u32,
        /// The pass itself — the wiring layer reads `satellite`,
        /// `start`, `max_elevation_deg`, `start_az_deg`,
        /// `end_az_deg` from it for the notification body.
        pass: Pass,
        /// Lead-time in minutes the user configured. Carried so
        /// the notification body can read "in 5 min" rather than
        /// recomputing from `pass.start - now` (and risking a
        /// "in 4 min" rendering when the tick arrives 30 s late).
        lead_min: u32,
    },
}

/// State machine for per-pass notifications.
///
/// Single-threaded — driven from the GTK main loop only.
#[derive(Debug, Default)]
pub struct NotifyScheduler {
    fired: HashSet<NotifyKey>,
}

impl NotifyScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Per-tick driver. Returns the actions to fire this tick.
    ///
    /// * `now` — current UTC time. Caller passes the same `now` it
    ///   uses for the rest of the tick so render and notify decisions
    ///   stay consistent.
    /// * `lead` — configured lead time. Stored as a `ChronoDuration`
    ///   rather than `u32 minutes` so unit tests can poke unusual
    ///   values (e.g. zero-duration to verify the boundary).
    /// * `lead_min` — same lead time in whole minutes, threaded into
    ///   `Action::Fire` so the body text doesn't have to re-derive
    ///   it from `lead.num_minutes()`.
    /// * `is_watched` — predicate against the user's watched-set.
    ///   A predicate (rather than a `&HashSet<u32>`) keeps the
    ///   module independent of how the wiring layer represents the
    ///   set (`Vec<u32>` from config vs. `HashSet<u32>` for
    ///   membership) and keeps the test cases trivial.
    /// * `passes` — the upcoming pass list, in any order. Each
    ///   pass's `satellite` name is looked up against
    ///   [`KNOWN_SATELLITES`](sdr_sat::KNOWN_SATELLITES) by the
    ///   wiring layer to map name → NORAD id; here we receive the
    ///   already-mapped `(norad_id, pass)` pairs.
    pub fn tick<'a, I, F>(
        &mut self,
        now: DateTime<Utc>,
        lead: ChronoDuration,
        lead_min: u32,
        passes: I,
        is_watched: F,
    ) -> Vec<Action>
    where
        I: IntoIterator<Item = (u32, &'a Pass)>,
        F: Fn(u32) -> bool,
    {
        // GC first so the dedup check below sees an up-to-date set.
        self.fired.retain(|key| {
            // We don't carry pass.end in the key, so we approximate:
            // a key is stale once its `pass_start_unix` is more than
            // a generous LEO-pass duration in the past. 30 minutes
            // covers the longest realistic LEO horizon-to-horizon
            // pass and avoids a name-lookup on every tick. The
            // looseness is fine — false positives only delay GC by
            // at most one full pass duration; they don't gate
            // anything.
            now.timestamp() - key.pass_start_unix < GC_GRACE.num_seconds()
        });

        let mut actions = Vec::new();
        for (norad_id, pass) in passes {
            if !is_watched(norad_id) {
                continue;
            }
            let to_start = pass.start - now;
            // Window: `0 < to_start <= lead`. Excluding zero / negative
            // means we don't fire for passes already in progress —
            // the user wanted advance notice, and an in-progress pass
            // is too late for "in N min" to make sense.
            if to_start <= ChronoDuration::zero() || to_start > lead {
                continue;
            }
            let key = NotifyKey {
                norad_id,
                pass_start_unix: pass.start.timestamp(),
            };
            if !self.fired.insert(key) {
                continue;
            }
            actions.push(Action::Fire {
                norad_id,
                pass: pass.clone(),
                lead_min,
            });
        }
        actions
    }
}

/// GC grace window — a fired entry is dropped this long after the
/// pass started. Sized to outlast any realistic LEO pass plus a
/// margin so an in-window notification never gets a duplicate fire
/// from the GC racing the next tick. Pure bookkeeping.
const GC_GRACE: ChronoDuration = ChronoDuration::minutes(30);

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn synthetic_pass(satellite: &str, start: DateTime<Utc>, duration_min: i64) -> Pass {
        Pass {
            satellite: satellite.to_string(),
            start,
            end: start + ChronoDuration::minutes(duration_min),
            max_elevation_deg: 56.0,
            max_el_time: start + ChronoDuration::minutes(duration_min / 2),
            start_az_deg: 245.0,
            end_az_deg: 105.0,
        }
    }

    fn anchor() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()
    }

    #[test]
    fn fires_when_within_lead_window() {
        let now = anchor();
        let lead = ChronoDuration::minutes(5);
        // Pass starts in 4 min — inside the 5 min lead window.
        let pass = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(4), 12);
        let mut sched = NotifyScheduler::new();
        let actions = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| true);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Fire {
                norad_id,
                pass,
                lead_min,
            } => {
                assert_eq!(*norad_id, 33591);
                assert_eq!(pass.satellite, "NOAA 19");
                assert_eq!(*lead_min, 5);
            }
        }
    }

    #[test]
    fn does_not_fire_outside_window() {
        let now = anchor();
        let lead = ChronoDuration::minutes(5);
        // Pass starts in 10 min — well outside the 5 min lead.
        let pass = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(10), 12);
        let mut sched = NotifyScheduler::new();
        let actions = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| true);
        assert!(actions.is_empty());
    }

    #[test]
    fn does_not_fire_for_unwatched_satellite() {
        let now = anchor();
        let lead = ChronoDuration::minutes(5);
        let pass = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(4), 12);
        let mut sched = NotifyScheduler::new();
        // is_watched returns false → no action.
        let actions = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| false);
        assert!(actions.is_empty());
    }

    #[test]
    fn dedups_across_consecutive_ticks() {
        let mut now = anchor();
        let lead = ChronoDuration::minutes(5);
        let pass = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(4), 12);
        let mut sched = NotifyScheduler::new();

        // First tick — fires.
        let a1 = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| true);
        assert_eq!(a1.len(), 1);

        // Advance one second, same pass — must NOT re-fire.
        now += ChronoDuration::seconds(1);
        let a2 = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| true);
        assert!(a2.is_empty(), "second tick re-fired: {a2:?}");

        // Advance to T+1 min into the pass — must NOT fire again.
        now = pass.start + ChronoDuration::minutes(1);
        let a3 = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| true);
        assert!(a3.is_empty());
    }

    #[test]
    fn does_not_fire_for_pass_already_in_progress() {
        let now = anchor();
        let lead = ChronoDuration::minutes(5);
        // Pass started 30 s ago — `to_start` is negative.
        let pass = synthetic_pass("NOAA 19", now - ChronoDuration::seconds(30), 12);
        let mut sched = NotifyScheduler::new();
        let actions = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| true);
        assert!(actions.is_empty());
    }

    #[test]
    fn fires_again_for_a_later_pass_of_same_satellite() {
        let mut now = anchor();
        let lead = ChronoDuration::minutes(5);
        let mut sched = NotifyScheduler::new();

        // First pass — starts in 4 min.
        let pass_a = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(4), 12);
        let a1 = sched.tick(now, lead, 5, [(33591u32, &pass_a)], |_| true);
        assert_eq!(a1.len(), 1);

        // 100 minutes later — first pass long over, GC has dropped it,
        // and a new pass starts in 4 min. Must fire — different
        // pass, different key, so the dedup set lets it through.
        now += ChronoDuration::minutes(100);
        let pass_b = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(4), 12);
        let a2 = sched.tick(now, lead, 5, [(33591u32, &pass_b)], |_| true);
        assert_eq!(a2.len(), 1, "second pass did not fire: {a2:?}");
    }

    #[test]
    fn boundary_at_exactly_lead_minutes_fires() {
        let now = anchor();
        let lead = ChronoDuration::minutes(5);
        // Exactly `lead` away — `to_start <= lead` includes the
        // upper boundary on purpose. Without this, a pass at exactly
        // T-5:00 would skip the only tick where it's in range.
        let pass = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(5), 12);
        let mut sched = NotifyScheduler::new();
        let actions = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| true);
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn handles_multiple_watched_passes_in_one_tick() {
        let now = anchor();
        let lead = ChronoDuration::minutes(5);
        let pass_a = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(2), 12);
        let pass_b = synthetic_pass("METEOR-M2 2", now + ChronoDuration::minutes(3), 12);
        let mut sched = NotifyScheduler::new();
        let watched: HashSet<u32> = [33591u32, 40069u32].into_iter().collect();
        let actions = sched.tick(
            now,
            lead,
            5,
            [(33591u32, &pass_a), (40069u32, &pass_b)],
            |id| watched.contains(&id),
        );
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn gc_drops_long_past_entries() {
        let mut now = anchor();
        let lead = ChronoDuration::minutes(5);
        let mut sched = NotifyScheduler::new();

        let pass = synthetic_pass("NOAA 19", now + ChronoDuration::minutes(4), 12);
        let _ = sched.tick(now, lead, 5, [(33591u32, &pass)], |_| true);
        assert_eq!(sched.fired.len(), 1);

        // Advance to >GC_GRACE past `pass.start` (not past `now`):
        // the GC predicate measures elapsed time since the pass
        // started, so we need `now - pass.start >= GC_GRACE`.
        now = pass.start + GC_GRACE + ChronoDuration::minutes(1);
        let _: Vec<Action> = sched.tick(now, lead, 5, std::iter::empty(), |_| true);
        assert!(sched.fired.is_empty(), "GC did not drop stale entry");
    }
}
