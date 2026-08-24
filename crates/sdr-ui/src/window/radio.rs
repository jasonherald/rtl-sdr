//! Radio panel wiring (bandwidth / VFO reset rows).

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{
    AppState, DeemphasisMode, Rc, ScannerForceDisable, SidebarPanels, UiToDsp, sidebar, spectrum,
};

/// Tolerance (Hz) for the "bandwidth is at its mode default"
/// comparison. The bandwidth `SpinRow` uses `digits(0)` so values
/// are already integer-aligned; this tolerance is just a
/// float-comparison guard, not a user-visible fuzziness.
pub(super) const BANDWIDTH_RESET_TOLERANCE_HZ: f64 = 0.5;

/// Update the bandwidth reset button's sensitivity: active only
/// when the spin row's current value differs from the current
/// demod mode's default bandwidth. Called from anywhere either
/// input (current bandwidth OR demod mode) can change. Per
/// issue #341.
pub(super) fn update_bandwidth_reset_sensitivity(
    radio: &sidebar::radio_panel::RadioPanel,
    state: &AppState,
) {
    let mode = state.demod_mode.get();
    // Conservative fallback: if we can't resolve the mode's
    // default (unreachable today — every DemodMode has a valid
    // ctor), keep the reset button inactive rather than claim
    // a comparison we can't actually compute.
    let Ok(default) = sdr_radio::demod::default_bandwidth_for_mode(mode) else {
        tracing::warn!(
            ?mode,
            "default_bandwidth_for_mode failed — disabling bandwidth reset button"
        );
        radio.bandwidth_reset_button.set_sensitive(false);
        return;
    };
    let current = radio.bandwidth_row.value();
    let at_default = (current - default).abs() < BANDWIDTH_RESET_TOLERANCE_HZ;
    radio.bandwidth_reset_button.set_sensitive(!at_default);
}

/// Retune the bandwidth `AdwSpinRow`'s allowed range to the
/// active demod's `[min_bandwidth, max_bandwidth]`. Called
/// whenever the demod mode changes so the row can't accept
/// values the demod will silently reject — that mismatch was
/// the root cause of issue #505 (audio stutter at panel-
/// bandwidth > per-mode max). The panel-level constants
/// `MIN_BANDWIDTH_HZ` / `MAX_BANDWIDTH_HZ` set the absolute
/// envelope the row can ever cover (covers WFM's full
/// 1-250 kHz range); this helper narrows it to the active
/// mode's actual range on every demod change.
///
/// Also clamps the row's current value into the new range —
/// without that, switching from WFM at 200 kHz to NFM would
/// leave the displayed 200 kHz reading stale until the user
/// manually adjusts.
///
/// **Self-suppresses `value-notify` around the auto-clamp
/// `set_value`.** Without that suppression, the clamp would
/// route through the spin-row's `connect_value_notify` handler
/// — which is the MANUAL-bandwidth-change path. That path
/// fires `force_disable.trigger("manual bandwidth change")`,
/// which would stop the scanner mid-retune (the scanner-driven
/// mode-change path calls this helper before its own
/// `set_value`), and dispatch a redundant `SetBandwidth`
/// command. Per `CodeRabbit` round 1 on PR #548. The clamp is
/// programmatic (the UI snapping to a mode change), not user
/// input, so no manual-side effects should fire.
///
/// The DSP doesn't need to be told about the clamp — the
/// caller is responsible for sending its own `SetBandwidth`
/// (or, in the DSP-echo case, the controller already changed
/// the bandwidth as part of the mode change).
pub(super) fn update_bandwidth_row_range_for_mode(
    radio: &sidebar::radio_panel::RadioPanel,
    state: &AppState,
    mode: sdr_types::DemodMode,
) {
    let Ok(min_bw) = sdr_radio::demod::min_bandwidth_for_mode(mode) else {
        tracing::warn!(
            ?mode,
            "min_bandwidth_for_mode failed — leaving bandwidth row range unchanged"
        );
        return;
    };
    let Ok(max_bw) = sdr_radio::demod::max_bandwidth_for_mode(mode) else {
        tracing::warn!(
            ?mode,
            "max_bandwidth_for_mode failed — leaving bandwidth row range unchanged"
        );
        return;
    };
    let adj = radio.bandwidth_row.adjustment();
    adj.set_lower(min_bw);
    adj.set_upper(max_bw);
    let current = radio.bandwidth_row.value();
    // When the existing value is outside the new mode's range,
    // snap to the new mode's *default* rather than its min or max.
    // The min/max represent absolute floors/ceilings; the default
    // is the right "this mode's natural setting" for a value that
    // we already know belongs to a different regime. The bug this
    // fixes: app starts with bandwidth_row at the panel's mode-
    // agnostic 12.5 kHz default, demod dropdown loads as WFM,
    // range update clamps to WFM_MIN (50 kHz) — which crushes
    // FM broadcast deviation peaks and produces fuzzy/static-
    // sounding audio. Same trap fires after a satellite NFM
    // 38 kHz session if the user manually flips back to WFM.
    // Falling back to WFM's 150 kHz default keeps broadcast
    // sounding right. Per silent-fail investigation following
    // the NOAA 15 pass.
    let target = if current < min_bw || current > max_bw {
        // Match on the result so a `default_bandwidth_for_mode`
        // failure is logged and we still emit a corrective value
        // — falling through with `None` would leave the row at
        // its now-out-of-range pre-clamp value, defeating the
        // whole snap. The min/max fallback is the same edge of
        // the range we're crossing, so the user always sees a
        // value inside the new mode's allowed band even when
        // the lookup helper goes sideways. Per CR round 1 on
        // PR #612.
        let safe = match sdr_radio::demod::default_bandwidth_for_mode(mode) {
            Ok(default_bw) => default_bw,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    ?mode,
                    "default_bandwidth_for_mode failed — falling back to range edge"
                );
                if current < min_bw { min_bw } else { max_bw }
            }
        };
        Some(safe)
    } else {
        None
    };
    if let Some(new_value) = target {
        // Suppress only around the actual `set_value` — keep
        // the suppress window as narrow as possible so a
        // genuinely-user-driven `value-notify` racing the GTK
        // main loop can't accidentally get swallowed.
        state.suppress_bandwidth_notify.set(true);
        radio.bandwidth_row.set_value(new_value);
        state.suppress_bandwidth_notify.set(false);
    }
}

