//! Audio panel wiring and volume persistence.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{AppState, Rc, SidebarPanels, UiToDsp, recording_path, sidebar};

/// Epsilon (in fractional volume units, i.e. `[0.0, 1.0]`) below
/// which the mirrored volume widgets are considered "already at the
/// target" and the sync side skips its `set_value` call. Prevents
/// floating-point round-trip artefacts from causing a trivial
/// mirror loop between the header `GtkScaleButton` (0.0..=1.0 step
/// 0.05) and the audio panel `AdwSpinRow` (0..=100 step 1 →
/// 0.01-per-step when scaled). A half-step worth of slack sits
/// comfortably below the smallest user-perceptible change.
pub(super) const VOLUME_SYNC_EPSILON: f64 = 0.005;

/// Wire volume persistence (closes #419) and two-way sync between
/// the header `GtkScaleButton` and the audio panel's
/// `volume_row` `AdwSpinRow`.
///
/// The header button is the single source of truth: its
/// `connect_value_changed` handler is the ONLY path that dispatches
/// `UiToDsp::SetVolume` and writes to the config. The audio-panel
/// row drives the button via `set_value` — its own
/// `connect_value_notify` just mirrors into the button and lets the
/// button's handler do the real work. That keeps one handler owning
/// dispatch + persist, and the mirror path stays idempotent.
///
/// Startup ordering (load-bearing):
///   1. Seed both widgets with the saved volume (no handlers yet,
///      so no dispatch or cascade).
///   2. Explicit `state.send_dsp(UiToDsp::SetVolume(saved))` —
///      guarantees the DSP starts at the restored level regardless
///      of `ScaleButton::set_value` being a no-op on same-value
///      (closes #424's "no 1-frame blast while config loads"
///      requirement).
///   3. Wire the handlers.
///
/// Any other code path that mutates volume (bookmark recall,
/// preferences restore, etc.) must go through
/// `volume_button.set_value(vol)` so this handler runs — direct
/// `send_dsp(SetVolume(..))` would leave the button / row / config
/// showing stale state until the user's next edit.
pub(super) fn connect_volume_persistence(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    volume_button: &gtk4::ScaleButton,
) {
    let saved_volume = config.read(|v| {
        v.get(sidebar::audio_panel::KEY_AUDIO_VOLUME)
            .and_then(serde_json::Value::as_f64)
            .map_or(1.0, |f| f.clamp(0.0, 1.0))
    });

    // Seed both widgets BEFORE wiring handlers. Setting values
    // first means the initial state isn't observed as a
    // "user change" — no handlers fire, no duplicate dispatch,
    // no mirror-path cascade. Dispatch is done explicitly below.
    volume_button.set_value(saved_volume);
    panels
        .audio
        .volume_row
        .set_value(saved_volume * sidebar::audio_panel::VOLUME_PERCENT_MAX);

    // Guaranteed initial dispatch to the DSP so audio starts at
    // the restored level regardless of how `ScaleButton::set_value`
    // interacts with its default (closes #424's "no 1-frame blast
    // while config loads" requirement).
    #[allow(clippy::cast_possible_truncation)]
    state.send_dsp(UiToDsp::SetVolume(saved_volume as f32));

    // Button is the single source of truth: its handler owns
    // dispatch + persist + mirror-to-row.
    let state_vol = Rc::clone(state);
    let config_vol = std::sync::Arc::clone(config);
    let volume_row_weak = panels.audio.volume_row.downgrade();
    volume_button.connect_value_changed(move |_btn, value| {
        // Audio-panel slider mirror runs unconditionally so the
        // header `ScaleButton` stays the single source of truth
        // for the audio panel's percent slider — including
        // during the ACARS programmatic mute/restore where
        // dispatch + persist are suppressed below. Without
        // mirroring here, the panel slider would still show the
        // pre-mute percent while the header sits at 0.0, and a
        // user touching the panel slider could fight ACARS by
        // dispatching the stale value. CR round 1 on PR #590.
        if let Some(row) = volume_row_weak.upgrade() {
            let target_pct = value * sidebar::audio_panel::VOLUME_PERCENT_MAX;
            if (row.value() - target_pct).abs()
                > VOLUME_SYNC_EPSILON * sidebar::audio_panel::VOLUME_PERCENT_MAX
            {
                row.set_value(target_pct);
            }
        }
        // Suppress fires when the ACARS engage path programmatically
        // sets the value to 0 (or restores it on disengage); without
        // this guard the auto-mute would persist 0.0 to config and
        // double-dispatch SetVolume. The engage / disengage arms in
        // `handle_dsp_message` take responsibility for the explicit
        // SetVolume dispatch. Mirrors `suppress_bandwidth_notify` /
        // `suppress_demod_notify`.
        if state_vol.suppress_volume_notify.get() {
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        state_vol.send_dsp(UiToDsp::SetVolume(value as f32));
        config_vol.write(|v| {
            v[sidebar::audio_panel::KEY_AUDIO_VOLUME] = serde_json::json!(value);
        });
    });

    // Audio panel row is mirror-only. Drives the button, which
    // runs the dispatch + config write. The idempotency check
    // breaks the `btn.set_value → row.set_value → btn.set_value`
    // loop when the two widgets are already in sync.
    let volume_button_weak = volume_button.downgrade();
    panels.audio.volume_row.connect_value_notify(move |row| {
        let value = (row.value() / sidebar::audio_panel::VOLUME_PERCENT_MAX).clamp(0.0, 1.0);
        if let Some(btn) = volume_button_weak.upgrade()
            && (btn.value() - value).abs() > VOLUME_SYNC_EPSILON
        {
            btn.set_value(value);
        }
    });
}

/// Connect audio panel controls to DSP commands.
pub(super) fn connect_audio_panel(panels: &SidebarPanels, state: &Rc<AppState>) {
    wire_sink_selector(panels, state);
    wire_network_sink_config(panels, state);
    wire_recording_toggles(panels, state);
}

/// Audio device + sink-type selectors (issue #247).
/// Split out per the 50-NLOC gate (#817).
fn wire_sink_selector(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Audio device selector — routes PipeWire output to the selected sink
    let state_dev = Rc::clone(state);
    let node_names = panels.audio.device_node_names.clone();
    panels.audio.device_row.connect_selected_notify(move |row| {
        let idx = row.selected() as usize;
        if let Some(node_name) = node_names.get(idx) {
            state_dev.send_dsp(UiToDsp::SetAudioDevice(node_name.clone()));
        }
    });

    // Sink type selector — toggles the engine between local
    // audio device and network stream, and shows/hides the
    // network config rows so the sidebar layout reflects the
    // active mode. Per issue #247.
    let state_sink_type = Rc::clone(state);
    let network_group = panels.audio.network_sink_group.clone();
    panels
        .audio
        .sink_type_row
        .connect_selected_notify(move |row| {
            // Match explicitly against both legal indices and
            // early-return on anything else. The previous shape
            // mapped any non-Network value to Local, which would
            // silently dispatch a sink swap on a transient or
            // future-added combo entry that this handler doesn't
            // know about. Per `CodeRabbit` round 2 on PR #351.
            let new_type = match row.selected() {
                sidebar::audio_panel::SINK_TYPE_LOCAL_IDX => sdr_core::AudioSinkType::Local,
                sidebar::audio_panel::SINK_TYPE_NETWORK_IDX => sdr_core::AudioSinkType::Network,
                unknown => {
                    tracing::warn!(
                        selected_idx = unknown,
                        "audio sink-type combo emitted unknown index; ignoring"
                    );
                    return;
                }
            };
            let network_visible = matches!(new_type, sdr_core::AudioSinkType::Network);
            // Toggle the whole Network-sink section instead of its
            // four rows individually — same pattern as the Radio
            // panel's De-emphasis / CTCSS group-level hides.
            network_group.set_visible(network_visible);
            state_sink_type.send_dsp(UiToDsp::SetAudioSinkType(new_type));
        });
}

/// Network host/port/protocol triple -> `SetNetworkSinkConfig`.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_sink_config(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Helper closure-builder: any change to the network host /
    // port / protocol triple re-sends the full SetNetworkSinkConfig
    // so the controller can rebuild the sink atomically. The
    // engine handler is idempotent — sending the same values
    // again is harmless. Per issue #247.
    let push_network_config = {
        let state = Rc::clone(state);
        let host_row = panels.audio.network_host_row.clone();
        let port_row = panels.audio.network_port_row.clone();
        let proto_row = panels.audio.network_protocol_row.clone();
        move || {
            let hostname = host_row.text().to_string();
            // SpinRow's adjustment is bounded (1..=65535), and
            // we explicitly clamp again here as belt-and-
            // suspenders against any future code path that
            // hands us a different adjustment. After the clamp
            // the value is finite and in [0, 65535] so the
            // narrowing cast is exact — the clippy lints below
            // are safe to silence with that justification.
            let port_clamped = port_row
                .value()
                .round()
                .clamp(f64::from(u16::MIN), f64::from(u16::MAX));
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to [0, u16::MAX] above"
            )]
            let port = port_clamped as u16;
            let protocol = sidebar::audio_panel::protocol_from_combo_idx(proto_row.selected());
            state.send_dsp(UiToDsp::SetNetworkSinkConfig {
                hostname,
                port,
                protocol,
            });
        }
    };

    // Hostname commits on Enter / focus-out (the AdwEntryRow's
    // `connect_apply` signal). connect_changed would fire per
    // keystroke and reconnect-on-every-character is bad UX.
    {
        let push = push_network_config.clone();
        panels.audio.network_host_row.connect_apply(move |_| push());
    }
    {
        let push = push_network_config.clone();
        panels
            .audio
            .network_port_row
            .connect_value_notify(move |_| push());
    }
    {
        let push = push_network_config.clone();
        panels
            .audio
            .network_protocol_row
            .connect_selected_notify(move |_| push());
    }
}

/// Audio recording switch row (the IQ recording toggle is wired in
/// `window/source.rs` alongside the source panel).
fn wire_recording_toggles(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Audio recording toggle
    let state_rec = Rc::clone(state);
    panels
        .audio
        .record_audio_row
        .connect_active_notify(move |row| {
            if row.is_active() {
                let path = recording_path("audio");
                tracing::info!(?path, "starting audio recording");
                state_rec.send_dsp(UiToDsp::StartAudioRecording(path));
            } else {
                tracing::info!("stopping audio recording");
                state_rec.send_dsp(UiToDsp::StopAudioRecording);
            }
        });
}
