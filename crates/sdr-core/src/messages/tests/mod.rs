use super::*;

/// Fixed bandwidth used by the message-variant round-trip
/// tests. 12.5 kHz is NFM's default and the value the VFO-drag
/// feedback loop most commonly emits in practice — hoisting it
/// to a const both removes the magic-number duplication in the
/// construct + match and documents the choice of value.
const TEST_BANDWIDTH_HZ: f64 = 12_500.0;

/// Fixed VFO offset used by the `VfoOffsetChanged` round-trip
/// test. 25 kHz is a representative non-zero offset that
/// click-to-tune / drag flows routinely emit — same hoisting
/// rationale as `TEST_BANDWIDTH_HZ`: avoids a magic-number
/// duplicated between construct and match.
const TEST_VFO_OFFSET_HZ: f64 = 25_000.0;

mod dsp_to_ui;
mod scanner_acars_sstv;
mod ui_to_dsp;