/// Tolerance (Hz) for the "VFO offset is at 0" comparison in
/// the floating reset button's visibility logic.
pub(super) const VFO_OFFSET_RESET_TOLERANCE_HZ: f64 = 0.5;

/// Update the floating "Reset VFO" button's visibility — shown
/// only when the VFO is in a non-default state, i.e. bandwidth
/// differs from the mode default OR offset is nonzero. Per
/// issue #341.
pub(super) fn update_vfo_reset_button_visibility(
    radio: &sidebar::radio_panel::RadioPanel,
    spectrum: &spectrum::SpectrumHandle,
    state: &AppState,
) {
    let mode = state.demod_mode.get();
    // Offset-at-zero is resolvable without the demod lookup, so
    // compute it first. If the bandwidth lookup below fails, we
    // can still decide visibility based on offset alone — the
    // click handler's `SetVfoOffset(0.0)` dispatch remains
    // useful even when the bandwidth reset path is broken.
    let offset_at_zero = spectrum.vfo_offset_hz().abs() < VFO_OFFSET_RESET_TOLERANCE_HZ;
    let Ok(default_bw) = sdr_radio::demod::default_bandwidth_for_mode(mode) else {
        tracing::warn!(
            ?mode,
            "default_bandwidth_for_mode failed — floating reset button \
             falls back to offset-only visibility"
        );
        // Button stays available when the user has a nonzero
        // offset to clear; hides when both paths would no-op.
        spectrum.vfo_reset_button.set_visible(!offset_at_zero);
        return;
    };
    let current_bw = radio.bandwidth_row.value();
    let bandwidth_at_default = (current_bw - default_bw).abs() < BANDWIDTH_RESET_TOLERANCE_HZ;
    spectrum
        .vfo_reset_button
        .set_visible(!(bandwidth_at_default && offset_at_zero));
}

/// Connect radio panel controls to DSP commands.
pub(super) fn connect_radio_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    scanner_force_disable: &Rc<ScannerForceDisable>,
) {
    wire_bandwidth_rows(panels, state, scanner_force_disable);
    wire_squelch_rows(panels, state);
    wire_af_rows(panels, state);
}

