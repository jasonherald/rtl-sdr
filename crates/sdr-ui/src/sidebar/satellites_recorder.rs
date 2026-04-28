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
use sdr_radio::af_chain::CtcssMode;
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

/// Where the per-pass imagery should land. Computed at AOS by
/// branching on [`sdr_sat::ImagingProtocol`] and stored on the
/// in-flight state so the LOS-side save uses the same path.
///
/// APT writes a single PNG; LRPT writes one PNG per AVHRR
/// channel (APID) into a directory, since LRPT is multispectral
/// and a single file can't represent all the data the user
/// actually wants to keep. Per epic #469 task 7.4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassOutput {
    /// Single PNG file (NOAA APT). Wiring layer dispatches via
    /// [`Action::SavePng`].
    AptPng(PathBuf),
    /// Directory holding one PNG per APID (Meteor-M LRPT).
    /// Wiring layer dispatches via [`Action::SaveLrptPass`] —
    /// the directory is created lazily by the wiring layer's
    /// per-channel save loop.
    LrptDir(PathBuf),
}

impl PassOutput {
    /// Stable per-protocol discriminant for tests and logs.
    /// Avoids matching on `Debug`-formatted strings.
    #[must_use]
    pub fn protocol(&self) -> sdr_sat::ImagingProtocol {
        match self {
            Self::AptPng(_) => sdr_sat::ImagingProtocol::Apt,
            Self::LrptDir(_) => sdr_sat::ImagingProtocol::Lrpt,
        }
    }
}

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
        /// Per-pass imagery target computed at AOS using the AOS
        /// timestamp. Stored on state so the LOS-side
        /// `Action::SavePng` / `Action::SaveLrptPass` uses the
        /// same timestamp as `audio_path` — without this the
        /// filenames would differ by exactly the pass duration
        /// (CR round 1 on PR #534).
        output: PassOutput,
        /// Audio recording path the wiring layer was asked to
        /// open at AOS. `Some(path)` means we'll fire
        /// [`Action::StopAutoAudioRecord`] at LOS to close it
        /// cleanly. `None` means audio recording is off for this
        /// pass — either the user toggle was off at AOS, or the
        /// pass is LRPT (whose audio path is silent stereo and
        /// wastes ~115 MB per 10-min pass for no benefit — 48 kHz
        /// × 2 ch × 2 B × 600 s, the WAV writer's hardcoded
        /// `AUDIO_SAMPLE_RATE` × `AUDIO_CHANNELS`). The
        /// captured value persists across the AOS toggle so a
        /// user flipping it mid-pass can't leave a half-stopped
        /// writer.
        audio_path: Option<PathBuf>,
    },
    /// Pass is in progress; the protocol-specific decoder is
    /// producing live image data. No per-tick work needed — we
    /// just wait for LOS.
    Recording {
        pass: Pass,
        saved_tune: SavedTune,
        output: PassOutput,
        audio_path: Option<PathBuf>,
    },
    /// Pass ended; image export and tune restore are pending.
    /// This state is single-tick: the next `tick` advances to
    /// `Idle`.
    Finalizing {
        pass: Pass,
        saved_tune: SavedTune,
        output: PassOutput,
        audio_path: Option<PathBuf>,
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
///
/// `scanner_running` snapshots the scanner's master-switch
/// state. The wiring layer's `tune_to_satellite` helper
/// force-disables the scanner as a manual-tune side effect
/// (same path bookmark recall takes) — so without restoring
/// here, an active pre-AOS scan session would be left off
/// after the pass ends. Mirrors `was_running`'s "return the
/// user to whatever they had configured" intent.
///
/// The audio-chain snapshot fields (`squelch_enabled`,
/// `auto_squelch_enabled`, `squelch_db`, `ctcss_mode`,
/// `fm_if_nr_enabled`) capture
/// the user's pre-AOS state for IF/AF settings that must be
/// **force-disabled during a satellite pass** because they're
/// destructive to data-bearing FM modulation:
///
/// - **Squelch / CTCSS** (#555): gate audio when SNR / tone
///   thresholds aren't met. APT image quality depends on
///   getting EVERY scan line; gating low-SNR rows to silence
///   produces black-streaked or fully-black PNGs.
/// - **FM IF NR** (#556): frequency-domain peak-bin filter
///   that zeros all FFT bins except the dominant one. Kills
///   the FM sidebands where the APT 2.4 kHz subcarrier and
///   ISS SSTV tone modulation live.
///
/// Wiring layer flips the corresponding widgets to disabled
/// at AOS (firing the existing change-notify dispatch chain)
/// and back to the saved values at LOS. User-visible — same
/// pattern as `scanner_running`.
#[allow(
    clippy::struct_excessive_bools,
    reason = "snapshot of tune + audio-chain state for the auto-record AOS→LOS \
              round trip — the bool fields each correspond to a distinct user \
              widget that has to be restored after the pass; collapsing them \
              into a sub-struct (e.g. `SavedAudioChain`) would obscure the \
              one-field-per-restored-widget mapping that makes the wiring \
              layer's restore loop trivially auditable"
)]
#[derive(Debug, Clone, Copy)]
pub struct SavedTune {
    pub freq_hz: f64,
    pub vfo_offset_hz: f64,
    pub mode: DemodMode,
    pub bandwidth_hz: u32,
    pub was_running: bool,
    pub scanner_running: bool,
    /// Pre-AOS squelch master-switch state. Forced OFF at AOS,
    /// restored at LOS. Per #555.
    pub squelch_enabled: bool,
    /// Pre-AOS auto-squelch toggle. Auto-squelch tracks the
    /// noise floor and gates audio dynamically — same audio-
    /// gating bug class as manual squelch, just adaptive.
    /// Forced OFF at AOS, restored at LOS. Per CR round 2 on
    /// PR #557.
    pub auto_squelch_enabled: bool,
    /// Pre-AOS squelch threshold (dBFS). Restored verbatim at
    /// LOS — leaving it untouched during the pass would be
    /// fine on its own (squelch is disabled), but persisting
    /// the user's preferred level avoids a silent reset on a
    /// "force-disable + force-defaults" approach.
    pub squelch_db: f32,
    /// Pre-AOS CTCSS mode (Off / Tone(hz)). Forced to Off at
    /// AOS, restored at LOS. Per #555.
    pub ctcss_mode: CtcssMode,
    /// Pre-AOS FM IF NR toggle. Forced OFF at AOS, restored at
    /// LOS. Per #556.
    pub fm_if_nr_enabled: bool,
}

