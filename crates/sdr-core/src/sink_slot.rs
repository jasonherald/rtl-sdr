//! Audio output sink multiplexer used by the DSP controller.
//!
//! The controller used to hold a single `sdr_sink_audio::AudioSink`
//! directly. Issue #247 surfaced the problem: the GTK Audio panel
//! exposed a "Sink type" combo (Audio / Network) but the engine
//! never honored the selection — `sdr_sink_network::NetworkSink`
//! existed in the workspace but had no path into the running
//! pipeline.
//!
//! This module wraps both implementations in a single enum so the
//! controller's audio path stays uniform regardless of the active
//! sink type. Switching is a state transition: drop the old sink,
//! construct the new one, restart it if the engine is already
//! running. The engine's audio block size, format, and rate are
//! identical for both variants (48 kHz stereo f32) so callers
//! never have to reformat per type.
//!
//! The two underlying sinks ship slightly different write APIs
//! (`AudioSink::write_samples` vs `NetworkSink::write_stereo_samples`),
//! so a `Box<dyn Sink>` (the `sdr_pipeline::sink_manager::Sink`
//! trait) wouldn't compose cleanly — the trait is lifecycle-only,
//! deliberately leaving the data path per-impl. This enum is the
//! adapter layer that gives the controller one `write_samples`
//! method that does the right thing.

use sdr_pipeline::sink_manager::Sink;
use sdr_sink_audio::AudioSink;
use sdr_sink_network::NetworkSink;
use sdr_types::{Protocol, SinkError, Stereo};

/// User-facing audio sink type. Mirrored on the UI side as a
/// two-option picker (local audio device vs network stream).
/// Stored on `DspState` so a sink restart (e.g. mode change,
/// engine restart) recreates the right variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioSinkType {
    /// `PipeWire` (Linux) or `CoreAudio` (macOS) — the local
    /// audio device.
    #[default]
    Local,
    /// TCP server / UDP unicast — see
    /// `sdr_sink_network::NetworkSink`.
    Network,
}

/// Status events the network sink emits to the UI. Currently
/// surfaces only switch boundaries and write failures; per-frame
/// TCP-client connect/disconnect tracking would require deeper
/// instrumentation in `NetworkSink` itself and is left for a
/// follow-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkSinkStatus {
    /// The network sink is now the active audio output.
    /// `endpoint` is the host:port the engine is bound to (TCP
    /// server) or sending to (UDP), useful for a status line in
    /// the UI.
    Active {
        endpoint: String,
        protocol: Protocol,
    },
    /// The network sink was switched off, replaced by another
    /// sink, or the engine stopped.
    Inactive,
    /// A startup or write failure took the network sink offline.
    /// `message` is a human-readable description suitable for a
    /// toast or status row.
    Error { message: String },
}

/// Active audio sink — holds whichever underlying sink the user
/// last selected. The two variants share lifecycle hooks
/// (`start` / `stop`) but have differently-named write methods,
/// so this wrapper exposes a single `write_samples` API for the
/// controller's hot path.
pub enum AudioSinkSlot {
    Local(AudioSink),
    Network(NetworkSink),
}

impl AudioSinkSlot {
    /// Construct the default local sink. Matches the engine's
    /// pre-#247 behavior so existing callers that build
    /// `DspState` without an explicit sink-type choice get the
    /// same audio path they always had.
    pub fn local_default() -> Self {
        Self::Local(AudioSink::new())
    }

    /// Construct a network sink with the given config. The sink
    /// is **not** started here — caller must call `start()`
    /// when the engine is ready (typically immediately, or after
    /// a sink-type switch when the engine is already running).
    pub fn network(hostname: &str, port: u16, protocol: Protocol) -> Self {
        Self::Network(NetworkSink::new(hostname, port, protocol))
    }

    /// Which variant is currently active.
    pub fn kind(&self) -> AudioSinkType {
        match self {
            Self::Local(_) => AudioSinkType::Local,
            Self::Network(_) => AudioSinkType::Network,
        }
    }

    /// Start the underlying sink (open audio device / bind
    /// socket). Errors if the variant's specific resource
    /// (audio device, port) is unavailable.
    pub fn start(&mut self) -> Result<(), SinkError> {
        match self {
            Self::Local(s) => s.start(),
            // `Sink::start` for `NetworkSink` opens the listener
            // (TCP server) or resolves + binds the UDP socket.
            // Use UFCS so we don't need to import the trait at
            // every call site.
            Self::Network(s) => Sink::start(s),
        }
    }

    /// Stop the underlying sink. Idempotent — safe to call
    /// when already stopped.
    pub fn stop(&mut self) -> Result<(), SinkError> {
        match self {
            Self::Local(s) => s.stop(),
            Self::Network(s) => Sink::stop(s),
        }
    }

    /// Write a block of stereo audio. Routes to the variant's
    /// preferred write method (the trait's lifecycle-only
    /// surface doesn't include data, so each variant defines
    /// its own).
    pub fn write_samples(&mut self, samples: &[Stereo]) -> Result<(), SinkError> {
        match self {
            Self::Local(s) => s.write_samples(samples),
            Self::Network(s) => s.write_stereo_samples(samples),
        }
    }

    /// Pick a specific local audio device by node name. No-op
    /// (and returns `Ok`) for the network variant, since
    /// "device selection" doesn't apply — the network sink's
    /// destination is configured by the host/port/protocol
    /// triple, not by a device UID.
    pub fn set_target(&mut self, name: &str) -> Result<(), SinkError> {
        match self {
            Self::Local(s) => s.set_target(name),
            Self::Network(_) => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn local_default_kind_is_local() {
        let slot = AudioSinkSlot::local_default();
        assert_eq!(slot.kind(), AudioSinkType::Local);
    }

    #[test]
    fn network_construction_kind_is_network() {
        let slot = AudioSinkSlot::network("127.0.0.1", 1234, Protocol::TcpClient);
        assert_eq!(slot.kind(), AudioSinkType::Network);
    }

    #[test]
    fn set_target_is_noop_on_network_variant() {
        // The network sink doesn't have a concept of device
        // selection — verify `set_target` returns `Ok` without
        // touching the underlying NetworkSink. Exercising both
        // a typical UID and the empty-string "system default"
        // sentinel since those are the two cases the controller
        // actually emits.
        let mut slot = AudioSinkSlot::network("127.0.0.1", 1234, Protocol::Udp);
        assert!(slot.set_target("anything").is_ok());
        assert!(slot.set_target("").is_ok());
    }

    #[test]
    fn variant_swap_round_trip_preserves_kind() {
        // Mirrors the controller's swap path: construct local,
        // verify, swap to network, verify, swap back, verify.
        // Catches a regression where a future refactor
        // accidentally drops the kind() match arm for one
        // variant.
        let mut slot = AudioSinkSlot::local_default();
        assert_eq!(slot.kind(), AudioSinkType::Local);
        slot = AudioSinkSlot::network("localhost", 5555, Protocol::TcpClient);
        assert_eq!(slot.kind(), AudioSinkType::Network);
        slot = AudioSinkSlot::local_default();
        assert_eq!(slot.kind(), AudioSinkType::Local);
    }
}
