//! Pure WEFAX auto-catch state machine (epic/ticket #913).
//!
//! `tick()` takes the observable world in and returns [`Action`]s the UI
//! layer interprets. No GTK, no I/O — mirrors
//! [`super::satellites_recorder::AutoRecorder`]'s `State`/`Action`/`tick`
//! pattern so the transition logic stays unit-testable without a GTK
//! harness.
//!
//! ```text
//! Idle ──(enable)──▶ Scanning ──(fax_present)──▶ Locked ──(Imaging)──▶ Imaging
//!   ▲                   ▲                            │                    │
//!   │                   └──(false-lock timeout)───────┘                    │
//!   │                   └──(presence loss mid-image)────────────────────────┘
//!   └──(disable, from any state)
//! ```
//!
//! `Locked` on chart-complete (`WefaxState::Stopped`) stays `Locked` on the
//! same channel rather than restarting a fresh scan rotation — the station
//! is very likely to send another chart on the same frequency shortly.

use chrono::TimeZone;
use sdr_core::messages::WefaxState;
use sdr_sat::{WefaxStation, is_active, stations_by_distance};
use std::path::PathBuf;

/// Dwell (in ticks) per candidate channel while scanning.
pub const SCAN_DWELL_TICKS: u32 = 3;
/// Ticks a Locked channel may go without imaging before it's a false lock.
pub const LOCK_TIMEOUT_TICKS: u32 = 10;

/// Which station(s) to scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StationChoice {
    /// Scan every known station, nearest-first.
    #[default]
    Auto,
    /// Restrict the rotation to a single named station.
    Pinned(&'static str),
}

/// A snapshot of the user's tune, restored when the catcher stops.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedTune {
    /// The RF center frequency the user was tuned to before auto-catch.
    pub center_hz: f64,
    /// The demod mode encoded as a small integer; the interpret layer maps it.
    pub demod_mode: u8,
    /// Whether the user's own tune was already WEFAX before auto-catch ran.
    pub was_wefax: bool,
}

/// One scan candidate = a station's single channel.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// The station this candidate channel belongs to.
    pub station: &'static str,
    /// The candidate's RF frequency, Hz.
    pub freq_hz: u64,
}

/// Inputs to one tick.
pub struct TickCtx {
    /// Whether auto-catch is toggled on.
    pub enabled: bool,
    /// Which station(s) to scan.
    pub choice: StationChoice,
    /// User's latitude, for nearest-station ranking.
    pub user_lat: f64,
    /// User's longitude, for nearest-station ranking.
    pub user_lon: f64,
    /// Minute-of-day UTC, for schedule checks (ticks are sample-free).
    pub now_min: u16,
    /// The decoder's current phase.
    pub wefax_state: WefaxState,
    /// Whether the fax subcarrier presence detector currently sees signal.
    pub fax_present: bool,
    /// The user's tune to restore when auto-catch stops.
    pub saved_tune: SavedTune,
}

/// Actions the UI layer interprets.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Retune the receiver to this RF frequency, Hz.
    Tune(u64),
    /// Set the demod mode to WEFAX.
    SetDemodMode,
    /// Reset the WEFAX decoder state.
    ResetDecoder,
    /// Open the live WEFAX viewer window.
    OpenViewer,
    /// Save the completed chart. The interpret layer fills the real path;
    /// the pure machine emits an empty [`PathBuf`] as a placeholder.
    SavePng(PathBuf),
    /// Restore the user's tune from before auto-catch ran.
    RestoreTune(SavedTune),
    /// Show an informational toast.
    Toast(String),
}

/// The catcher's current phase.
#[derive(Clone, Debug)]
pub enum CatcherState {
    /// Auto-catch is off.
    Idle,
    /// Rotating through candidate channels looking for a fax subcarrier.
    Scanning {
        /// The rotation, built once when scanning starts.
        candidates: Vec<Candidate>,
        /// Index of the channel currently tuned.
        idx: usize,
        /// Ticks spent dwelling on the current channel so far.
        dwell: u32,
        /// The user's tune, to restore on disable.
        saved: SavedTune,
    },
    /// Presence detected on a channel; waiting to confirm it's a real chart.
    Locked {
        /// The channel that's locked.
        cand: Candidate,
        /// Ticks spent locked without entering `Imaging`.
        waited: u32,
        /// The user's tune, to restore on disable.
        saved: SavedTune,
    },
    /// The decoder is actively assembling a chart.
    Imaging {
        /// The channel being imaged.
        cand: Candidate,
        /// The user's tune, to restore on disable.
        saved: SavedTune,
    },
}

