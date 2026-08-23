use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use sdr_server_rtltcp::{ClientInfo, InitialDeviceState, codec::Codec};

use super::{
    SERVER_STATUS_POLL_INTERVAL, format_commanded_state, format_data_rate, format_hz, format_uptime,
};

// ============================================================
// Test fixture constants (`CodeRabbit` round 2 on PR #402).
// Names make each scenario's intent obvious at a glance:
// "is this testing 145 MHz 2m-band tune or 100 MHz WFM"
// reads clearer when the literal has a rationale.
// ============================================================

/// Placeholder peer port for `ClientInfo` fixtures that don't
/// exercise the peer address field — any non-privileged port
/// works, so pick one well above the well-known range.
const FIXTURE_PEER_PORT: u16 = 42_000;
/// 2-meter amateur band test frequency (145.5 MHz) — stands in
/// for "non-default freq the user commanded" in fallback tests.
const FIXTURE_FREQ_2M_HZ: u32 = 145_500_000;
/// 100 MHz WFM broadcast band test frequency — second sample
/// to catch tests that pass on the 2m fixture by coincidence.
const FIXTURE_FREQ_WFM_HZ: u32 = 100_000_000;
/// Typical RTL-SDR sample rate (2.4 Msps) — used across tune
/// fixtures.
const FIXTURE_SAMPLE_RATE_HZ: u32 = 2_400_000;
/// Mid-range tuner gain in tenths-of-dB (29.6 dB) — well
/// inside the R820T table so "auto vs manual" branches aren't
/// ambiguous.
const FIXTURE_GAIN_MID_TENTHS: i32 = 296;
/// Upper-range tuner gain in tenths-of-dB (49.6 dB) — matches
/// the R820T's documented top step so the "manual gain in dB"
/// formatter has a realistic ceiling value.
const FIXTURE_GAIN_TOP_TENTHS: i32 = 496;
/// Low-but-visible manual gain in tenths-of-dB (20 dB) —
/// used specifically in the "auto overrides manual" test to
/// prove the auto flag wins over any set value.
const FIXTURE_GAIN_LOW_TENTHS: i32 = 200;

/// Fresh `InitialDeviceState` matching what `Server::start`
/// stores when the user takes the upstream-default path. Most
/// format tests use this; the ones that want to prove
/// fallback-to-initial override the relevant field.
fn default_initial() -> InitialDeviceState {
    InitialDeviceState::default()
}

/// Build a `ClientInfo` fixture for the `format_commanded_state`
/// tests. Defaults to unset per-session fields (`None` on
/// `current_freq` / `current_sample_rate` / `current_gain`) so
/// each test only overrides the fields it's exercising.
fn info(
    current_freq_hz: Option<u32>,
    current_sample_rate_hz: Option<u32>,
    current_gain_tenths_db: Option<i32>,
    current_gain_auto: Option<bool>,
) -> ClientInfo {
    ClientInfo {
        id: 0,
        peer: SocketAddr::from(([127, 0, 0, 1], FIXTURE_PEER_PORT)),
        connected_since: Instant::now(),
        codec: Codec::None,
        role: sdr_server_rtltcp::extension::Role::Control,
        bytes_sent: 0,
        buffers_dropped: 0,
        last_command: None,
        current_freq_hz,
        current_sample_rate_hz,
        current_gain_tenths_db,
        current_gain_auto,
        recent_commands: VecDeque::new(),
    }
}

#[test]
fn format_uptime_uses_compact_unit_picker() {
    // Sub-minute: just seconds.
    assert_eq!(format_uptime(Duration::from_secs(5)), "5s");
    // Sub-hour: minutes + seconds, no hours prefix.
    assert_eq!(format_uptime(Duration::from_secs(61)), "1m 1s");
    assert_eq!(format_uptime(Duration::from_secs(3599)), "59m 59s");
    // Hour+: full triple.
    assert_eq!(format_uptime(Duration::from_secs(3661)), "1h 1m 1s");
    assert_eq!(format_uptime(Duration::from_secs(7322)), "2h 2m 2s");
}

#[test]
fn format_data_rate_picks_kbps_below_mbps_boundary() {
    // 0.5 Mbps worth of bytes over the 500 ms interval → 0.5 Mbps
    // → still kbps under the 1 Mbps switchover. (1 Mbps =
    // 125_000 bytes/s, so 500 ms of 0.5 Mbps is 31_250 bytes.)
    assert_eq!(
        format_data_rate(31_250, SERVER_STATUS_POLL_INTERVAL),
        "500.0 kbps"
    );
    // ~4.8 Mbps (the rtl_tcp canonical rate) over 500 ms.
    // 4.8 Mbps * 0.5 s = 2.4 Mbit = 300_000 bytes.
    assert_eq!(
        format_data_rate(300_000, SERVER_STATUS_POLL_INTERVAL),
        "4.80 Mbps"
    );
    // Zero bytes → "0.0 kbps" not a panic.
    assert_eq!(format_data_rate(0, SERVER_STATUS_POLL_INTERVAL), "0.0 kbps");
}

#[test]
fn format_data_rate_handles_zero_interval() {
    // A degenerate 0-second interval would divide by zero; fn
    // must return a safe sentinel so the row renders instead of
    // crashing.
    assert_eq!(format_data_rate(100, Duration::ZERO), "—");
}

