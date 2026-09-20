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
//!   │                   └──(sustained presence loss mid-image)─────────────┘
//!   └──(disable, from any state)
//! ```
//!
//! `Locked` on chart-complete (`WefaxState::Stopped`) stays `Locked` on the
//! same channel rather than restarting a fresh scan rotation — the station
//! is very likely to send another chart on the same frequency shortly.

use chrono::TimeZone;
use sdr_core::messages::WefaxState;
use sdr_sat::{WefaxStation, is_active, stations_by_distance};
use sdr_types::DemodMode;
use std::path::PathBuf;

/// Dwell (in ticks) per candidate channel while scanning.
pub const SCAN_DWELL_TICKS: u32 = 3;
/// Ticks a Locked channel may go without imaging before it's a false lock.
pub const LOCK_TIMEOUT_TICKS: u32 = 10;
/// Hard ceiling on ticks a channel may stay `Locked` without reaching
/// `Imaging`, *regardless* of whether presence still reads true. ~60 s at
/// the ~500 ms tick — comfortably longer than a ~20 s WEFAX phasing
/// sequence, so a genuinely-phasing chart still reaches `Imaging` first,
/// but a present-but-never-imaging lock (a steady birdie, or an in-band
/// carrier with no chart) can't hang the rotation forever.
pub const LOCK_MAX_TICKS: u32 = 120;
/// Ticks of *continuous* presence loss during `Imaging` before it's treated
/// as a real signal loss rather than a brief subcarrier dip (e.g. over a
/// blank/white image region) and falls back to `Scanning`. A single missed
/// tick must NOT abort an in-progress chart — that's the exact output this
/// feature exists to produce.
pub const IMAGING_LOSS_TICKS: u32 = 5;

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
#[derive(Clone, Debug, PartialEq)]
pub struct SavedTune {
    /// The RF center frequency the user was tuned to before auto-catch.
    pub center_hz: f64,
    /// The user's demod mode, stored as the domain enum directly so the
    /// pure state machine stays decoupled from the header dropdown's
    /// index presentation.
    pub demod_mode: DemodMode,
    /// The user's channel bandwidth (Hz), restored after the demod mode
    /// so `SetDemodMode`'s default-bandwidth reset doesn't clobber it.
    pub bandwidth_hz: f64,
    /// Whether the user's own tune was already WEFAX before auto-catch ran.
    pub was_wefax: bool,
}