/// The pure WEFAX auto-catch state machine.
pub struct WefaxCatcher {
    state: CatcherState,
}

impl WefaxCatcher {
    /// Create a new, idle catcher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: CatcherState::Idle,
        }
    }

    /// The catcher's current state.
    #[must_use]
    pub fn state(&self) -> &CatcherState {
        &self.state
    }

    /// Advance the state machine by one tick, returning the actions the
    /// caller should interpret.
    // `ctx` is taken by value per the documented public interface (Task 6
    // constructs a fresh `TickCtx` each tick and hands over ownership);
    // internally it's only ever borrowed, which clippy's pedantic
    // `needless_pass_by_value` otherwise flags.
    #[allow(clippy::needless_pass_by_value)]
    pub fn tick(&mut self, ctx: TickCtx) -> Vec<Action> {
        // Disable from any active state -> restore + Idle.
        if !ctx.enabled {
            return self.go_idle(&ctx);
        }
        match std::mem::replace(&mut self.state, CatcherState::Idle) {
            CatcherState::Idle => self.on_idle(&ctx),
            CatcherState::Scanning {
                candidates,
                idx,
                dwell,
                saved,
            } => self.on_scanning(&ctx, candidates, idx, dwell, saved),
            CatcherState::Locked {
                cand,
                waited,
                saved,
            } => self.on_locked(&ctx, cand, waited, saved),
            CatcherState::Imaging { cand, saved } => self.on_imaging(&ctx, cand, saved),
        }
    }

    // --- per-state helpers (each ≤ ~44 NLOC) ---

    fn go_idle(&mut self, ctx: &TickCtx) -> Vec<Action> {
        let was_active = !matches!(self.state, CatcherState::Idle);
        self.state = CatcherState::Idle;
        if was_active {
            vec![Action::RestoreTune(ctx.saved_tune.clone())]
        } else {
            Vec::new()
        }
    }

    /// Build the scan rotation: all channels of every station matching
    /// `ctx.choice`, schedule-active stations first (nice-to-have
    /// prioritization; inactive stations remain in the rotation as
    /// fallback rather than being dropped).
    fn build_candidates(ctx: &TickCtx) -> Vec<Candidate> {
        let ranked = stations_by_distance(ctx.user_lat, ctx.user_lon);
        let matches_choice = |s: &&'static WefaxStation| match ctx.choice {
            StationChoice::Auto => true,
            StationChoice::Pinned(name) => s.name == name,
        };
        let mut stations: Vec<&'static WefaxStation> = ranked
            .iter()
            .map(|(s, _)| *s)
            .filter(matches_choice)
            .collect();
        stations.sort_by_key(|s| !Self::is_active_now(s, ctx.now_min));
        stations
            .into_iter()
            .flat_map(Self::station_candidates)
            .collect()
    }

    /// Whether `s` is broadcasting at `now_min` (minute-of-day UTC).
    /// `is_active` only inspects hour/minute, so the calendar date used to
    /// build the `DateTime<Utc>` is an arbitrary fixed anchor — this stays
    /// a pure function of `now_min`, with no wall-clock read.
    fn is_active_now(s: &WefaxStation, now_min: u16) -> bool {
        let (hour, min) = (u32::from(now_min / 60), u32::from(now_min % 60));
        chrono::Utc
            .with_ymd_and_hms(2000, 1, 1, hour, min, 0)
            .single()
            .is_some_and(|now| is_active(s, now))
    }

    fn station_candidates(s: &'static WefaxStation) -> Vec<Candidate> {
        s.channels_hz
            .iter()
            .map(|&f| Candidate {
                station: s.name,
                freq_hz: f,
            })
            .collect()
    }

    fn on_idle(&mut self, ctx: &TickCtx) -> Vec<Action> {
        let candidates = Self::build_candidates(ctx);
        if candidates.is_empty() {
            self.state = CatcherState::Idle;
            return vec![Action::Toast("No receivable WEFAX stations".into())];
        }
        let first = candidates[0].clone();
        self.state = CatcherState::Scanning {
            candidates,
            idx: 0,
            dwell: 0,
            saved: ctx.saved_tune.clone(),
        };
        Self::tune_actions(&first)
    }

    fn tune_actions(c: &Candidate) -> Vec<Action> {
        vec![
            Action::ResetDecoder,
            Action::Tune(c.freq_hz),
            Action::SetDemodMode,
        ]
    }

    fn on_scanning(
        &mut self,
        ctx: &TickCtx,
        candidates: Vec<Candidate>,
        idx: usize,
        dwell: u32,
        saved: SavedTune,
    ) -> Vec<Action> {
        if ctx.fax_present {
            let cand = candidates[idx].clone();
            self.state = CatcherState::Locked {
                cand,
                waited: 0,
                saved,
            };
            return Vec::new();
        }
        if dwell + 1 >= SCAN_DWELL_TICKS {
            let next = (idx + 1) % candidates.len();
            let cand = candidates[next].clone();
            self.state = CatcherState::Scanning {
                candidates,
                idx: next,
                dwell: 0,
                saved,
            };
            return Self::tune_actions(&cand);
        }
        self.state = CatcherState::Scanning {
            candidates,
            idx,
            dwell: dwell + 1,
            saved,
        };
        Vec::new()
    }

    fn on_locked(
        &mut self,
        ctx: &TickCtx,
        cand: Candidate,
        waited: u32,
        saved: SavedTune,
    ) -> Vec<Action> {
        if matches!(ctx.wefax_state, WefaxState::Imaging) {
            self.state = CatcherState::Imaging { cand, saved };
            return vec![Action::OpenViewer];
        }
        if !ctx.fax_present && waited + 1 >= LOCK_TIMEOUT_TICKS {
            // False lock — resume scanning from a fresh rotation.
            return self.on_idle(ctx);
        }
        self.state = CatcherState::Locked {
            cand,
            waited: waited + 1,
            saved,
        };
        Vec::new()
    }

    fn on_imaging(&mut self, ctx: &TickCtx, cand: Candidate, saved: SavedTune) -> Vec<Action> {
        if matches!(ctx.wefax_state, WefaxState::Stopped) {
            // Chart complete: save; STAY on this working channel (->
            // Locked, waiting for the next chart on the same frequency).
            self.state = CatcherState::Locked {
                cand,
                waited: 0,
                saved,
            };
            return vec![Action::SavePng(PathBuf::new())];
        }
        if !ctx.fax_present {
            // Signal loss mid-image -> back to scanning.
            return self.on_idle(ctx);
        }
        self.state = CatcherState::Imaging { cand, saved };
        Vec::new()
    }
}