#[test]
fn format_hz_picks_unit_by_magnitude() {
    assert_eq!(format_hz(500), "500 Hz");
    assert_eq!(format_hz(1_500), "1.500 kHz");
    assert_eq!(format_hz(100_300_000), "100.300 MHz");
    assert_eq!(format_hz(1_500_000_000), "1.500 GHz");
}

#[test]
fn format_commanded_state_no_client_renders_idle_placeholder() {
    // `None` means no connected client — the row should show
    // the idle `STATUS_IDLE_VALUE_SUBTITLE` placeholder. Guards
    // against a phantom row when the server is up but nobody's
    // connected.
    let subtitle = format_commanded_state(None, &default_initial());
    assert_eq!(
        subtitle,
        crate::sidebar::server_panel::STATUS_IDLE_VALUE_SUBTITLE
    );
}

#[test]
fn format_commanded_state_falls_back_to_server_initial_when_client_silent() {
    // A connected client that hasn't sent any commands yet —
    // row should render the SERVER'S configured `initial`
    // values (what the user configured at `Server::start`),
    // not the library's upstream `rtl_tcp.c` defaults. Here
    // the initial is a non-default 145 MHz / 2.4 Msps / 29.6 dB,
    // so the subtitle should read those values even though the
    // client hasn't sent any SetX commands yet.
    // Per `CodeRabbit` round 1 on PR #402.
    let initial = InitialDeviceState {
        center_freq_hz: FIXTURE_FREQ_2M_HZ,
        sample_rate_hz: FIXTURE_SAMPLE_RATE_HZ,
        gain_tenths_db: Some(FIXTURE_GAIN_MID_TENTHS),
        ..InitialDeviceState::default()
    };
    let subtitle = format_commanded_state(Some(&info(None, None, None, None)), &initial);
    assert!(
        subtitle.contains("145.500 MHz"),
        "server's configured initial freq should show: {subtitle}"
    );
    assert!(
        subtitle.contains("2.400 MHz"),
        "server's configured initial sample rate should show: {subtitle}"
    );
    assert!(
        subtitle.contains("gain 29.6 dB"),
        "server's configured initial gain should show: {subtitle}"
    );
}

#[test]
fn format_commanded_state_renders_auto_when_initial_gain_is_none() {
    // `initial.gain_tenths_db = None` encodes upstream's
    // automatic-gain mode (the CLI's `-g 0` path). With no
    // client overrides, the gain text should read "auto", not
    // a literal dB value. Regression for the pre-CR "initial"
    // placeholder that was meaningless to users.
    let initial = InitialDeviceState {
        gain_tenths_db: None,
        ..InitialDeviceState::default()
    };
    let subtitle = format_commanded_state(Some(&info(None, None, None, None)), &initial);
    assert!(
        subtitle.contains("gain auto"),
        "initial gain None should render as auto: {subtitle}"
    );
}

#[test]
fn format_commanded_state_renders_client_auto_gain_preference() {
    // When the client has sent SetGainMode(auto), "auto" wins
    // regardless of any previous manual gain value OR the
    // server's configured initial gain.
    let client = info(
        Some(FIXTURE_FREQ_2M_HZ),
        Some(FIXTURE_SAMPLE_RATE_HZ),
        Some(FIXTURE_GAIN_LOW_TENTHS),
        Some(true),
    );
    let subtitle = format_commanded_state(Some(&client), &default_initial());
    assert!(subtitle.contains("145.500 MHz"));
    assert!(subtitle.contains("2.400 MHz"));
    assert!(
        subtitle.contains("gain auto"),
        "client auto should override manual gain value: {subtitle}"
    );
}

#[test]
fn format_commanded_state_renders_manual_gain_in_db() {
    // SetTunerGain records tenths-of-dB; the render converts to
    // full dB with one decimal.
    let client = info(
        Some(FIXTURE_FREQ_WFM_HZ),
        Some(FIXTURE_SAMPLE_RATE_HZ),
        Some(FIXTURE_GAIN_TOP_TENTHS),
        Some(false),
    );
    let subtitle = format_commanded_state(Some(&client), &default_initial());
    assert!(
        subtitle.contains("gain 49.6 dB"),
        "49.6 dB should render from 496 tenths: {subtitle}"
    );
}

#[test]
fn format_log_age_buckets() {
    use super::format_log_age;
    // < 2 s → "just now" debounces the 500 ms poll from showing
    // "0s ago" / "1s ago" noise on the most-recent entry.
    assert_eq!(format_log_age(Duration::from_millis(0)), "just now");
    assert_eq!(format_log_age(Duration::from_millis(1999)), "just now");
    // 2 s – 59 s → "Ns ago"
    assert_eq!(format_log_age(Duration::from_secs(2)), "2s ago");
    assert_eq!(format_log_age(Duration::from_secs(59)), "59s ago");
    // 1 m – 59 m → "Nm ago"
    assert_eq!(format_log_age(Duration::from_mins(1)), "1m ago");
    assert_eq!(format_log_age(Duration::from_secs(3599)), "59m ago");
    // 1 h+ → "Nh ago" (rare — single-session command histories
    // almost never live long enough, but the bucket keeps the
    // formatter total).
    assert_eq!(format_log_age(Duration::from_hours(1)), "1h ago");
}