/// Bandwidth spin row + reset button (echo-suppressed).
/// Split out per the 50-NLOC gate (#817).
fn wire_bandwidth_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    scanner_force_disable: &Rc<ScannerForceDisable>,
) {
    // Bandwidth. The DSP can originate a change too (VFO drag on
    // the spectrum dispatches `UiToDsp::SetBandwidth` directly,
    // and the controller echoes `DspToUi::BandwidthChanged` so the
    // spin row reflects the drag). The echo path updates this row
    // via `set_value` which re-fires `connect_value_notify` —
    // `suppress_bandwidth_notify` breaks the cycle by telling this
    // handler to skip the DSP dispatch when the change originated
    // on the DSP side.
    let state_bw = Rc::clone(state);
    let force_disable_bw = Rc::clone(scanner_force_disable);
    panels.radio.bandwidth_row.connect_value_notify(move |row| {
        if state_bw.suppress_bandwidth_notify.get() {
            return;
        }
        // Not a DSP echo → this is the user turning the spin row.
        // Force-disable scanner so the new bandwidth applies to
        // the user's chosen channel instead of the scanner's next
        // hop.
        force_disable_bw.trigger("manual bandwidth change");
        state_bw.send_dsp(UiToDsp::SetBandwidth(row.value()));
    });

    // Bandwidth reset button → `SetBandwidth(mode_default)`. Per
    // #341. Routes through DSP so the echo updates the spin row
    // — no direct `set_value` manipulation that would skip the
    // DSP / scanner-mutex / force-disable machinery.
    let state_bw_reset = Rc::clone(state);
    let force_disable_bw_reset = Rc::clone(scanner_force_disable);
    panels
        .radio
        .bandwidth_reset_button
        .connect_clicked(move |_| {
            // Reset is a manual change — stop the scanner first
            // so the cleaned-up bandwidth doesn't race the next
            // scanner retune. Same contract as the manual
            // bandwidth-row edit above.
            force_disable_bw_reset.trigger("manual bandwidth reset");
            let mode = state_bw_reset.demod_mode.get();
            match sdr_radio::demod::default_bandwidth_for_mode(mode) {
                Ok(default) => {
                    state_bw_reset.send_dsp(UiToDsp::SetBandwidth(default));
                }
                Err(e) => {
                    tracing::warn!(
                        ?mode,
                        error = %e,
                        "default_bandwidth_for_mode failed on reset click — no dispatch"
                    );
                }
            }
        });
}

/// Squelch enable/level/auto rows.
/// Split out per the 50-NLOC gate (#817).
fn wire_squelch_rows(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Squelch enable
    let state_squelch_en = Rc::clone(state);
    panels
        .radio
        .squelch_enabled_row
        .connect_active_notify(move |row| {
            state_squelch_en.send_dsp(UiToDsp::SetSquelchEnabled(row.is_active()));
        });

    // Squelch level
    let state_squelch_lvl = Rc::clone(state);
    panels
        .radio
        .squelch_level_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation)]
            state_squelch_lvl.send_dsp(UiToDsp::SetSquelch(row.value() as f32));
        });

    // Auto-squelch
    let state_auto_sq = Rc::clone(state);
    panels
        .radio
        .auto_squelch_row
        .connect_active_notify(move |row| {
            state_auto_sq.send_dsp(UiToDsp::SetAutoSquelch(row.is_active()));
        });
}

/// Deemphasis / noise blanker / FM IF NR / WFM stereo rows.
/// Split out per the 50-NLOC gate (#817).
fn wire_af_rows(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Deemphasis
    let state_de = Rc::clone(state);
    panels
        .radio
        .deemphasis_row
        .connect_selected_notify(move |row| {
            use crate::sidebar::radio_panel::{
                DEEMPHASIS_EU50_IDX, DEEMPHASIS_NONE_IDX, DEEMPHASIS_US75_IDX,
            };
            let mode = match row.selected() {
                DEEMPHASIS_NONE_IDX => DeemphasisMode::None,
                DEEMPHASIS_EU50_IDX => DeemphasisMode::Eu50,
                DEEMPHASIS_US75_IDX => DeemphasisMode::Us75,
                other => {
                    // Transient model-churn value (matches the
                    // audio panel's legal-index contract) — fall
                    // back to None but leave a trace.
                    tracing::warn!(other, "deemphasis combo: unexpected index");
                    DeemphasisMode::None
                }
            };
            state_de.send_dsp(UiToDsp::SetDeemphasis(mode));
        });

    // Noise blanker
    let state_noise_blanker = Rc::clone(state);
    panels
        .radio
        .noise_blanker_row
        .connect_active_notify(move |row| {
            state_noise_blanker.send_dsp(UiToDsp::SetNbEnabled(row.is_active()));
        });

    // Noise blanker level
    let state_nb_level = Rc::clone(state);
    panels.radio.nb_level_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation)]
        state_nb_level.send_dsp(UiToDsp::SetNbLevel(row.value() as f32));
    });

    // FM IF NR
    let state_fm_nr = Rc::clone(state);
    panels.radio.fm_if_nr_row.connect_active_notify(move |row| {
        state_fm_nr.send_dsp(UiToDsp::SetFmIfNrEnabled(row.is_active()));
    });

    // WFM Stereo
    let state_stereo = Rc::clone(state);
    panels.radio.stereo_row.connect_active_notify(move |row| {
        state_stereo.send_dsp(UiToDsp::SetWfmStereo(row.is_active()));
    });

    wire_notch_rows(panels, state);

    wire_gate_rows(panels, state);
}