impl Default for WefaxCatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn ctx(enabled: bool, present: bool, wefax: WefaxState) -> TickCtx {
        TickCtx {
            enabled,
            choice: StationChoice::Auto,
            user_lat: 30.0,
            user_lon: -90.0,
            now_min: 0, // ticks are sample-free; monotone counter
            wefax_state: wefax,
            fax_present: present,
            saved_tune: SavedTune::default(),
        }
    }

    #[test]
    fn enable_snapshots_tune_and_starts_scanning() {
        let mut c = WefaxCatcher::new();
        let acts = c.tick(ctx(true, false, WefaxState::Idle));
        assert!(matches!(c.state(), CatcherState::Scanning { .. }));
        // first candidate tuned + mode set + decoder reset
        assert!(acts.iter().any(|a| matches!(a, Action::Tune(_))));
        assert!(acts.iter().any(|a| matches!(a, Action::SetDemodMode)));
        assert!(acts.iter().any(|a| matches!(a, Action::ResetDecoder)));
    }

    #[test]
    fn presence_on_a_channel_locks_it() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle)); // -> Scanning
        // let the dwell elapse with no signal would advance; now signal appears
        let _ = c.tick(ctx(true, true, WefaxState::Idle));
        assert!(matches!(c.state(), CatcherState::Locked { .. }));
    }

    #[test]
    fn locked_transitions_to_imaging_and_opens_viewer() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        c.tick(ctx(true, true, WefaxState::Idle)); // Locked
        let acts = c.tick(ctx(true, true, WefaxState::Imaging));
        assert!(matches!(c.state(), CatcherState::Imaging { .. }));
        assert!(acts.iter().any(|a| matches!(a, Action::OpenViewer)));
    }

    #[test]
    fn imaging_then_complete_saves_and_stays() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        c.tick(ctx(true, true, WefaxState::Idle)); // Locked
        c.tick(ctx(true, true, WefaxState::Imaging)); // Imaging (viewer opens)
        let acts = c.tick(ctx(true, true, WefaxState::Stopped)); // complete
        assert!(acts.iter().any(|a| matches!(a, Action::SavePng(_))));
        // stays on the working channel (not back to a fresh scan rotation)
        assert!(matches!(
            c.state(),
            CatcherState::Locked { .. } | CatcherState::Imaging { .. }
        ));
    }

    #[test]
    fn disable_restores_tune_and_idles() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        let acts = c.tick(ctx(false, false, WefaxState::Idle));
        assert!(acts.iter().any(|a| matches!(a, Action::RestoreTune(_))));
        assert!(matches!(c.state(), CatcherState::Idle));
    }

    #[test]
    fn disable_while_already_idle_is_a_noop() {
        let mut c = WefaxCatcher::new();
        let acts = c.tick(ctx(false, false, WefaxState::Idle));
        assert!(acts.is_empty());
        assert!(matches!(c.state(), CatcherState::Idle));
    }

    #[test]
    fn false_lock_times_out_back_to_scanning() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        c.tick(ctx(true, true, WefaxState::Idle)); // Locked
        // presence drops and stays down past the lock timeout, no imaging
        for _ in 0..=LOCK_TIMEOUT_TICKS {
            c.tick(ctx(true, false, WefaxState::Idle));
        }
        assert!(matches!(c.state(), CatcherState::Scanning { .. }));
    }

    #[test]
    fn locked_without_presence_increments_waited_before_timeout() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        c.tick(ctx(true, true, WefaxState::Idle)); // Locked, waited=0
        c.tick(ctx(true, false, WefaxState::Idle)); // waited=1
        match c.state() {
            CatcherState::Locked { waited, .. } => assert_eq!(*waited, 1),
            other => panic!("expected Locked, got {other:?}"),
        }
    }

    #[test]
    fn presence_loss_mid_imaging_returns_to_scanning() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        c.tick(ctx(true, true, WefaxState::Idle)); // Locked
        c.tick(ctx(true, true, WefaxState::Imaging)); // Imaging
        let acts = c.tick(ctx(true, false, WefaxState::Imaging)); // signal drops mid-image
        assert!(matches!(c.state(), CatcherState::Scanning { .. }));
        assert!(acts.iter().any(|a| matches!(a, Action::Tune(_))));
    }

    #[test]
    fn dwell_advances_to_next_candidate_after_dwell_ticks() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle)); // enable -> Scanning idx0
        let first_freq = match c.state() {
            CatcherState::Scanning {
                candidates, idx, ..
            } => candidates[*idx].freq_hz,
            other => panic!("expected Scanning, got {other:?}"),
        };
        let mut acts = Vec::new();
        for _ in 0..SCAN_DWELL_TICKS {
            acts = c.tick(ctx(true, false, WefaxState::Idle));
        }
        match c.state() {
            CatcherState::Scanning {
                candidates,
                idx,
                dwell,
                ..
            } => {
                assert_eq!(*dwell, 0);
                assert_ne!(candidates[*idx].freq_hz, first_freq);
            }
            other => panic!("expected Scanning, got {other:?}"),
        }
        assert!(acts.iter().any(|a| matches!(a, Action::Tune(_))));
    }

    #[test]
    fn scanning_dwell_increments_without_retune_before_elapsed() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle)); // -> Scanning idx0 dwell0
        let acts = c.tick(ctx(true, false, WefaxState::Idle)); // dwell1, no retune (SCAN_DWELL_TICKS=3)
        assert!(!acts.iter().any(|a| matches!(a, Action::Tune(_))));
        match c.state() {
            CatcherState::Scanning { idx, dwell, .. } => {
                assert_eq!(*idx, 0);
                assert_eq!(*dwell, 1);
            }
            other => panic!("expected Scanning, got {other:?}"),
        }
    }

    #[test]
    fn empty_rotation_no_candidates_toasts_and_stays_idle() {
        let mut c = WefaxCatcher::new();
        let mut tctx = ctx(true, false, WefaxState::Idle);
        tctx.choice = StationChoice::Pinned("Nonexistent Station");
        let acts = c.tick(tctx);
        assert!(matches!(c.state(), CatcherState::Idle));
        assert!(acts.iter().any(|a| matches!(a, Action::Toast(_))));
    }

    #[test]
    fn pinned_choice_filters_candidates_to_one_station() {
        let mut c = WefaxCatcher::new();
        let mut tctx = ctx(true, false, WefaxState::Idle);
        tctx.choice = StationChoice::Pinned("NMG New Orleans");
        c.tick(tctx);
        match c.state() {
            CatcherState::Scanning { candidates, .. } => {
                assert!(!candidates.is_empty());
                assert!(candidates.iter().all(|c| c.station == "NMG New Orleans"));
            }
            other => panic!("expected Scanning, got {other:?}"),
        }
    }

    #[test]
    fn default_impl_matches_new() {
        let c = WefaxCatcher::default();
        assert!(matches!(c.state(), CatcherState::Idle));
    }
}