impl Default for SavedTune {
    fn default() -> Self {
        Self {
            center_hz: 0.0,
            demod_mode: DemodMode::Wfm,
            bandwidth_hz: 0.0,
            was_wefax: false,
        }
    }
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
        /// The scan rotation, carried through so a false lock can resume
        /// at the NEXT candidate rather than restarting at index 0.
        candidates: Vec<Candidate>,
        /// Index (into `candidates`) of the locked channel.
        idx: usize,
        /// The channel that's locked (== `candidates[idx]`).
        cand: Candidate,
        /// Ticks spent locked without entering `Imaging`.
        waited: u32,
        /// The user's tune, to restore on disable.
        saved: SavedTune,
    },
    /// The decoder is actively assembling a chart.
    Imaging {
        /// The scan rotation, carried through so a sustained presence loss
        /// mid-image can resume at the NEXT candidate, not index 0.
        candidates: Vec<Candidate>,
        /// Index (into `candidates`) of the channel being imaged.
        idx: usize,
        /// The channel being imaged (== `candidates[idx]`).
        cand: Candidate,
        /// The user's tune, to restore on disable.
        saved: SavedTune,
        /// Ticks of continuous presence loss seen so far (debounced against
        /// [`IMAGING_LOSS_TICKS`] before falling back to `Scanning`).
        lost: u32,
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

    /// Test-only: drive the catcher directly into `Imaging` so callers
    /// (e.g. `AppState::is_recording`'s table test) can exercise the
    /// "chart in flight" branch without replaying a full tick sequence.
    #[cfg(test)]
    pub(crate) fn force_imaging_for_test(&mut self) {
        let cand = Candidate {
            station: "TEST",
            freq_hz: 4_000_000,
        };
        self.state = CatcherState::Imaging {
            candidates: vec![cand.clone()],
            idx: 0,
            cand,
            saved: SavedTune::default(),
            lost: 0,
        };
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
            return self.go_idle();
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
                candidates,
                idx,
                cand,
                waited,
                saved,
            } => self.on_locked(&ctx, candidates, idx, cand, waited, saved),
            CatcherState::Imaging {
                candidates,
                idx,
                cand,
                saved,
                lost,
            } => self.on_imaging(&ctx, candidates, idx, cand, saved, lost),
        }
    }

    // --- per-state helpers (each ≤ ~44 NLOC) ---

    /// The tune snapshot captured when auto-catch entered its current
    /// active state (if any). Restoring from this — rather than from
    /// whatever `TickCtx::saved_tune` happens to carry on the disabling
    /// tick — keeps the machine self-contained: it always restores the
    /// tune it captured at enable time, not a value the caller must
    /// remember to keep supplying unchanged.
    fn captured_saved(state: &CatcherState) -> Option<SavedTune> {
        match state {
            CatcherState::Idle => None,
            CatcherState::Scanning { saved, .. }
            | CatcherState::Locked { saved, .. }
            | CatcherState::Imaging { saved, .. } => Some(saved.clone()),
        }
    }

    fn go_idle(&mut self) -> Vec<Action> {
        let captured = Self::captured_saved(&self.state);
        self.state = CatcherState::Idle;
        match captured {
            Some(saved) => vec![Action::RestoreTune(saved)],
            None => Vec::new(),
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
        Self::sort_schedule_active_first(&mut stations, ctx.now_min);
        stations
            .into_iter()
            .flat_map(Self::station_candidates)
            .collect()
    }

    /// Stable-sort so schedule-active-at-`now_min` stations come first;
    /// inactive stations stay in the rotation (stable order preserved among
    /// ties) as fallback rather than being dropped.
    fn sort_schedule_active_first(stations: &mut [&'static WefaxStation], now_min: u16) {
        stations.sort_by_key(|s| !Self::is_active_now(s, now_min));
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

    /// Enter `Scanning` fresh, snapshotting `ctx.saved_tune` as the tune to
    /// restore later. Only called when the *previous* state was `Idle`
    /// (i.e. a real user-facing enable) — internal resume-scanning paths
    /// use [`Self::resume_scanning`] instead, to preserve the tune already
    /// captured at the original enable.
    fn on_idle(&mut self, ctx: &TickCtx) -> Vec<Action> {
        self.start_scanning(ctx, ctx.saved_tune.clone())
    }

    /// Resume scanning at the NEXT candidate in the SAME rotation,
    /// preserving a `saved` tune already captured by an earlier state
    /// (false-lock timeout, lock hard-ceiling, or presence loss during
    /// `Imaging`). Advancing past the offending channel — rather than
    /// rebuilding from index 0 via [`Self::start_scanning`] — stops a
    /// persistently-interfering early candidate from starving all later
    /// ones by re-locking itself every rotation.
    fn resume_next_candidate(
        &mut self,
        candidates: Vec<Candidate>,
        idx: usize,
        saved: SavedTune,
    ) -> Vec<Action> {
        let next = (idx + 1) % candidates.len();
        let cand = candidates[next].clone();
        self.state = CatcherState::Scanning {
            candidates,
            idx: next,
            dwell: 0,
            saved,
        };
        Self::tune_actions(&cand)
    }

    fn start_scanning(&mut self, ctx: &TickCtx, saved: SavedTune) -> Vec<Action> {
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
            saved,
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
                candidates,
                idx,
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
        candidates: Vec<Candidate>,
        idx: usize,
        cand: Candidate,
        waited: u32,
        saved: SavedTune,
    ) -> Vec<Action> {
        if matches!(ctx.wefax_state, WefaxState::Imaging) {
            self.state = CatcherState::Imaging {
                candidates,
                idx,
                cand,
                saved,
                lost: 0,
            };
            return vec![Action::OpenViewer];
        }
        if waited + 1 >= LOCK_MAX_TICKS {
            // Hard ceiling: present-but-never-imaging (birdie / carrier
            // with no chart). Advance past it so it can't hang forever.
            return self.resume_next_candidate(candidates, idx, saved);
        }
        if !ctx.fax_present && waited + 1 >= LOCK_TIMEOUT_TICKS {
            // Fast false-lock rescan: presence simply dropped. Advance to
            // the next candidate, preserving the originally captured tune.
            return self.resume_next_candidate(candidates, idx, saved);
        }
        self.state = CatcherState::Locked {
            candidates,
            idx,
            cand,
            waited: waited + 1,
            saved,
        };
        Vec::new()
    }

    fn on_imaging(
        &mut self,
        ctx: &TickCtx,
        candidates: Vec<Candidate>,
        idx: usize,
        cand: Candidate,
        saved: SavedTune,
        lost: u32,
    ) -> Vec<Action> {
        if matches!(ctx.wefax_state, WefaxState::Stopped) {
            // Chart complete: save; STAY on this working channel (->
            // Locked, waiting for the next chart on the same frequency).
            self.state = CatcherState::Locked {
                candidates,
                idx,
                cand,
                waited: 0,
                saved,
            };
            return vec![Action::SavePng(PathBuf::new())];
        }
        if !ctx.fax_present {
            // Debounce: a brief subcarrier dip (e.g. over a blank/white
            // image region) must not abort an in-progress chart. Only a
            // *sustained* loss falls back to scanning — at the NEXT
            // candidate, not a fresh index-0 rotation.
            if lost + 1 >= IMAGING_LOSS_TICKS {
                return self.resume_next_candidate(candidates, idx, saved);
            }
            self.state = CatcherState::Imaging {
                candidates,
                idx,
                cand,
                saved,
                lost: lost + 1,
            };
            return Vec::new();
        }
        self.state = CatcherState::Imaging {
            candidates,
            idx,
            cand,
            saved,
            lost: 0,
        };
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
mod tests;
