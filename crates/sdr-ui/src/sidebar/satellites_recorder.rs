//! Auto-record-on-pass state machine (epic #468 / ticket #482b).
//!
//! Drives the unattended-receive flow for NOAA APT passes:
//!
//! ```text
//! Idle ──(pass arming + auto-record on + quality OK)──▶ BeforePass
//!                                                          │
//!         (settle window elapsed)                          ▼
//!                                                       Recording
//!                                                          │
//!         (pass.end ≤ now)                                 ▼
//!                                                       Finalizing ──▶ Idle
//! ```
//!
//! State transitions are **pure** — `tick` produces a `Vec<Action>`
//! the caller (`window.rs::connect_satellites_panel`) interprets. No
//! widget mutation, no DSP commands, no I/O happen inside this
//! module. That keeps the transition logic unit-testable without a
//! GTK harness, and lets the caller batch / order side-effects
//! however it wants.
//!
//! The wiring layer drives `tick` from the existing 1 Hz countdown
//! timer (the same one that updates pass-row titles) so we don't add
//! a second `GLib` source.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use sdr_sat::Pass;
use sdr_types::DemodMode;

use crate::sidebar::satellites_panel::tune_target_for_pass;

/// Minimum peak elevation (degrees) for an auto-record-eligible
/// pass. Below this, APT decode is mostly horizon-grazing noise and
/// the saved PNG is mostly disk waste. Hardcoded to the
/// "winners + good" tier (≥ 25°) for V1; will become a user-
/// selectable combo in #511.
pub const AUTO_RECORD_MIN_ELEV_DEG: f64 = 25.0;

/// Lead-in before AOS at which the recorder enters `BeforePass` and
/// fires the auto-tune. Gives the channel filter, demod, and
/// decoder a few seconds to settle before the satellite crosses
/// the elevation floor.
const AOS_LEAD_SECS: i64 = 5;

/// Settle time after the auto-tune before the recorder advances to
/// `Recording`. Decoder is producing lines during this window, but
/// they're pre-AOS noise so we don't count them toward the
/// "actually receiving" status.
const SETTLE_SECS: i64 = 3;

/// Recorder lifecycle. Each variant carries the data the next
/// transition needs so the caller doesn't have to thread state
/// through the call site.
#[derive(Debug, Clone)]
pub enum State {
    /// No active recording. Awaiting the next eligible pass.
    Idle,
    /// Auto-tune dispatched; waiting for the channel filter / demod
    /// / decoder to settle before declaring `Recording`. `tuned_at`
    /// is the wall-clock time we issued the tune so the settle
    /// timer can compare against `now`.
    BeforePass {
        pass: Pass,
        tuned_at: DateTime<Utc>,
        /// Snapshot of the user's tune state captured before we
        /// took over. Restored at LOS so the user comes back to
        /// whatever they were listening to.
        saved_tune: SavedTune,
    },
    /// Pass is in progress; APT decoder is producing the live
    /// image. No per-tick work needed — we just wait for LOS.
    Recording { pass: Pass, saved_tune: SavedTune },
    /// Pass ended; PNG export and tune restore are pending. This
    /// state is single-tick: the next `tick` advances to `Idle`.
    Finalizing {
        pass: Pass,
        saved_tune: SavedTune,
        png_path: PathBuf,
    },
}

/// Snapshot of the radio's tune state at the moment the recorder
/// took over. Stored on the in-flight state so a `Finalizing`
/// transition can restore it without the caller having to re-snap.
///
/// Carries `vfo_offset_hz` separately from `freq_hz` so a user-
/// dragged VFO position survives the auto-record round trip:
/// snapshot captures both, restore replays both. Without this,
/// LOS would re-tune to bare centre frequency and the user would
/// lose whatever signal they had pinned with a VFO drag pre-AOS.
///
/// `was_running` snapshots the source's playback state so the
/// LOS restore can return to it. If the user had playback off
/// when auto-record armed (a common "set it and forget it"
/// scenario), we want to leave them off post-LOS — not silently
/// keep the radio chewing CPU after the pass ended.
///
/// `bandwidth_hz` is `u32` to match `Action::StartAutoRecord`
/// and `KnownSatellite::bandwidth_hz` — single integral type
/// for every cross-boundary handoff. The bandwidth row uses
/// `f64` internally but we round at the snapshot boundary so
/// the restore path doesn't have to.
#[derive(Debug, Clone, Copy)]
pub struct SavedTune {
    pub freq_hz: f64,
    pub vfo_offset_hz: f64,
    pub mode: DemodMode,
    pub bandwidth_hz: u32,
    pub was_running: bool,
}