/// Side effects the wiring layer must perform on each transition.
/// Returned from `tick` so the state machine itself stays pure.
#[derive(Debug, Clone)]
pub enum Action {
    /// Tune the radio to the satellite's downlink and open the
    /// protocol-appropriate live viewer. Fired on `Idle →
    /// BeforePass`. The wiring layer's `interpret_action`
    /// matches on `protocol` to dispatch to the right decoder /
    /// viewer (APT today, LRPT in Task 7 of epic #469, SSTV in
    /// epic #472). Per #514.
    StartAutoRecord {
        satellite: String,
        /// Stable NORAD catalog id of the satellite. Threaded through
        /// from the recorder's `KnownSatellite` lookup so the wiring
        /// layer can stash it as the rotation-pass key without a
        /// fragile name → catalog re-lookup. Per CR round 3 on PR
        /// #571 (and the catalog-rename / alias-drift class of bugs
        /// it guards against).
        norad_id: u32,
        freq_hz: u64,
        mode: DemodMode,
        bandwidth_hz: u32,
        protocol: sdr_sat::ImagingProtocol,
    },
    /// Open a WAV writer at `audio_path` to capture the
    /// demodulated audio for the duration of the pass. Fired
    /// alongside [`Action::StartAutoRecord`] only when the user
    /// has the "also save audio" toggle on. Wiring layer maps
    /// to `UiToDsp::StartAudioRecording(path)`. Per #533.
    StartAutoAudioRecord(PathBuf),
    /// Save the in-flight APT image to `png_path`. Fired on
    /// `Recording → Finalizing` for APT passes. Caller is
    /// expected to call `AptImageView::export_png` against the
    /// open viewer.
    SavePng(PathBuf),
    /// Save the in-flight LRPT pass into the given directory.
    /// Fired on `Recording → Finalizing` for LRPT passes.
    /// Caller walks every APID known to `LrptImageView` and
    /// writes one PNG per channel into the directory (creating
    /// it if needed). Per epic #469 task 7.4. Distinct from
    /// `SavePng` so the wiring layer's per-protocol export
    /// strategy is statically separated — no path-meaning
    /// overload.
    SaveLrptPass(PathBuf),
    /// Stop the in-flight WAV writer opened by
    /// [`Action::StartAutoAudioRecord`]. Fired alongside
    /// [`Action::SavePng`] on LOS, but only when audio recording
    /// was actually started at AOS — flipping the toggle mid-
    /// pass does NOT retroactively start or stop recording.
    /// Wiring layer maps to `UiToDsp::StopAudioRecording`.
    StopAutoAudioRecord,
    /// Flush in-flight imaging-decoder state between passes.
    /// Fired alongside [`Action::SavePng`] /
    /// [`Action::SaveLrptPass`] on either LOS transition path —
    /// the normal `Recording → Finalizing` (end-of-pass) AND
    /// the `BeforePass → Finalizing` short-circuit (1 Hz driver
    /// stalled, or pass entirely inside the settle window).
    /// Both paths build the action vec via `los_actions_for`,
    /// which guarantees save → optional stop-audio → reset
    /// ordering. Without it, when the user was already running
    /// pre-AOS (`SavedTune.was_running == true`), the source
    /// stays open across the LOS → AOS boundary and the LRPT
    /// pipeline's `ImageAssembler` + APT decoder accumulator
    /// retain pass N's state when pass N+1 begins. Wiring
    /// layer maps to `UiToDsp::ResetImagingDecoders`. Per
    /// issue #544 + `CodeRabbit` round 1 on PR #560.
    ResetImagingDecoders,
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
///
/// Carries the set of imaging protocols the wiring layer has
/// fully wired (decoder + viewer + LOS save). Catalog entries
/// whose protocol isn't in this set are skipped at AOS — the
/// state machine never transitions to `BeforePass`, so the LOS-
/// side `SavePng` / `RestoreTune` actions never fire either.
///
/// This is the primary defense against "catalog flipped to
/// `Some(Lrpt)` ahead of Task 7 wiring": without this gate, an
/// unsupported protocol would arm the recorder, the wiring
/// layer's `interpret_action` would fail-closed at AOS, but
/// `RestoreTune` at LOS would still clobber any user retunes
/// during the pass and `SavePng` would post a confusing
/// "viewer was closed" toast. Per CR round 2 on PR #541.
pub struct AutoRecorder {
    state: State,
    supported_protocols: Vec<sdr_sat::ImagingProtocol>,
}

impl Default for AutoRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoRecorder {
    /// Build a recorder that arms on every imaging protocol the
    /// wiring layer has fully wired in `interpret_action`
    /// (decoder tap, viewer open, LOS save). As of epic #469
    /// task 7 that's `[Apt, Lrpt]`; ISS SSTV adds `Sstv` once
    /// epic #472 ships.
    #[must_use]
    pub fn new() -> Self {
        Self::with_supported_protocols(&[
            sdr_sat::ImagingProtocol::Apt,
            sdr_sat::ImagingProtocol::Lrpt,
        ])
    }

