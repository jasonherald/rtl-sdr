//! Commands emitted by the scanner in response to events.
//! The DSP controller applies these — scanner itself never
//! touches the source, sink, or radio module directly.

use crate::channel::ChannelKey;
use crate::state::ScannerState;

#[derive(Debug, Clone)]
pub enum ScannerCommand {
    /// Retune the source and reconfigure the radio module to this
    /// channel. Controller dispatches `source.set_center_freq`,
    /// `radio_module.set_demod_mode`, `set_bandwidth`,
    /// `set_ctcss_mode`, `set_voice_squelch_mode` in order.
    Retune {
        freq_hz: u64,
        demod_mode: sdr_types::DemodMode,
        bandwidth: f64,
        ctcss: Option<sdr_radio::af_chain::CtcssMode>,
        voice_squelch: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
    },

    /// Gate the final PCM stream to the audio device. DSP chain
    /// keeps running so squelch edges still fire; only user-
    /// audible output is silenced.
    MuteAudio(bool),

    /// UI-facing: active channel changed. `None` during Idle.
    ActiveChannelChanged(Option<ChannelKey>),

    /// UI-facing: scanner phase indicator updated.
    StateChanged(ScannerState),

    /// Emitted when the active rotation is fully empty — every
    /// channel is either removed, disabled, or locked out.
    /// UI surfaces as a toast; scanner transitions to Idle
    /// afterwards.
    EmptyRotation,
}