/// Side effects the wiring layer must perform on each transition.
/// Returned from `tick` so the state machine itself stays pure.
#[derive(Debug, Clone)]
pub enum Action {
    /// Tune the radio to the satellite's downlink and open the APT
    /// viewer window. Fired on `Idle → BeforePass`.
    StartAutoRecord {
        satellite: String,
        freq_hz: u64,
        mode: DemodMode,
        bandwidth_hz: u32,
    },
    /// Save the in-flight APT image to `png_path`. Fired on
    /// `Recording → Finalizing`. Caller is expected to call
    /// `AptImageView::export_png` against the open viewer.
    SavePng(PathBuf),
    /// Restore the radio to the pre-recording tune. Fired on
    /// `Finalizing → Idle`. Caller dispatches the same triple
    /// through the same primitive the play button uses.
    RestoreTune(SavedTune),
    /// Surface a status message to the user. The wiring layer
    /// chooses how (toast, status row, log). Two flavours:
    /// `info` and `warn`.
    Toast { message: String, kind: ToastKind },
}

/// Toast severity, matching the existing `AdwToastOverlay` use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Warn,
}

/// The recorder.
pub struct AutoRecorder {
    state: State,
}

impl Default for AutoRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoRecorder {
    #[must_use]
    pub fn new() -> Self {
        Self { state: State::Idle }
    }

    /// Snapshot of the current state — exposed so the wiring layer
    /// can reflect "currently recording {sat}" in the UI.
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Drive the state machine forward. Returns a list of actions
    /// the caller must execute (tune, save, toast, restore).
    ///
    /// `passes` should be the panel's current upcoming-pass list,
    /// already sorted by `start`. `auto_record_on` reflects the
    /// panel's switch state (false → recorder is dormant; an
    /// in-flight pass keeps running to LOS regardless, but no new
    /// passes will arm).
    ///
    /// `now_tune` is the radio's current `(freq_hz, mode,
    /// bandwidth_hz)`. Captured at AOS as the `saved_tune` for the
    /// in-flight pass so a later LOS can restore.
    pub fn tick(
        &mut self,
        now: DateTime<Utc>,
        passes: &[Pass],
        auto_record_on: bool,
        now_tune: SavedTune,
    ) -> Vec<Action> {
        match self.state.clone() {
            State::Idle => self.tick_idle(now, passes, auto_record_on, now_tune),
            State::BeforePass {
                pass,
                tuned_at,
                saved_tune,
            } => self.tick_before_pass(now, pass, tuned_at, saved_tune),
            State::Recording { pass, saved_tune } => self.tick_recording(now, pass, saved_tune),
            State::Finalizing {
                pass,
                saved_tune,
                png_path,
            } => self.tick_finalizing(pass, saved_tune, png_path),
        }
    }

    fn tick_idle(
        &mut self,
        now: DateTime<Utc>,
        passes: &[Pass],
        auto_record_on: bool,
        now_tune: SavedTune,
    ) -> Vec<Action> {
        if !auto_record_on {
            return Vec::new();
        }
        // Find the soonest eligible upcoming pass. Eligibility:
        // 1. Satellite is in our catalog (lookup yields tune target).
        //    LRPT-only satellites are out — the APT decoder won't
        //    decode their signal even if we tune to it.
        // 2. Peak elevation meets the quality threshold.
        // 3. AOS is within `AOS_LEAD_SECS` (start tuning a few
        //    seconds early so the pipeline is ready at AOS proper).
        for pass in passes {
            let Some((freq_hz, mode, bandwidth_hz)) = tune_target_for_pass(pass) else {
                continue;
            };
            if !is_apt_capable(&pass.satellite) {
                continue;
            }
            if pass.max_elevation_deg < AUTO_RECORD_MIN_ELEV_DEG {
                continue;
            }
            let secs_to_aos = (pass.start - now).num_seconds();
            if !(0..=AOS_LEAD_SECS).contains(&secs_to_aos) && pass.start > now {
                // Not yet within the lead-in window. The pass list
                // is sorted by start; once we hit one beyond the
                // window the rest are too. Break out.
                if secs_to_aos > AOS_LEAD_SECS {
                    break;
                }
                continue;
            }
            // Fall through: pass is in the lead-in window OR has
            // already started (we missed the lead — start tuning
            // immediately).
            let mut actions = Vec::with_capacity(2);
            actions.push(Action::StartAutoRecord {
                satellite: pass.satellite.clone(),
                freq_hz,
                mode,
                bandwidth_hz,
            });
            actions.push(Action::Toast {
                message: format!("{} pass starting — auto-recording", pass.satellite),
                kind: ToastKind::Info,
            });
            self.state = State::BeforePass {
                pass: pass.clone(),
                tuned_at: now,
                saved_tune: now_tune,
            };
            return actions;
        }
        Vec::new()
    }

