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
/// actually wants to keep. SSTV writes a directory of per-image
/// PNGs — ISS ARISS events send ~12 images per pass and each
/// image is distinct content, so a directory matches LRPT's
/// multi-artifact model. Per epic #469 task 7.4 + epic #472.
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
    /// Directory holding one PNG per decoded SSTV image (ISS SSTV).
    /// Wiring layer dispatches via [`Action::SaveSstvPass`].
    /// Named `img0.png`, `img1.png`, etc. in arrival order.
    /// Per epic #472.
    SstvDir(PathBuf),
}

impl PassOutput {
    /// Stable per-protocol discriminant for tests and logs.
    /// Avoids matching on `Debug`-formatted strings.
    #[must_use]
    pub fn protocol(&self) -> sdr_sat::ImagingProtocol {
        match self {
            Self::AptPng(_) => sdr_sat::ImagingProtocol::Apt,
            Self::LrptDir(_) => sdr_sat::ImagingProtocol::Lrpt,
            Self::SstvDir(_) => sdr_sat::ImagingProtocol::Sstv,
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
    /// Pre-AOS de-emphasis `ComboRow` selection (0 = None, 1 = EU
    /// 50 µs, 2 = US 75 µs). Forced to 0 (None) at AOS for the
    /// satellite-image decoders, restored at LOS. The 2400 Hz
    /// APT subcarrier sits in the rolloff of US 75 µs (cutoff
    /// ~2122 Hz), so a user listening to FM broadcast pre-pass
    /// would otherwise have the subcarrier attenuated through
    /// the entire pass. Per silent-fail investigation following
    /// the NOAA 15 pass.
    pub deemphasis_idx: u32,
    /// Pre-AOS audio notch toggle. Forced OFF at AOS, restored
    /// at LOS. A user-set notch anywhere in the 1.5-3 kHz
    /// audio band (typical "remove a hum" use) would null the
    /// APT subcarrier. Per silent-fail investigation following
    /// the NOAA 15 pass.
    pub notch_enabled: bool,
    /// Pre-AOS Doppler-tracker master switch. Forced OFF at
    /// AOS for the duration of an imaging-protocol pass,
    /// restored at LOS. The Doppler shift on a NOAA / Meteor /
    /// ISS pass is ±3.5 kHz worst case at 137 MHz / 145 MHz —
    /// well inside the channel filters of all three imaging
    /// protocols (APT 38 kHz, LRPT 144 kHz, SSTV 12.5 kHz).
    /// During a pass the tracker's 4 Hz tick re-dispatches
    /// `SetVfoOffset(predicted_doppler)` which can disrupt
    /// QPSK Costas lock (LRPT) and the APT line-rate clock.
    /// Disabling for the pass duration loses no functional
    /// value (channel filters absorb the shift) and removes
    /// a known disruption source. Per silent-fail investigation
    /// following the NOAA 15 pass.
    pub doppler_enabled: bool,
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
    /// Save all decoded SSTV images from this pass into `dir`.
    /// Fired on `Recording → Finalizing` for SSTV passes.
    /// Wiring layer creates the directory and writes one PNG
    /// per completed image accumulated in `AppState::sstv_completed_images`:
    /// `img0.png`, `img1.png`, etc. in arrival order. Distinct
    /// from `SaveLrptPass` so the wiring layer's dispatch is
    /// statically typed — per epic #472.
    SaveSstvPass(PathBuf),
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
    /// (decoder tap, viewer open, LOS save). As of epic #472
    /// that's `[Apt, Lrpt, Sstv]`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_supported_protocols(&[
            sdr_sat::ImagingProtocol::Apt,
            sdr_sat::ImagingProtocol::Lrpt,
            sdr_sat::ImagingProtocol::Sstv,
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
    /// reality (`[Apt, Lrpt, Sstv]` as of epic #472). Per CR
    /// round 2 on PR #541: exposing the variadic builder
    /// publicly would let the wiring layer opt into protocols
    /// whose LOS flow isn't actually safe yet. The SSTV path is
    /// mode-agnostic: PD120, PD180, and PD240 (slowrx 0.2.1)
    /// already dispatch as `ImagingProtocol::Sstv`, and any
    /// future slowrx V2 modes (Robot / Scottie / Martin) will
    /// land the same way without a constructor change.
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
        min_elev_deg: f64,
        now_tune: SavedTune,
    ) -> Vec<Action> {
        match self.state.clone() {
            State::Idle => self.tick_idle(
                now,
                passes,
                auto_record_on,
                audio_record_on,
                min_elev_deg,
                now_tune,
            ),
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
        min_elev_deg: f64,
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
        // 4. Peak elevation meets the user-selected quality
        //    threshold (`min_elev_deg` — fed in from the panel's
        //    `AutoRecordQuality` combo per #511; previously a
        //    hardcoded 25° constant).
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
            if pass.max_elevation_deg < min_elev_deg {
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
                sdr_sat::ImagingProtocol::Sstv => PassOutput::SstvDir(sstv_dir_for(pass, now)),
            };
            // Audio recording is suppressed for LRPT regardless
            // of the user toggle: the LRPT demod is a silent
            // passthrough (the imagery is the artifact), and
            // the WAV writer's hardcoded 48 kHz × 2 ch × 2 B
            // means 10+ minutes of stereo silence would burn
            // ~115 MB per pass for no value. (`144 kHz` is the
            // demod's IF rate, not the WAV writer's; an earlier
            // draft conflated the two.) The toggle still
            // applies to APT and SSTV — both produce audible
            // audio (SSTV is audible FSK modulation) that some
            // users may want to record.
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

/// Build the export directory for an ISS SSTV pass:
/// `~/sdr-recordings/sstv-ISS--ZARYA--2026-04-25-143015`.
///
/// The wiring layer creates the directory lazily and writes one
/// PNG per completed SSTV image inside it (`img0.png`,
/// `img1.png`, …). ARISS events typically emit ~12 images per
/// pass — a directory mirrors the LRPT model for multi-artifact
/// passes. Per epic #472.
#[must_use]
fn sstv_dir_for(pass: &Pass, now: DateTime<Utc>) -> PathBuf {
    pass_recording_dir(pass, now, "sstv")
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
        PassOutput::SstvDir(p) => Action::SaveSstvPass(p.clone()),
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
mod tests;