    /// Build a recorder that arms only on the given imaging
    /// protocols. The slice is the set of protocols the wiring
    /// layer has fully wired in `interpret_action` (decoder
    /// tap, viewer open, LOS save). Catalog entries whose
    /// `imaging_protocol` falls outside this set are silently
    /// skipped — the state machine stays in `Idle` rather than
    /// transitioning to `BeforePass`, so no AOS-side actions
    /// fire and no LOS cleanup fires either.
    ///
    /// **Crate-private** so external callers can only use
    /// [`Self::new`], which encodes today's "fully wired"
    /// reality (`[Apt]`). Per CR round 2 on PR #541: exposing
    /// the variadic builder publicly would let the wiring
    /// layer opt into protocols whose LOS flow isn't actually
    /// safe yet. When Task 7 of epic #469 finishes the LRPT
    /// wiring, flip `new()` to default to `[Apt, Lrpt]` rather
    /// than re-exporting this builder.
    #[must_use]
    fn with_supported_protocols(supported: &[sdr_sat::ImagingProtocol]) -> Self {
        Self {
            state: State::Idle,
            supported_protocols: supported.to_vec(),
        }
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
    /// `audio_record_on` reflects the "also save audio" toggle.
    /// Sampled exclusively at AOS — flipping it mid-pass does NOT
    /// retroactively start or stop a recording (avoids leaving a
    /// half-stopped writer behind, and matches the `auto_record_on`
    /// "in-flight pass keeps running" semantics).
    ///
    /// `now_tune` is the radio's current `(freq_hz, mode,
    /// bandwidth_hz)`. Captured at AOS as the `saved_tune` for the
    /// in-flight pass so a later LOS can restore.
    pub fn tick(
        &mut self,
        now: DateTime<Utc>,
        passes: &[Pass],
        auto_record_on: bool,
        audio_record_on: bool,
        now_tune: SavedTune,
    ) -> Vec<Action> {
        match self.state.clone() {
            State::Idle => self.tick_idle(now, passes, auto_record_on, audio_record_on, now_tune),
            State::BeforePass {
                pass,
                tuned_at,
                saved_tune,
                output,
                audio_path,
            } => self.tick_before_pass(now, pass, tuned_at, saved_tune, output, audio_path),
            State::Recording {
                pass,
                saved_tune,
                output,
                audio_path,
            } => self.tick_recording(now, pass, saved_tune, output, audio_path),
            State::Finalizing {
                pass,
                saved_tune,
                output,
                audio_path,
            } => self.tick_finalizing(pass, saved_tune, output, audio_path),
        }
    }

    fn tick_idle(
        &mut self,
        now: DateTime<Utc>,
        passes: &[Pass],
        auto_record_on: bool,
        audio_record_on: bool,
        now_tune: SavedTune,
    ) -> Vec<Action> {
        if !auto_record_on {
            return Vec::new();
        }
        // Find the soonest eligible upcoming pass. Eligibility:
        // 1. Satellite is in our catalog (lookup yields tune
        //    target).
        // 2. Catalog entry has `imaging_protocol = Some(_)`. None
        //    means the satellite is in the catalog for pass-
        //    prediction display only — auto-record doesn't have
        //    a decoder + viewer wired for it yet (Meteor LRPT
        //    until Task 7 of #469; ISS SSTV until #472). Per
        //    #514 — replaced the old hardcoded `is_apt_capable`
        //    NOAA-name check.
        // 3. Protocol is in `self.supported_protocols`. Per CR
        //    round 2 on PR #541: even if a catalog entry is
        //    flipped to `Some(Lrpt)` ahead of the wiring layer
        //    actually supporting it, the state machine refuses
        //    to arm. Without this gate, the recorder would
        //    transition to `BeforePass`, the wiring layer's
        //    fail-closed AOS branch would no-op, but the
        //    LOS-side `SavePng` + `RestoreTune` actions would
        //    still fire — clobbering any user retunes during
        //    the pass.
        // 4. Peak elevation meets the quality threshold.
        // 5. AOS is within `AOS_LEAD_SECS` (start tuning a few
        //    seconds early so the pipeline is ready at AOS proper).
        for pass in passes {
            let Some((freq_hz, mode, bandwidth_hz, Some(protocol), norad_id)) =
                tune_target_for_pass(pass)
            else {
                continue;
            };
            if !self.supported_protocols.contains(&protocol) {
                continue;
            }
            if pass.max_elevation_deg < AUTO_RECORD_MIN_ELEV_DEG {
                continue;
            }
            // Skip already-finished passes. A stale displayed-pass
            // snapshot (or a panel that hasn't recomputed since
            // the user resumed from suspend) can leave entries in
            // the list whose `end` is already in the past. Without
            // this guard the loop below would emit
            // `StartAutoRecord` for a finished pass — the UI
            // would briefly retune + open the viewer, then save
            // an empty PNG on the next tick. Pass list is sorted
            // by start, so we just `continue` rather than `break`
            // — there could be a future pass behind a stale one.
            if pass.end <= now {
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
            //
            // Imagery and audio paths use the same AOS timestamp
            // so the artifacts pair by string match. CR round 1
            // on PR #534 caught the prior bug: png_path_for was
            // called at LOS while audio_path_for was called at
            // AOS, so a 10-min pass produced filenames 10 min
            // apart.
            //
            // Per epic #469 task 7.4, the imagery target depends
            // on the protocol: APT writes a single PNG; LRPT
            // writes a directory of per-channel PNGs.
            let output = match protocol {
                sdr_sat::ImagingProtocol::Apt => PassOutput::AptPng(png_path_for(pass, now)),
                sdr_sat::ImagingProtocol::Lrpt => PassOutput::LrptDir(lrpt_dir_for(pass, now)),
            };
            // Audio recording is suppressed for LRPT regardless
            // of the user toggle: the LRPT demod is a silent
            // passthrough (the imagery is the artifact), and
            // the WAV writer's hardcoded 48 kHz × 2 ch × 2 B
            // means 10+ minutes of stereo silence would burn
            // ~115 MB per pass for no value. (`144 kHz` is the
            // demod's IF rate, not the WAV writer's; an earlier
            // draft conflated the two.) The toggle still
            // applies to APT — voice/audio capture is genuinely
            // useful there.
            let want_audio = audio_record_on && protocol != sdr_sat::ImagingProtocol::Lrpt;
            let audio_path = want_audio.then(|| audio_path_for(pass, now));
            let mut actions = Vec::with_capacity(3);
            actions.push(Action::StartAutoRecord {
                satellite: pass.satellite.clone(),
                norad_id,
                freq_hz,
                mode,
                bandwidth_hz,
                protocol,
            });
            if let Some(path) = &audio_path {
                actions.push(Action::StartAutoAudioRecord(path.clone()));
            }
            // "Starting" reads wrong if we missed the lead window
            // (laptop wake, recompute lag, etc.) and the pass is
            // already underway — in that case the user sees the
            // toast announcing "starting" while the pass clock is
            // already counting down. `in progress` is the honest
            // phrasing for that case.
            let phase = if pass.start <= now {
                "in progress"
            } else {
                "starting"
            };
            actions.push(Action::Toast {
                message: format!("{} pass {phase} — auto-recording", pass.satellite),
                kind: ToastKind::Info,
            });
            self.state = State::BeforePass {
                pass: pass.clone(),
                tuned_at: now,
                saved_tune: now_tune,
                output,
                audio_path,
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
        output: PassOutput,
        audio_path: Option<PathBuf>,
    ) -> Vec<Action> {
        // LOS already arrived (e.g. the 1 Hz driver stalled on a
        // sleep / suspend cycle, or a very short pass elapsed
        // entirely inside the settle window). Skip straight to
        // finalizing AND emit the protocol-appropriate save —
        // otherwise we'd jump to Idle on the next tick without
        // ever exporting the image. `output` was computed at AOS
        // so the path pairs with `audio_path`. Same action vec
        // as the `tick_recording` LOS path — both go through
        // `los_actions_for` so the BeforePass-stall edge case
        // gets the same `ResetImagingDecoders` flush. Per
        // CodeRabbit round 1 on PR #560.
        if pass.end <= now {
            let actions = los_actions_for(&output, audio_path.is_some());
            self.state = State::Finalizing {
                pass: pass.clone(),
                saved_tune,
                output,
                audio_path,
            };
            return actions;
        }
        if (now - tuned_at).num_seconds() >= SETTLE_SECS {
            self.state = State::Recording {
                pass,
                saved_tune,
                output,
                audio_path,
            };
        }
        Vec::new()
    }

    fn tick_recording(
        &mut self,
        now: DateTime<Utc>,
        pass: Pass,
        saved_tune: SavedTune,
        output: PassOutput,
        audio_path: Option<PathBuf>,
    ) -> Vec<Action> {
        if pass.end <= now {
            // Same action vec as the `tick_before_pass` LOS
            // short-circuit — both paths go through
            // `los_actions_for` so a stalled BeforePass tick
            // gets the same `ResetImagingDecoders` flush. The
            // helper documents the ordering rationale (save →
            // stop audio → reset decoders) and the idempotency
            // contract with the source-stop reset.
            let actions = los_actions_for(&output, audio_path.is_some());
            self.state = State::Finalizing {
                pass,
                saved_tune,
                output,
                audio_path,
            };
            return actions;
        }
        Vec::new()
    }

    fn tick_finalizing(
        &mut self,
        _pass: Pass,
        saved_tune: SavedTune,
        _output: PassOutput,
        _audio_path: Option<PathBuf>,
    ) -> Vec<Action> {
        // Single-tick state: SavePng was issued on entry; restore
        // tune and return to Idle.
        self.state = State::Idle;
        vec![Action::RestoreTune(saved_tune)]
    }
}

// `is_apt_capable` removed in PR closing #514. Replaced by a
// two-layer eligibility gate in `tick_idle`:
//
// 1. The catalog-driven `imaging_protocol.is_some()` check —
//    keeps non-imaging satellites (Meteor / ISS today) out of
//    the auto-record flow while still letting them surface in
//    the upcoming-passes list and respond to the play button.
//    Source of truth: [`sdr_sat::KnownSatellite::imaging_protocol`].
//
// 2. The `self.supported_protocols.contains(&protocol)` check —
//    deny-by-default safety net keyed on what the wiring layer
//    can actually handle in `interpret_action`. Without this,
//    a catalog entry flipped to `Some(Lrpt)` ahead of Task 7
//    wiring would still transition the state machine through
//    `BeforePass → Recording → Finalizing`, so the LOS-side
//    `SavePng` (no-op) and `RestoreTune` (clobbers user state)
//    would fire even though the AOS side fail-closed. Per CR
//    round 2 on PR #541. Source of truth: the
//    `AutoRecorder::new()` constructor in this file.
//
// Adding a new protocol means: (a) add the variant to
// `sdr_sat::ImagingProtocol`, (b) flip the relevant
// `KnownSatellite::imaging_protocol`, (c) add a `match` arm in
// `window.rs::interpret_action`, (d) update `new()` to include
// the new protocol in the default supported set.

/// Build the export path for an APT pass:
/// `~/sdr-recordings/apt-NOAA-19-2026-04-25-143015.png`.
/// Centralised here so the `SavePng` action and the toast message
/// can't drift on naming.
#[must_use]
fn png_path_for(pass: &Pass, now: DateTime<Utc>) -> PathBuf {
    pass_recording_path(pass, now, "apt", "png")
}

/// Build the export directory for an LRPT pass:
/// `~/sdr-recordings/lrpt-METEOR-M2-3-2026-04-25-143015`.
///
/// The wiring layer creates the directory lazily and writes one
/// PNG per APID inside it (e.g. `apid64.png`, `apid65.png`).
/// LRPT is multispectral — a single file can't capture every
/// channel — so the per-pass artifact is a directory rather
/// than a file. Per epic #469 task 7.4.
#[must_use]
fn lrpt_dir_for(pass: &Pass, now: DateTime<Utc>) -> PathBuf {
    pass_recording_dir(pass, now, "lrpt")
}

/// Build the audio-recording path for a satellite + timestamp:
/// `~/sdr-recordings/audio-NOAA-19-2026-04-25-143015.wav`.
/// Pairs with [`png_path_for`] — same sat slug + timestamp so a
/// post-pass viewer can pair PNG with WAV by filename match.
#[must_use]
fn audio_path_for(pass: &Pass, now: DateTime<Utc>) -> PathBuf {
    pass_recording_path(pass, now, "audio", "wav")
}

/// Project a [`PassOutput`] to the matching save action. Lives
/// here (and not as `impl PassOutput`) because [`Action`] is a
/// recorder concept and we want the variant→action mapping
/// localised to the state-machine module — anyone touching the
/// dispatch reads it next to the variant emission.
#[must_use]
fn save_action_for(output: &PassOutput) -> Action {
    match output {
        PassOutput::AptPng(p) => Action::SavePng(p.clone()),
        PassOutput::LrptDir(p) => Action::SaveLrptPass(p.clone()),
    }
}

/// Build the action vec for a `Recording → Finalizing` (or
/// `BeforePass → Finalizing` short-circuit) LOS transition.
/// Both paths emit the same actions in the same order:
///
/// 1. Protocol-appropriate save (`SavePng` for APT,
///    `SaveLrptPass` for LRPT) — must run BEFORE the reset so
///    the export can still snapshot the just-finished pass's
///    pixels.
/// 2. `StopAutoAudioRecord` — only when audio was started at
///    AOS. Toggling the audio switch mid-pass doesn't
///    retroactively start or stop recording.
/// 3. `ResetImagingDecoders` — flush the in-flight APT / LRPT
///    decoder buffers so the next pass starts clean. When
///    `was_running == true` pre-AOS, this is the only hook
///    between passes; when `was_running == false`, the
///    subsequent `RestoreTune` triggers source-stop which
///    resets again — idempotent. Per issue #544.
///
/// Centralising this into one helper guards against the
/// `tick_before_pass` short-circuit (1 Hz driver stalled, or
/// pass entirely inside the settle window) silently dropping
/// the reset action — both call sites stay in lockstep on what
/// "LOS finalisation" means. Per `CodeRabbit` round 1 on PR
/// #560.
#[must_use]
fn los_actions_for(output: &PassOutput, has_audio: bool) -> Vec<Action> {
    let mut actions = Vec::with_capacity(3);
    actions.push(save_action_for(output));
    if has_audio {
        actions.push(Action::StopAutoAudioRecord);
    }
    actions.push(Action::ResetImagingDecoders);
    actions
}

/// Shared filename builder for the per-pass file artifacts —
/// the APT PNG (`png_path_for`) and the WAV (`audio_path_for`)
/// both go through here so a future filename-format tweak only
/// touches one place.
fn pass_recording_path(pass: &Pass, now: DateTime<Utc>, prefix: &str, extension: &str) -> PathBuf {
    glib::home_dir().join("sdr-recordings").join(format!(
        "{prefix}-{sat}-{stamp}.{extension}",
        sat = pass_satellite_slug(pass),
        stamp = pass_timestamp(now),
    ))
}

/// Shared directory builder for per-pass directory artifacts —
/// the LRPT pass directory (`lrpt_dir_for`) goes through here.
/// Same satellite-slug + timestamp logic as the file builder
/// minus the extension. Pulled out as a sibling rather than a
/// special case in `pass_recording_path` so the call sites stay
/// readable and the "no extension" axis isn't smuggled through
/// an empty-string parameter.
fn pass_recording_dir(pass: &Pass, now: DateTime<Utc>, prefix: &str) -> PathBuf {
    glib::home_dir().join("sdr-recordings").join(format!(
        "{prefix}-{sat}-{stamp}",
        sat = pass_satellite_slug(pass),
        stamp = pass_timestamp(now),
    ))
}

/// Filesystem-safe slug for a pass's satellite name: spaces /
/// parens / etc become hyphens, and runs of hyphens collapse so
/// "NOAA 19" → "NOAA-19" (not "NOAA--19"). We control the name
/// source ([`sdr_sat::KNOWN_SATELLITES`]) so a heavy-handed
/// sanitizer is fine.
fn pass_satellite_slug(pass: &Pass) -> String {
    let raw: String = pass
        .satellite
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    raw.split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// AOS timestamp formatted in the user's local timezone, used
/// in every per-pass artifact name so the PNG / directory / WAV
/// triplet pair by string match.
fn pass_timestamp(now: DateTime<Utc>) -> String {
    now.with_timezone(&chrono::Local)
        .format("%Y-%m-%d-%H%M%S")
        .to_string()
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
            scanner_running: false,
            squelch_enabled: false,
            auto_squelch_enabled: false,
            squelch_db: -50.0,
            ctcss_mode: CtcssMode::Off,
            fm_if_nr_enabled: false,
        }
    }

    #[test]
    fn idle_arms_when_pass_in_lead_window_and_eligible() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass starts in 3 s — inside the 5 s lead-in.
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        let actions = r.tick(now, &[pass], true, false, default_tune());
        assert!(matches!(r.state(), State::BeforePass { .. }));
        assert!(matches!(actions[0], Action::StartAutoRecord { .. }));
        // Pre-AOS arming reports "starting" — the pass hasn't
        // crossed `pass.start` yet.
        match &actions[1] {
            Action::Toast { message, .. } => {
                assert!(
                    message.contains("starting"),
                    "expected starting copy, got: {message}",
                );
            }
            other => panic!("expected Toast, got {other:?}"),
        }
    }

    #[test]
    fn idle_arms_for_already_started_pass_uses_in_progress_copy() {
        // Missed the lead window — the laptop woke from suspend
        // mid-pass, or the 1 Hz tick stalled long enough that the
        // displayed-pass entry crosses `pass.start` before
        // `tick_idle` saw it. Recorder still arms (pass.end is
        // future, eligibility holds) but the toast must read "in
        // progress" — saying "starting" when the pass clock is
        // already running would lie to the user.
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass started 30 s ago, ends in 9.5 min — eligible by
        // every other gate (NOAA, 50° peak, end > now).
        let pass = synthetic_noaa19(now, -30, 600, 50.0);
        let actions = r.tick(now, &[pass], true, false, default_tune());
        assert!(matches!(r.state(), State::BeforePass { .. }));
        match &actions[1] {
            Action::Toast { message, .. } => {
                assert!(
                    message.contains("in progress"),
                    "expected in-progress copy, got: {message}",
                );
            }
            other => panic!("expected Toast, got {other:?}"),
        }
    }

    #[test]
    fn idle_does_not_arm_when_toggle_off() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        let actions = r.tick(now, &[pass], false, false, default_tune());
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
    }

    #[test]
    fn idle_does_not_arm_below_quality_threshold() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // 20° peak — "marginal" tier, below the 25° "good" floor.
        let pass = synthetic_noaa19(now, 3, 720, 20.0);
        let actions = r.tick(now, &[pass], true, false, default_tune());
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
    }

