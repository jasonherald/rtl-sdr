use super::*;

/// Compile-time validation that DSP buffer constants are consistent.
const _: () = {
    assert!(DEFAULT_FFT_SIZE > 0);
    assert!(DEFAULT_SAMPLE_RATE > 0.0);
    assert!(DEFAULT_CENTER_FREQ > 0.0);
    assert!(RECV_TIMEOUT_MS > 0);
    assert!(VFO_OUTPUT_PADDING > 0);
};

/// Drain every pending `DspToUi` event from `rx`.
fn drain(rx: &mpsc::Receiver<DspToUi>) -> Vec<DspToUi> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

fn test_pre_lock_snapshot() -> crate::acars_airband_lock::PreLockSnapshot {
    crate::acars_airband_lock::PreLockSnapshot {
        source_rate_hz: 2_400_000.0,
        center_freq_hz: 100_000_000.0,
        vfo_offset_hz: 0.0,
        source_type: SourceType::RtlSdr,
        frontend_decim: 8,
    }
}

// ACARS decode-tap unit tests (#474). Inlined here per the
// workspace convention (tests at file bottom in
// `#[cfg(test)] mod tests`); access `acars_decode_tap`
// directly through the module hierarchy. End-to-end
// engage→ack→disengage is covered by the `Engine`-API
// integration test in `tests/acars_pipeline_integration.rs`.

use crate::acars_airband_lock::{ACARS_CENTER_HZ, ACARS_SOURCE_RATE_HZ, US_SIX_CHANNELS_HZ};

mod frontend_vfo;
mod lifecycle;
mod lrpt;
mod recording_acars;
mod sstv;