    fn tick_before_pass(
        &mut self,
        now: DateTime<Utc>,
        pass: Pass,
        tuned_at: DateTime<Utc>,
        saved_tune: SavedTune,
    ) -> Vec<Action> {
        // LOS already arrived (e.g. the 1 Hz driver stalled on a
        // sleep / suspend cycle, or a very short pass elapsed
        // entirely inside the settle window). Skip straight to
        // finalizing AND emit the SavePng — otherwise we'd jump
        // to Idle on the next tick without ever exporting the
        // image.
        if pass.end <= now {
            let png_path = png_path_for(&pass, now);
            self.state = State::Finalizing {
                pass: pass.clone(),
                saved_tune,
                png_path: png_path.clone(),
            };
            return vec![Action::SavePng(png_path)];
        }
        if (now - tuned_at).num_seconds() >= SETTLE_SECS {
            self.state = State::Recording { pass, saved_tune };
        }
        Vec::new()
    }

    fn tick_recording(
        &mut self,
        now: DateTime<Utc>,
        pass: Pass,
        saved_tune: SavedTune,
    ) -> Vec<Action> {
        if pass.end <= now {
            // Emit `SavePng` only — the success / failure toast
            // is the wiring layer's responsibility (it knows the
            // export's actual outcome). Announcing "image saved"
            // here would lie if the user closed the viewer
            // mid-pass or `export_png` errored on disk-full /
            // permissions.
            let png_path = png_path_for(&pass, now);
            self.state = State::Finalizing {
                pass,
                saved_tune,
                png_path: png_path.clone(),
            };
            return vec![Action::SavePng(png_path)];
        }
        Vec::new()
    }

    fn tick_finalizing(
        &mut self,
        _pass: Pass,
        saved_tune: SavedTune,
        _png_path: PathBuf,
    ) -> Vec<Action> {
        // Single-tick state: SavePng was issued on entry; restore
        // tune and return to Idle.
        self.state = State::Idle;
        vec![Action::RestoreTune(saved_tune)]
    }
}

/// Is this satellite something we can actually decode in the APT
/// viewer? NOAA POES (15 / 18 / 19) carry the analog APT subcarrier
/// our decoder is built for. METEOR-M / ISS use different
/// modulations (LRPT / SSTV) and would tune correctly but produce
/// no decoded image — pointless to auto-record them today. When
/// LRPT / SSTV decoders ship (epics #469 / #472) this gate will
/// loosen accordingly.
#[must_use]
fn is_apt_capable(satellite_name: &str) -> bool {
    matches!(satellite_name, "NOAA 15" | "NOAA 18" | "NOAA 19")
}

/// Build the export path for a satellite + timestamp:
/// `~/sdr-recordings/apt-NOAA-19-2026-04-25-143015.png`.
/// Centralised here so the `SavePng` action and the toast message
/// can't drift on naming.
#[must_use]
fn png_path_for(pass: &Pass, now: DateTime<Utc>) -> PathBuf {
    let stamp = now
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d-%H%M%S")
        .to_string();
    // Sanitize the satellite name for filesystem safety: spaces /
    // parens / etc become hyphens. We control the name source
    // (KNOWN_SATELLITES) so a heavy-handed sanitizer is fine.
    let sat_slug: String = pass
        .satellite
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Collapse runs of `-` so "NOAA 19" → "NOAA-19", not
    // "NOAA--19", and trim leading/trailing.
    let sat_slug = sat_slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    glib::home_dir()
        .join("sdr-recordings")
        .join(format!("apt-{sat_slug}-{stamp}.png"))
}