    #[test]
    fn idle_does_not_arm_for_unflagged_satellite() {
        // ISS is in the catalog but `imaging_protocol: None`
        // (SSTV decoder + viewer ship in epic #472). The
        // recorder must skip it — tuning would succeed but no
        // decoder would produce imagery, and the LOS-side save
        // would emit an action with no backing data.
        //
        // Pre-task-7 of epic #469 this test used Meteor as the
        // unflagged-protocol fixture; once Meteor flipped to
        // `Some(Lrpt)` we needed a different unflagged catalog
        // entry. ISS is the canonical "in the catalog for
        // pass display, no auto-record yet" case.
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let mut pass = synthetic_noaa19(now, 3, 720, 50.0);
        pass.satellite = "ISS (ZARYA)".to_string();
        let actions = r.tick(now, &[pass], true, false, default_tune());
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
    }

    #[test]
    fn idle_skips_passes_already_past_los() {
        // Suspend / resume: the laptop sleeps mid-session and wakes
        // up after a pass has already ended. The displayed-pass
        // list may still carry the finished entry until the next
        // recompute fires. The recorder must NOT arm for a
        // finished pass — otherwise we'd briefly retune + open
        // the viewer and save an empty PNG on the next tick.
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass ended 30 seconds ago.
        let mut pass = synthetic_noaa19(now, -660, 600, 50.0); // started -660 s ago, ended -60 s ago
        // Sanity: this pass is in the past from `now`'s perspective.
        assert!(pass.end < now);
        let actions = r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
        // Mutating the pass to make peak elevation match the
        // threshold + extending end past now would arm — pinning
        // that the only thing keeping us idle was the LOS check.
        pass.end = now + ChronoDuration::seconds(720);
        pass.start = now + ChronoDuration::seconds(3);
        let actions = r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::BeforePass { .. }));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::StartAutoRecord { .. }))
        );
    }

    #[test]
    fn idle_skips_passes_outside_lead_window() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass starts in 10 min. Way outside the 5 s lead-in.
        let pass = synthetic_noaa19(now, 600, 720, 50.0);
        let actions = r.tick(now, &[pass], true, false, default_tune());
        assert!(matches!(r.state(), State::Idle));
        assert!(actions.is_empty());
    }

    #[test]
    fn before_pass_advances_to_recording_after_settle() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        // Initial arm.
        r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::BeforePass { .. }));
        // Pre-settle tick: still in BeforePass.
        let later = now + ChronoDuration::seconds(2);
        r.tick(
            later,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::BeforePass { .. }));
        // Past settle window: advance to Recording.
        let later = now + ChronoDuration::seconds(SETTLE_SECS);
        r.tick(later, &[pass], true, false, default_tune());
        assert!(matches!(r.state(), State::Recording { .. }));
    }

    #[test]
    fn recording_advances_to_finalizing_at_los() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 600, 50.0); // 10 min pass
        r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Recording { .. }));
        // Tick past LOS.
        let los_plus_one = pass.end + ChronoDuration::seconds(1);
        let actions = r.tick(los_plus_one, &[pass], true, false, default_tune());
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
        // Scoped typed constants for the round-trip pins. Each
        // value is intentionally distinct from the recorder's
        // defaults / the `default_tune` fixture so a regression
        // that resets a field would fail loudly. Declared before
        // any statements per clippy's `items_after_statements`.
        // Per CR round 1 on PR #557.
        const SAVED_FREQ_HZ: f64 = 89_700_000.0;
        const SAVED_VFO_OFFSET_HZ: f64 = 25_000.0;
        // Distinct from `default_tune()`'s 200_000 so a
        // regression that resets bandwidth to the fixture
        // default fails loudly. Per CR round 3 on PR #557.
        const SAVED_BANDWIDTH_HZ: u32 = 180_000;
        const SAVED_SQUELCH_DB: f32 = -42.5;
        const SAVED_CTCSS_TONE_HZ: f32 = 100.0;

        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 60, 50.0);
        let saved = SavedTune {
            freq_hz: SAVED_FREQ_HZ,
            vfo_offset_hz: SAVED_VFO_OFFSET_HZ, // pin a non-zero offset for the round trip
            mode: DemodMode::Wfm,
            bandwidth_hz: SAVED_BANDWIDTH_HZ,
            was_running: false,
            scanner_running: true, // pin: pre-AOS scan must come back at LOS
            // pin: pre-AOS audio-chain settings must come back at LOS
            squelch_enabled: true,
            auto_squelch_enabled: true,
            squelch_db: SAVED_SQUELCH_DB,
            ctcss_mode: CtcssMode::Tone(SAVED_CTCSS_TONE_HZ),
            fm_if_nr_enabled: true,
        };
        // Walk all transitions.
        r.tick(now, std::slice::from_ref(&pass), true, false, saved);
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        let los_plus = pass.end + ChronoDuration::seconds(1);
        r.tick(
            los_plus,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Finalizing { .. }));
        let actions = r.tick(los_plus, &[pass], true, false, default_tune());
        assert!(matches!(r.state(), State::Idle));
        // Restore action carries the original saved tune,
        // including the VFO offset (so a user's drag position
        // survives the auto-record round trip), the pre-AOS
        // playback state (so a stopped radio doesn't silently
        // keep running after LOS), and the pre-AOS scanner
        // state (since `tune_a` force-disables the scanner at
        // AOS, so a previously-running scan would otherwise be
        // dropped permanently).
        match &actions[0] {
            Action::RestoreTune(t) => {
                assert_eq!(t.freq_hz, SAVED_FREQ_HZ);
                assert_eq!(t.vfo_offset_hz, SAVED_VFO_OFFSET_HZ);
                assert_eq!(t.mode, DemodMode::Wfm);
                assert_eq!(t.bandwidth_hz, SAVED_BANDWIDTH_HZ);
                assert!(!t.was_running);
                assert!(t.scanner_running);
                // Audio-chain pre-AOS state survives round-trip
                // (#555 / #556): squelch enable + auto-squelch +
                // level, CTCSS mode, and FM IF NR all come back.
                assert!(t.squelch_enabled);
                assert!(t.auto_squelch_enabled);
                assert!((t.squelch_db - SAVED_SQUELCH_DB).abs() < f32::EPSILON);
                assert!(
                    matches!(t.ctcss_mode, CtcssMode::Tone(hz) if (hz - SAVED_CTCSS_TONE_HZ).abs() < f32::EPSILON)
                );
                assert!(t.fm_if_nr_enabled);
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
        r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::BeforePass { .. }));
        // Jump to a moment past LOS (simulate stalled driver).
        let post_los = pass.end + ChronoDuration::seconds(5);
        let actions = r.tick(
            post_los,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Finalizing { .. }));
        assert!(
            actions.iter().any(|a| matches!(a, Action::SavePng(_))),
            "BeforePass→Finalizing must emit SavePng even when stalled past LOS"
        );
    }

    /// Regression for the BeforePass-stall LOS edge case
    /// (`#544` + `CodeRabbit` round 1 on PR #560). The
    /// `tick_before_pass` short-circuit jumps straight to
    /// `Finalizing` without entering `Recording` — earlier
    /// drafts of the #544 fix only added `ResetImagingDecoders`
    /// to the `tick_recording` LOS path, so a stalled driver
    /// would skip the reset and leak APT/LRPT state into the
    /// next pass on exactly the edge case the issue is trying
    /// to close. Both LOS paths now go through the shared
    /// `los_actions_for` helper; this test pins that the
    /// stalled-BeforePass path still emits the reset.
    #[test]
    fn los_during_before_pass_still_emits_reset_imaging_decoders() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 60, 50.0);
        r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::BeforePass { .. }));
        let post_los = pass.end + ChronoDuration::seconds(5);
        let actions = r.tick(
            post_los,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Finalizing { .. }));
        assert_save_before_reset(&actions, "BeforePass-stall LOS");
    }

    #[test]
    fn overlapping_pass_does_not_re_arm_while_recording() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass_a = synthetic_noaa19(now, 3, 720, 50.0);
        // Arm + settle into Recording.
        r.tick(
            now,
            std::slice::from_ref(&pass_a),
            true,
            false,
            default_tune(),
        );
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass_a),
            true,
            false,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Recording { .. }));
        // A second NOAA pass appears in the list (mid-recording).
        // The recorder should ignore it — Recording stays put.
        let mut pass_b = synthetic_noaa19(now, 30, 720, 60.0);
        pass_b.satellite = "NOAA 18".to_string();
        let actions = r.tick(after_settle, &[pass_a, pass_b], true, false, default_tune());
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

    #[test]
    fn audio_path_pairs_with_png_path_on_same_timestamp() {
        // Helper-function level pairing: with the same timestamp
        // input, PNG and WAV stems differ only in the "apt-" /
        // "audio-" prefix and the extension. This is the
        // contract `pass_recording_path` is supposed to enforce.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 30, 15).unwrap();
        let pass = synthetic_noaa19(now, 0, 720, 50.0);
        let png = png_path_for(&pass, now);
        let audio = audio_path_for(&pass, now);
        let png_stem = png.file_stem().unwrap().to_string_lossy().to_string();
        let audio_stem = audio.file_stem().unwrap().to_string_lossy().to_string();
        let png_tail = png_stem.strip_prefix("apt-").unwrap();
        let audio_tail = audio_stem.strip_prefix("audio-").unwrap();
        assert_eq!(png_tail, audio_tail, "slug+timestamp must match");
        assert_eq!(png.parent(), audio.parent());
        assert!(
            audio
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
        );
    }

    #[test]
    fn audio_and_png_paths_share_aos_timestamp_across_pass_duration() {
        // CR round 1 on PR #534 caught the production bug the
        // previous unit test missed: png_path_for was called at
        // LOS while audio_path_for was called at AOS, so a
        // typical 10-15 minute pass produced filenames that
        // differed by the entire pass duration — breaking the
        // "pair by filename match" contract.
        //
        // Drive the state machine through a real AOS → settle →
        // LOS transition, capturing the audio path the recorder
        // emitted at AOS and the png path it emitted at LOS,
        // then assert their timestamp tails match. Without the
        // pre-compute-png-at-AOS fix this test fails by exactly
        // the pass duration.
        let mut r = AutoRecorder::new();
        let now_aos = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass starts in 3 s (inside the 5 s lead-in window),
        // lasts 720 s — large enough that an LOS-timestamped
        // png_path would be obviously wrong vs the AOS-
        // timestamped audio_path (12-minute delta).
        let pass = synthetic_noaa19(now_aos, 3, 720, 50.0);

        // AOS — capture the audio path the recorder asked for.
        let aos_actions = r.tick(
            now_aos,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        let audio_path = aos_actions
            .iter()
            .find_map(|a| match a {
                Action::StartAutoAudioRecord(p) => Some(p.clone()),
                _ => None,
            })
            .expect("StartAutoAudioRecord at AOS");

        // Settle, then LOS at a wall-clock time clearly after
        // AOS. If png_path were re-computed at LOS, this delta
        // would surface as a different timestamp in the PNG
        // filename.
        let after_settle = now_aos + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        let los_plus = pass.end + ChronoDuration::seconds(1);
        let los_actions = r.tick(
            los_plus,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        let png_path = los_actions
            .iter()
            .find_map(|a| match a {
                Action::SavePng(p) => Some(p.clone()),
                _ => None,
            })
            .expect("SavePng at LOS");

        // The defining assertion: the timestamp tail of the PNG
        // matches the timestamp tail of the WAV. Strip the
        // "apt-NOAA-19-" / "audio-NOAA-19-" prefix and compare
        // verbatim.
        let png_stem = png_path.file_stem().unwrap().to_string_lossy().to_string();
        let audio_stem = audio_path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let png_ts = png_stem.strip_prefix("apt-NOAA-19-").unwrap();
        let audio_ts = audio_stem.strip_prefix("audio-NOAA-19-").unwrap();
        assert_eq!(
            png_ts, audio_ts,
            "PNG and WAV must share the AOS timestamp (regression: \
             pre-fix produced png={png_ts} vs audio={audio_ts}, \
             differing by the pass duration)",
        );
    }

    #[test]
    fn audio_toggle_off_does_not_emit_audio_actions() {
        // Per #533: with the audio toggle off at AOS, the recorder
        // must NOT emit StartAutoAudioRecord at AOS or
        // StopAutoAudioRecord at LOS. PNG path is unaffected.
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        // AOS with audio_record_on = false.
        let aos_actions = r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(
            aos_actions
                .iter()
                .any(|a| matches!(a, Action::StartAutoRecord { .. }))
        );
        assert!(
            !aos_actions
                .iter()
                .any(|a| matches!(a, Action::StartAutoAudioRecord(_))),
            "audio toggle off → no StartAutoAudioRecord",
        );
        // Settle + LOS — audio toggle flipped on mid-pass should
        // NOT retroactively emit StartAutoAudioRecord, and there
        // must be no StopAutoAudioRecord at LOS either (because
        // we never started one).
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        let los_plus = pass.end + ChronoDuration::seconds(1);
        let los_actions = r.tick(los_plus, &[pass], true, true, default_tune());
        assert!(los_actions.iter().any(|a| matches!(a, Action::SavePng(_))));
        assert!(
            !los_actions
                .iter()
                .any(|a| matches!(a, Action::StopAutoAudioRecord)),
            "no audio recording was started → no StopAutoAudioRecord",
        );
    }

    #[test]
    fn audio_toggle_on_emits_paired_start_and_stop() {
        // Per #533: audio_record_on at AOS emits
        // StartAutoAudioRecord(path) alongside StartAutoRecord;
        // LOS emits StopAutoAudioRecord alongside SavePng.
        // The audio path must share the satellite slug with the
        // PNG path the LOS emits.
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        let aos_actions = r.tick(now, std::slice::from_ref(&pass), true, true, default_tune());
        let audio_path = aos_actions.iter().find_map(|a| match a {
            Action::StartAutoAudioRecord(p) => Some(p.clone()),
            _ => None,
        });
        let audio_path = audio_path.expect("audio toggle on must emit StartAutoAudioRecord");
        assert!(
            audio_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("audio-NOAA-19-")
        );
        // Settle then LOS — flipping the audio toggle off mid-
        // pass should NOT cancel the in-flight stop. The stop
        // fires at LOS unconditionally based on the captured
        // audio_path.
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        let los_plus = pass.end + ChronoDuration::seconds(1);
        let los_actions = r.tick(los_plus, &[pass], true, false, default_tune());
        assert!(los_actions.iter().any(|a| matches!(a, Action::SavePng(_))));
        assert!(
            los_actions
                .iter()
                .any(|a| matches!(a, Action::StopAutoAudioRecord)),
            "in-flight audio recording must stop at LOS even after toggle flip",
        );
    }

    /// Build a synthetic METEOR-M 2 pass. Used by the
    /// supported-protocols-gate tests below — the catalog flags
    /// METEOR-M 2 with `imaging_protocol: None` today, but
    /// these tests need to simulate "what happens if a future
    /// edit flips it to `Some(Lrpt)` before the wiring layer
    /// supports it." The recorder gate is what saves us in
    /// that scenario.
    fn synthetic_meteor_m2(
        now: DateTime<Utc>,
        aos_offset_secs: i64,
        duration_secs: i64,
        peak_elev_deg: f64,
    ) -> Pass {
        let start = now + ChronoDuration::seconds(aos_offset_secs);
        Pass {
            satellite: "METEOR-M 2".to_string(),
            start,
            end: start + ChronoDuration::seconds(duration_secs),
            max_elevation_deg: peak_elev_deg,
            max_el_time: start + ChronoDuration::seconds(duration_secs / 2),
            start_az_deg: 245.0,
            end_az_deg: 105.0,
        }
    }

    #[test]
    fn unsupported_protocol_does_not_arm_recorder() {
        // CR round 2 on PR #541: a catalog entry with a protocol
        // outside `supported_protocols` must NOT arm the recorder
        // — `tick_idle` keeps the state at `Idle` rather than
        // transitioning to `BeforePass`. This is the primary
        // defense against "Meteor catalog flipped to Some(Lrpt)
        // ahead of Task 7 wiring": without this gate, the
        // wiring layer's fail-closed AOS branch would no-op,
        // but the LOS-side `SavePng` + `RestoreTune` would
        // still fire and clobber the user's mid-pass state.
        //
        // Simulate the bad-future scenario by using a recorder
        // configured for `Apt` only, then feeding it a synthetic
        // pass for a satellite the catalog flags as a different
        // protocol (METEOR-M 2 with — once Task 7 ships —
        // `Some(Lrpt)`). Even after we monkeypatch the catalog
        // entry to advertise Lrpt, the recorder must refuse to
        // arm.
        //
        // The recorder consults the catalog directly via
        // `tune_target_for_pass`, so monkeypatching the catalog
        // isn't possible from a test. Instead use the inverse
        // construction: build a recorder that supports NOTHING
        // (`with_supported_protocols(&[])`), then feed it an
        // APT-flagged NOAA pass. Same code path — the
        // protocol-gate `continue` fires either way.
        let mut r = AutoRecorder::with_supported_protocols(&[]);
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        let actions = r.tick(now, &[pass], true, false, default_tune());
        // No actions of any kind — no StartAutoRecord, no Toast,
        // no transition.
        assert!(
            actions.is_empty(),
            "unsupported protocol must not arm the recorder; got {actions:?}"
        );
        assert!(
            matches!(r.state(), State::Idle),
            "state must stay Idle, not transition to BeforePass"
        );
    }

    #[test]
    fn unsupported_protocol_blocks_full_pass_lifecycle() {
        // Drives the recorder through the entire would-be pass
        // lifecycle (AOS → settle → LOS) on an unsupported
        // protocol and asserts NO actions ever fire — most
        // importantly, no LOS-side `SavePng` or `RestoreTune`
        // (which would clobber the user's mid-pass state).
        let mut r = AutoRecorder::with_supported_protocols(&[]);
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);

        // AOS: gated out, no actions, state stays Idle.
        let aos = r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(aos.is_empty());
        assert!(matches!(r.state(), State::Idle));

        // Settle window — still no transition.
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        let mid = r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(mid.is_empty());
        assert!(matches!(r.state(), State::Idle));

        // LOS — most important: NO SavePng / RestoreTune fire.
        // Without the supported-protocols gate, `BeforePass →
        // Recording → Finalizing` would have transitioned by
        // now and we'd see those cleanup actions here.
        let los_plus = pass.end + ChronoDuration::seconds(1);
        let los = r.tick(
            los_plus,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        assert!(
            !los.iter().any(|a| matches!(a, Action::SavePng(_))),
            "no SavePng on unsupported-protocol LOS",
        );
        assert!(
            !los.iter().any(|a| matches!(a, Action::RestoreTune(_))),
            "no RestoreTune on unsupported-protocol LOS — the user's mid-pass state must not be clobbered",
        );
        assert!(matches!(r.state(), State::Idle));
    }

    #[test]
    fn supported_protocol_arms_recorder_normally() {
        // Sanity: with `supported_protocols = [Apt]` (the
        // default), an APT-flagged catalog entry arms the
        // recorder as expected. Pins the negative-test contract
        // above by showing the gate is the only thing stopping
        // the unsupported case. Per CR round 2 on PR #541:
        // assert the dispatched payload's `protocol` field
        // explicitly so a future regression that ships
        // `StartAutoRecord` with the wrong protocol (e.g. an
        // off-by-one indexing into the catalog) fails here.
        let mut r = AutoRecorder::with_supported_protocols(&[sdr_sat::ImagingProtocol::Apt]);
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        let actions = r.tick(now, &[pass], true, false, default_tune());
        let dispatched = actions.iter().find_map(|a| match a {
            Action::StartAutoRecord {
                satellite,
                protocol,
                ..
            } => Some((satellite.clone(), *protocol)),
            _ => None,
        });
        let (satellite, protocol) =
            dispatched.expect("supported protocol must emit StartAutoRecord");
        assert_eq!(satellite, "NOAA 19");
        assert_eq!(
            protocol,
            sdr_sat::ImagingProtocol::Apt,
            "dispatched protocol must match the catalog entry's flag",
        );
        assert!(matches!(r.state(), State::BeforePass { .. }));
    }

    #[test]
    fn meteor_synthetic_pass_helper_works() {
        // Sanity for the helper itself — pins the satellite
        // name our gate tests rely on.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_meteor_m2(now, 0, 600, 50.0);
        assert_eq!(pass.satellite, "METEOR-M 2");
    }

    // ─── Per-pass output paths (epic #469 task 7.4) ──────────

    #[test]
    fn lrpt_dir_includes_satellite_slug_and_no_extension() {
        // LRPT's per-pass artifact is a directory, not a file —
        // pin the slug + stamp + lack-of-extension contract so
        // a future filename refactor doesn't accidentally
        // reintroduce a `.png` suffix that would conflict with
        // the per-channel files written inside the directory.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 30, 15).unwrap();
        let pass = synthetic_meteor_m2(now, 0, 720, 50.0);
        let dir = lrpt_dir_for(&pass, now);
        let s = dir.to_string_lossy().to_string();
        assert!(s.contains("lrpt-METEOR-M-2-"), "got {s}");
        assert!(
            dir.extension().is_none(),
            "LRPT pass artifact must be a directory, not a file: {dir:?}"
        );
    }

    #[test]
    fn lrpt_dir_pairs_with_audio_path_on_same_timestamp() {
        // If the user has the audio toggle on, a future scenario
        // (hypothetically — see suppression test below) the WAV
        // and the LRPT directory must share a timestamp so a
        // post-pass viewer can pair them by string match. Same
        // contract `audio_path_pairs_with_png_path_on_same_timestamp`
        // enforces for APT.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 30, 15).unwrap();
        let pass = synthetic_meteor_m2(now, 0, 720, 50.0);
        let dir = lrpt_dir_for(&pass, now);
        let audio = audio_path_for(&pass, now);
        let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
        let audio_stem = audio.file_stem().unwrap().to_string_lossy().to_string();
        let dir_tail = dir_name.strip_prefix("lrpt-").unwrap();
        let audio_tail = audio_stem.strip_prefix("audio-").unwrap();
        assert_eq!(dir_tail, audio_tail, "slug+timestamp must match");
        assert_eq!(dir.parent(), audio.parent());
    }

    #[test]
    fn pass_output_protocol_dispatch_matches_variant() {
        // The `protocol()` discriminant mirrors the variant
        // 1:1 — pin it so a future variant addition without
        // updating `protocol()` fails this test loudly instead
        // of silently dispatching to the wrong save action.
        let apt = PassOutput::AptPng(PathBuf::from("/tmp/apt.png"));
        let lrpt = PassOutput::LrptDir(PathBuf::from("/tmp/lrpt-dir"));
        assert_eq!(apt.protocol(), sdr_sat::ImagingProtocol::Apt);
        assert_eq!(lrpt.protocol(), sdr_sat::ImagingProtocol::Lrpt);
    }

    #[test]
    fn save_action_for_apt_emits_save_png() {
        let action = save_action_for(&PassOutput::AptPng(PathBuf::from("/tmp/apt.png")));
        assert!(matches!(action, Action::SavePng(_)));
    }

    #[test]
    fn save_action_for_lrpt_emits_save_lrpt_pass() {
        let action = save_action_for(&PassOutput::LrptDir(PathBuf::from("/tmp/lrpt-dir")));
        assert!(matches!(action, Action::SaveLrptPass(_)));
    }

    /// LRPT recorder configured with both Apt + Lrpt support so
    /// the Meteor pass actually arms. The default constructor
    /// only supports `[Apt]` today (Task 7.5 flips it to
    /// `[Apt, Lrpt]`); these tests need the wider set to
    /// exercise the LRPT path.
    fn lrpt_recorder() -> AutoRecorder {
        AutoRecorder::with_supported_protocols(&[
            sdr_sat::ImagingProtocol::Apt,
            sdr_sat::ImagingProtocol::Lrpt,
        ])
    }

    #[test]
    fn lrpt_pass_at_aos_emits_save_lrpt_pass_at_los() {
        // End-to-end: Meteor pass through AOS → settle → LOS
        // dispatches `Action::SaveLrptPass` (NOT `Action::SavePng`)
        // because the per-pass artifact is a directory of
        // per-channel PNGs.
        //
        // Lock the wiring contract so a future refactor that
        // accidentally routes the LRPT path through the APT
        // save action fails this test instead of the user's
        // disk (the directory path would land where the APT
        // save handler expects a file).
        let mut r = lrpt_recorder();
        let now_aos = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_meteor_m2(now_aos, 3, 600, 50.0);

        // AOS — arm the recorder.
        let aos_actions = r.tick(
            now_aos,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        // CR-bait check: the LRPT arm must dispatch the
        // protocol on Action::StartAutoRecord so the wiring
        // layer can route to the LRPT viewer/decoder.
        assert!(aos_actions.iter().any(|a| matches!(
            a,
            Action::StartAutoRecord {
                protocol: sdr_sat::ImagingProtocol::Lrpt,
                ..
            }
        )));

        // Advance through settle to Recording, then to LOS.
        let after_settle = now_aos + ChronoDuration::seconds(SETTLE_SECS + 1);
        let _ = r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        assert!(matches!(r.state(), State::Recording { .. }));

        let after_los = pass.end + ChronoDuration::seconds(1);
        let los_actions = r.tick(
            after_los,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        assert!(
            los_actions
                .iter()
                .any(|a| matches!(a, Action::SaveLrptPass(_))),
            "LRPT LOS must emit SaveLrptPass, got {los_actions:?}"
        );
        assert!(
            !los_actions.iter().any(|a| matches!(a, Action::SavePng(_))),
            "LRPT LOS must NOT emit SavePng (that's APT-only), got {los_actions:?}"
        );
    }

    #[test]
    fn lrpt_pass_suppresses_audio_recording_even_when_toggle_on() {
        // LRPT's demod is a silent passthrough; the WAV writer
        // is hardcoded at 48 kHz stereo, so 10+ minutes of
        // silence would burn ~115 MB per pass for no value.
        // The recorder must suppress audio for LRPT regardless
        // of the user's "also save audio" toggle. The toggle
        // still applies to APT — voice/audio capture is
        // genuinely useful there.
        let mut r = lrpt_recorder();
        let now_aos = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_meteor_m2(now_aos, 3, 600, 50.0);

        // Toggle ON, but Meteor pass should still skip the
        // StartAutoAudioRecord emission.
        let aos_actions = r.tick(
            now_aos,
            std::slice::from_ref(&pass),
            true,
            true, // audio_record_on
            default_tune(),
        );
        assert!(
            !aos_actions
                .iter()
                .any(|a| matches!(a, Action::StartAutoAudioRecord(_))),
            "LRPT pass must NOT start audio recording even with toggle on; got {aos_actions:?}"
        );

        // Drive to LOS — must also skip StopAutoAudioRecord
        // (no recording was started, so stopping would be a
        // no-op disguised as a real action).
        let after_settle = now_aos + ChronoDuration::seconds(SETTLE_SECS + 1);
        let _ = r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        let after_los = pass.end + ChronoDuration::seconds(1);
        let los_actions = r.tick(
            after_los,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        assert!(
            !los_actions
                .iter()
                .any(|a| matches!(a, Action::StopAutoAudioRecord)),
            "LRPT LOS must NOT emit StopAutoAudioRecord (no recording was started); got {los_actions:?}"
        );
    }

    #[test]
    fn apt_pass_still_records_audio_with_toggle_on() {
        // Inverse of the LRPT suppression — make sure the LRPT
        // gate didn't accidentally mute APT audio recording.
        let mut r = lrpt_recorder();
        let now_aos = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now_aos, 3, 600, 50.0);
        let aos_actions = r.tick(
            now_aos,
            std::slice::from_ref(&pass),
            true,
            true,
            default_tune(),
        );
        assert!(
            aos_actions
                .iter()
                .any(|a| matches!(a, Action::StartAutoAudioRecord(_))),
            "APT pass with audio toggle on must emit StartAutoAudioRecord; got {aos_actions:?}"
        );
    }

    /// Per #544: LOS must emit `ResetImagingDecoders` so the
    /// in-flight APT/LRPT decoder state from the just-finished
    /// pass doesn't bleed into the next one when the source
    /// stays open across the LOS → AOS boundary. Pinning the
    /// emit here so the wiring layer's between-pass cleanup
    /// hook can't go stealth-quiet on a refactor.
    ///
    /// Helper for the three LOS-reset tests below: assert the
    /// LOS contract that save runs BEFORE reset — the save
    /// action's snapshot read of the shared `LrptImage` would
    /// otherwise capture an empty buffer instead of the
    /// just-finished pass. Pure positional assertion, panics
    /// with a clear message that includes the offending action
    /// vec. Per `CodeRabbit` round 2 on PR #560.
    fn assert_save_before_reset(actions: &[Action], label: &str) {
        let save_idx = actions
            .iter()
            .position(|a| matches!(a, Action::SavePng(_) | Action::SaveLrptPass(_)))
            .unwrap_or_else(|| panic!("{label}: LOS must emit a save action; got {actions:?}"));
        let reset_idx = actions
            .iter()
            .position(|a| matches!(a, Action::ResetImagingDecoders))
            .unwrap_or_else(|| {
                panic!("{label}: LOS must emit Action::ResetImagingDecoders; got {actions:?}")
            });
        assert!(
            save_idx < reset_idx,
            "{label}: save must precede reset; got {actions:?}",
        );
    }

    #[test]
    fn los_emits_reset_imaging_decoders() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_noaa19(now, 3, 720, 50.0);
        r.tick(
            now,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle,
            std::slice::from_ref(&pass),
            true,
            false,
            default_tune(),
        );
        let los_plus = pass.end + ChronoDuration::seconds(1);
        let los_actions = r.tick(los_plus, &[pass], true, false, default_tune());
        assert_save_before_reset(&los_actions, "single-pass LOS");
    }

    /// Per #544: two back-to-back passes must each emit their
    /// own `ResetImagingDecoders` — the recorder's state machine
    /// is reusable across passes (`Idle → BeforePass → Recording
    /// → Finalizing → Idle` is the per-pass cycle). Without a
    /// reset between them, an overnight unattended setup with
    /// 4-6 LRPT passes would accrete the pipeline's
    /// `ImageAssembler` state monotonically.
    #[test]
    fn two_back_to_back_passes_each_emit_reset() {
        let mut r = AutoRecorder::new();
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();

        // Pass 1.
        let pass1 = synthetic_noaa19(now, 3, 720, 50.0);
        r.tick(
            now,
            std::slice::from_ref(&pass1),
            true,
            false,
            default_tune(),
        );
        let after_settle_1 = now + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle_1,
            std::slice::from_ref(&pass1),
            true,
            false,
            default_tune(),
        );
        let los_1 = pass1.end + ChronoDuration::seconds(1);
        let los_1_actions = r.tick(
            los_1,
            std::slice::from_ref(&pass1),
            true,
            false,
            default_tune(),
        );
        let reset_count_1 = los_1_actions
            .iter()
            .filter(|a| matches!(a, Action::ResetImagingDecoders))
            .count();
        assert_eq!(reset_count_1, 1, "pass 1 LOS must emit exactly one reset");
        assert_save_before_reset(&los_1_actions, "pass 1 LOS");

        // Settle from Finalizing back to Idle (the next tick after
        // LOS does Finalizing → Idle and emits RestoreTune).
        let post_los_1 = los_1 + ChronoDuration::seconds(1);
        r.tick(post_los_1, &[pass1], true, false, default_tune());

        // Pass 2 — fresh AOS, schedule it after pass 1 completed.
        let pass2_aos = post_los_1 + ChronoDuration::seconds(60);
        let pass2 = synthetic_noaa19(pass2_aos, 0, 720, 50.0);
        r.tick(
            pass2_aos,
            std::slice::from_ref(&pass2),
            true,
            false,
            default_tune(),
        );
        let after_settle_2 = pass2_aos + ChronoDuration::seconds(SETTLE_SECS + 1);
        r.tick(
            after_settle_2,
            std::slice::from_ref(&pass2),
            true,
            false,
            default_tune(),
        );
        let los_2 = pass2.end + ChronoDuration::seconds(1);
        let los_2_actions = r.tick(los_2, &[pass2], true, false, default_tune());
        let reset_count_2 = los_2_actions
            .iter()
            .filter(|a| matches!(a, Action::ResetImagingDecoders))
            .count();
        assert_eq!(reset_count_2, 1, "pass 2 LOS must emit exactly one reset");
        assert_save_before_reset(&los_2_actions, "pass 2 LOS");
    }
}