/// Notch filter enable/frequency rows.
/// Split out per the 50-NLOC gate (#817).
fn wire_notch_rows(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Notch filter enable
    let state_notch_en = Rc::clone(state);
    panels
        .radio
        .notch_enabled_row
        .connect_active_notify(move |row| {
            state_notch_en.send_dsp(UiToDsp::SetNotchEnabled(row.is_active()));
        });

    // Notch filter frequency
    let state_notch_freq = Rc::clone(state);
    panels
        .radio
        .notch_freq_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation)]
            state_notch_freq.send_dsp(UiToDsp::SetNotchFrequency(row.value() as f32));
        });
}

/// CTCSS tone/threshold + voice-squelch mode/threshold rows.
/// Split out per the 50-NLOC gate (#817).
fn wire_gate_rows(panels: &SidebarPanels, state: &Rc<AppState>) {
    // CTCSS tone selector
    let state_ctcss = Rc::clone(state);
    let radio_for_ctcss = panels.radio.clone();
    panels.radio.ctcss_row.connect_selected_notify(move |row| {
        let mode = sidebar::radio_panel::RadioPanel::ctcss_mode_from_index(row.selected());
        state_ctcss.send_dsp(UiToDsp::SetCtcssMode(mode));
        // Push the status row label immediately — the detector
        // only emits `CtcssSustainedChanged` on actual gate
        // edges, so without this the label would lag behind a
        // mode change (stay on "Tone detected" after flipping to
        // Off, or stay on "Off" after picking a tone until the
        // first detector window confirms).
        radio_for_ctcss.set_ctcss_sustained(false);
    });

    // CTCSS detection threshold
    let state_ctcss_thresh = Rc::clone(state);
    panels
        .radio
        .ctcss_threshold_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation)]
            state_ctcss_thresh.send_dsp(UiToDsp::SetCtcssThreshold(row.value() as f32));
        });

    // Voice squelch mode
    //
    // On mode change: tell the AF chain to rebuild its detector,
    // reconfigure the threshold spin row (units + range + default
    // value), and push the status row label to the appropriate
    // "waiting" / "Off" text so it doesn't lag behind the first
    // real detector edge.
    //
    // The initial startup layout is Off, so nothing else needs
    // to fire — `apply_voice_squelch_mode_ui(Off)` is called
    // here too to make the starting state consistent.
    panels
        .radio
        .apply_voice_squelch_mode_ui(sdr_dsp::voice_squelch::VoiceSquelchMode::Off);
    let state_vs_mode = Rc::clone(state);
    let radio_for_vs = panels.radio.clone();
    panels
        .radio
        .voice_squelch_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Use the DEFAULT threshold for the target mode, NOT
            // the current spin-row value. The previous mode's
            // threshold is in different units (normalized ratio
            // for Syllabic, dB for Snr), so forwarding it to the
            // new variant would land far outside the new
            // detector's tuning range — e.g. Off → Snr seeding
            // 0.15 dB, or Snr → Syllabic seeding 6.0 as a
            // normalized ratio. Both fail the detector.
            //
            // `apply_voice_squelch_mode_ui` below reconfigures
            // the spin row's adjustment range AND seeds its
            // value from the mode's inline threshold, so the
            // UI and DSP end up aligned on the same default
            // value in the same units.
            let threshold =
                sidebar::radio_panel::RadioPanel::voice_squelch_default_threshold_for_index(idx);
            let mode =
                sidebar::radio_panel::RadioPanel::voice_squelch_mode_from_index(idx, threshold);
            state_vs_mode.send_dsp(UiToDsp::SetVoiceSquelchMode(mode));
            radio_for_vs.apply_voice_squelch_mode_ui(mode);
            radio_for_vs.set_voice_squelch_open(false);
        });

    // Voice squelch threshold
    let state_vs_thresh = Rc::clone(state);
    panels
        .radio
        .voice_squelch_threshold_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation)]
            state_vs_thresh.send_dsp(UiToDsp::SetVoiceSquelchThreshold(row.value() as f32));
        });
}