// `glib` is referenced via the GTK4 stack but only available on
// Linux per the workspace gating; the panel itself is Linux-only.
use gtk4::glib;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, TimeZone};

    /// Build a synthetic NOAA 19 pass starting `aos_offset_secs`
    /// from `now`, lasting `duration_secs`, with the given peak
    /// elevation. Mirrors the synthetic-pass fixture pattern used
    /// by the `satellites_panel` tests.
    fn synthetic_noaa19(
        now: DateTime<Utc>,
        aos_offset_secs: i64,
        duration_secs: i64,
        peak_elev_deg: f64,
    ) -> Pass {
        let start = now + ChronoDuration::seconds(aos_offset_secs);
        Pass {
            satellite: "NOAA 19".to_string(),
            start,
            end: start + ChronoDuration::seconds(duration_secs),
            max_elevation_deg: peak_elev_deg,
            max_el_time: start + ChronoDuration::seconds(duration_secs / 2),
            start_az_deg: 245.0,
            end_az_deg: 105.0,
        }
    }

    fn default_tune() -> SavedTune {
        SavedTune {
            freq_hz: 100_000_000.0,
            vfo_offset_hz: 0.0,
            mode: DemodMode::Wfm,
            bandwidth_hz: 200_000,
            was_running: true,
        }
    }

    #[test]
    fn idle_arms_when_pass_in_lead_window_and_eligible() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass starts in 3 s — inside the 5 s lead-in.
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        let actions = r.tick(now, &[pass], true, default_tune());
        assert!(matches!(r.state(), State::BeforePass { .. }));
        assert!(matches!(actions[0], Action::StartAutoRecord { .. }));
        assert!(matches!(actions[1], Action::Toast { .. }));
    }

    #[test]
    fn idle_does_not_arm_when_toggle_off() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        let actions = r.tick(now, &[pass], false, default_tune());
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
    }

    #[test]
    fn idle_does_not_arm_below_quality_threshold() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // 20° peak — "marginal" tier, below the 25° "good" floor.
        let pass = synthetic_noaa19(now, 3, 720, 20.0);
        let actions = r.tick(now, &[pass], true, default_tune());
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
    }

    #[test]
    fn idle_does_not_arm_for_non_apt_satellite() {
        // METEOR-M is in the catalog but uses LRPT, not APT —
        // tuning would succeed but the APT decoder would never
        // produce a meaningful image.
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let mut pass = synthetic_noaa19(now, 3, 720, 50.0);
        pass.satellite = "METEOR-M 2".to_string();
        let actions = r.tick(now, &[pass], true, default_tune());
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
    }

    #[test]
    fn idle_skips_passes_outside_lead_window() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass starts in 10 min. Way outside the 5 s lead-in.
        let pass = synthetic_noaa19(now, 600, 720, 50.0);
        let actions = r.tick(now, &[pass], true, default_tune());
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
    }

    #[test]
    fn before_pass_advances_to_recording_after_settle() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        // Initial arm.
        r.tick(now, std::slice::from_ref(&pass), true, default_tune());
        assert!(matches!(r.state(), State::BeforePass { .. }));
        // Pre-settle tick: still in BeforePass.
        let later = now + ChronoDuration::seconds(2);
        r.tick(later, std::slice::from_ref(&pass), true, default_tune());
        assert!(matches!(r.state(), State::BeforePass { .. }));
        // Past settle window: advance to Recording.
        let later = now + ChronoDuration::seconds(SETTLE_SECS);
        r.tick(later, &[pass], true, default_tune());
        assert!(matches!(r.state(), State::Recording { .. }));
    }

    #[test]
    fn recording_advances_to_finalizing_at_los() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 600, 50.0); // 10 min pass
        r.tick(now, std::slice::from_ref(&pass), true, default_tune());
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Recording { .. }));
        // Tick past LOS.
        let los_plus_one = pass.end + ChronoDuration::seconds(1);
        let actions = r.tick(los_plus_one, &[pass], true, default_tune());
        assert!(matches!(r.state(), State::Finalizing { .. }));
        // Only `SavePng` — the success / failure toast is the
        // wiring layer's responsibility now (it knows the export
        // outcome). Asserting absence of any Toast keeps the
        // recorder honest about what it claims.
        assert!(matches!(actions[0], Action::SavePng(_)));
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Toast { .. })),
            "recorder must not announce save success before export — actions: {actions:?}"
        );
    }

    #[test]
    fn finalizing_advances_to_idle_with_restore_action() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 60, 50.0);
        let saved = SavedTune {
            freq_hz: 89_700_000.0,
            vfo_offset_hz: 25_000.0, // pin a non-zero offset for the round trip
            mode: DemodMode::Wfm,
            bandwidth_hz: 200_000,
            was_running: false,
        };
        // Walk all transitions.
        r.tick(now, std::slice::from_ref(&pass), true, saved);
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            default_tune(),
        );
        let los_plus = pass.end + ChronoDuration::seconds(1);
        r.tick(los_plus, std::slice::from_ref(&pass), true, default_tune());
        assert!(matches!(r.state(), State::Finalizing { .. }));
        let actions = r.tick(los_plus, &[pass], true, default_tune());
        assert!(matches!(r.state(), State::Idle));
        // Restore action carries the original saved tune,
        // including the VFO offset (so a user's drag position
        // survives the auto-record round trip) and the
        // pre-AOS playback state (so a stopped radio doesn't
        // silently keep running after LOS).
        match &actions[0] {
            Action::RestoreTune(t) => {
                assert_eq!(t.freq_hz, 89_700_000.0);
                assert_eq!(t.vfo_offset_hz, 25_000.0);
                assert_eq!(t.mode, DemodMode::Wfm);
                assert_eq!(t.bandwidth_hz, 200_000);
                assert!(!t.was_running);
            }
            other => panic!("expected RestoreTune, got {other:?}"),
        }
    }

    #[test]
    fn los_during_before_pass_still_emits_save_png() {
        // Regression: a 1 Hz driver stall (sleep / suspend) can
        // jump the recorder from BeforePass to Finalizing without
        // ever entering Recording. The PNG must still be saved —
        // otherwise the pass completes silently and the user
        // loses whatever decoder lines did arrive during the
        // stall window.
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 60, 50.0); // 1 min pass, 3 s lead-in
        r.tick(now, std::slice::from_ref(&pass), true, default_tune());
        assert!(matches!(r.state(), State::BeforePass { .. }));
        // Jump to a moment past LOS (simulate stalled driver).
        let post_los = pass.end + ChronoDuration::seconds(5);
        let actions = r.tick(post_los, std::slice::from_ref(&pass), true, default_tune());
        assert!(matches!(r.state(), State::Finalizing { .. }));
        assert!(
            actions.iter().any(|a| matches!(a, Action::SavePng(_))),
            "BeforePass→Finalizing must emit SavePng even when stalled past LOS"
        );
    }

    #[test]
    fn overlapping_pass_does_not_re_arm_while_recording() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass_a = synthetic_noaa19(now, 3, 720, 50.0);
        // Arm + settle into Recording.
        r.tick(now, std::slice::from_ref(&pass_a), true, default_tune());
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass_a),
            true,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Recording { .. }));
        // A second NOAA pass appears in the list (mid-recording).
        // The recorder should ignore it — Recording stays put.
        let mut pass_b = synthetic_noaa19(now, 30, 720, 60.0);
        pass_b.satellite = "NOAA 18".to_string();
        let actions = r.tick(after_settle, &[pass_a, pass_b], true, default_tune());
        assert!(matches!(r.state(), State::Recording { .. }));
        // No StartAutoRecord action emitted.
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::StartAutoRecord { .. }))
        );
    }

    #[test]
    fn png_path_includes_satellite_slug_and_timestamp() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 30, 15).unwrap();
        let pass = synthetic_noaa19(now, 0, 720, 50.0);
        let path = png_path_for(&pass, now);
        let s = path.to_string_lossy().to_string();
        assert!(s.contains("apt-NOAA-19-"));
        assert!(
            std::path::Path::new(&s)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
        );
    }
}
