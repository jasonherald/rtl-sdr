//! Server panel — "Share over network" controls exposing a local
//! RTL-SDR dongle to remote `rtl_tcp` clients.
//!
//! Always-visible activity panel: lives behind the 📡 Share icon
//! on the left activity bar. The legacy hotplug-gated hide/show
//! behaviour was removed when Share became its own activity — the
//! user's click on the icon is the explicit opt-in gesture, and
//! the Start switch still errors gracefully when no local RTL-SDR
//! is plugged in (an exclusivity-guard toast fires if the local
//! RTL-SDR is currently the active source, since that would cause
//! a USB double-open).
//!
//! The panel itself only builds widgets; the wire-up (start/stop,
//! stats polling, activity log) lives in `window.rs` alongside the
//! rest of the DSP/UI bridge. Keeping this file widget-only mirrors
//! the pattern in `source_panel.rs` / `audio_panel.rs` / etc.

use libadwaita as adw;

/// Config key for the persisted server nickname (mDNS TXT field).
const KEY_SERVER_NICKNAME: &str = "rtl_tcp_server_nickname";
/// Config key for the persisted TCP bind port.
const KEY_SERVER_PORT: &str = "rtl_tcp_server_port";
/// Config key for the persisted bind-address selector index
/// (`BIND_LOOPBACK_IDX` / `BIND_ALL_INTERFACES_IDX`).
const KEY_SERVER_BIND_IDX: &str = "rtl_tcp_server_bind_idx";
/// Config key for the persisted "Announce via mDNS" switch state.
const KEY_SERVER_ADVERTISE: &str = "rtl_tcp_server_advertise";
/// Config key for the persisted default center frequency (Hz).
const KEY_SERVER_DEFAULT_FREQ_HZ: &str = "rtl_tcp_server_default_freq_hz";
/// Config key for the persisted default sample-rate selector
/// index (0..=10 in the 11-entry list). Stored as an index rather
/// than a Hz value so a future rate-table edit doesn't break
/// existing configs.
const KEY_SERVER_DEFAULT_SR_IDX: &str = "rtl_tcp_server_default_sample_rate_idx";
/// Config key for the persisted default tuner gain (dB).
const KEY_SERVER_DEFAULT_GAIN_DB: &str = "rtl_tcp_server_default_gain_db";
/// Config key for the persisted default PPM correction.
const KEY_SERVER_DEFAULT_PPM: &str = "rtl_tcp_server_default_ppm";
/// Config key for the persisted default bias-tee toggle.
const KEY_SERVER_DEFAULT_BIAS_TEE: &str = "rtl_tcp_server_default_bias_tee";
/// Config key for the persisted default direct-sampling toggle.
const KEY_SERVER_DEFAULT_DIRECT_SAMPLING: &str = "rtl_tcp_server_default_direct_sampling";
/// Config key for the persisted compression-codec selector index
/// (`COMPRESSION_OFF_IDX` / `COMPRESSION_LZ4_IDX`). Stored as an
/// index so a future addition (e.g. Zstd) doesn't invalidate old
/// configs — unknown indices fall back to `Off` on restore.
const KEY_SERVER_COMPRESSION_IDX: &str = "rtl_tcp_server_compression_idx";
/// Config key for the persisted listener cap (max `Role::Listen`
/// clients). See [`MIN_LISTENER_CAP`] / [`MAX_LISTENER_CAP`] for
/// the allowed range and [`sdr_server_rtltcp::DEFAULT_LISTENER_CAP`]
/// for the default. Per issue #395.
const KEY_SERVER_LISTENER_CAP: &str = "rtl_tcp_server_listener_cap";
/// Config key for the "Require key" switch state (bool). The key
/// bytes themselves live in the OS keyring under
/// [`KEYRING_KEY_AUTH_KEY`] — `sdr_config` is plaintext JSON,
/// which is the wrong place for secret bytes. Per issue #395.
const KEY_SERVER_REQUIRE_AUTH: &str = "rtl_tcp_server_require_auth";

/// Keyring service name for all `sdr-rs` secrets. Matches the value
/// used in `preferences::accounts_page` so both `RadioReference`
/// and `rtl_tcp` auth-key entries show up under the same service
/// heading in `seahorse` / `Keychain Access`.
pub const KEYRING_SERVICE: &str = "sdr-rs";
/// Keyring entry name holding the `rtl_tcp` pre-shared auth key.
/// Stored as a lowercase-hex string so it round-trips through
/// keyring's `String` API without custom base64/UTF-8 coercion
/// — `rand::rngs::SysRng`-backed keys are arbitrary bytes, not text.
/// Per issue #395.
pub const KEYRING_KEY_AUTH_KEY: &str = "rtl_tcp-server-auth-key";

/// Default TCP port for `rtl_tcp`. Matches upstream `rtl_tcp.c` and
/// every ecosystem client's default. Changing it means users have to
/// know the custom port on every client — keep as a knob but default
/// to the well-known value.
pub const DEFAULT_SERVER_PORT: f64 = 1234.0;
/// Lowest TCP port we'll accept. 1023 and below are privileged on
/// Unix and require `CAP_NET_BIND_SERVICE` / root — we're not going
/// to run as root, so refuse up front.
pub const MIN_SERVER_PORT: f64 = 1024.0;
/// Highest legal TCP port (16-bit unsigned max).
pub const MAX_SERVER_PORT: f64 = 65_535.0;
/// Spin-row per-click step for the port field.
const SERVER_PORT_STEP: f64 = 1.0;
/// Spin-row page step (`PgUp` / `PgDn`) for the port field.
const SERVER_PORT_PAGE: f64 = 100.0;

/// Minimum listener-cap value. 0 is legal — it means
/// "control-only; no listeners allowed" (the user explicitly
/// blocks any `Role::Listen` client). Per issue #395.
pub const MIN_LISTENER_CAP: f64 = 0.0;
/// Maximum listener-cap value the UI lets the user pick. 32 is the
/// soft cap from issue #395 — above that a single dongle's IQ
/// bandwidth starts showing measurable fan-out overhead, and the
/// `ClientSlot` / `ClientRegistry` structs aren't optimized for
/// hundreds of live clients either. Backend accepts larger values
/// via direct library calls; the UI just doesn't expose them.
pub const MAX_LISTENER_CAP: f64 = 32.0;
/// Spin-row per-click step for the listener-cap row.
const LISTENER_CAP_STEP: f64 = 1.0;
/// Spin-row page step (`PgUp` / `PgDn`) for the listener-cap row.
const LISTENER_CAP_PAGE: f64 = 5.0;

/// Subtitle shown on `auth_key_row` when the key is masked
/// (default state). Fixed-length run of bullet chars — doesn't
/// leak key length and renders at the same width as a plausible
/// revealed value so the row height doesn't jump when the user
/// toggles reveal. Per issue #395.
pub const AUTH_KEY_MASKED_PLACEHOLDER: &str = "••••••••••••••••••••••••••••••••";

/// Encode an auth-key byte slice as lowercase hex for keyring
/// storage and clipboard copy. Pre-sized allocation (two hex
/// chars per input byte) keeps the hot "toggle reveal" UI path
/// allocation-free after the initial key load. Per issue #395.
pub fn auth_key_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // write! on String is infallible; _ lets us ignore the
        // Result without burdening callers with unwrap_or_else.
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Decode a lowercase-hex auth-key string back into raw bytes.
/// Strict validation: rejects odd-length, non-ASCII, non-hex
/// input, AND decoded lengths outside
/// `1..=sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN`. Returns
/// `None` for any malformed input; callers treat that as "keyring
/// value is corrupt, regenerate on next toggle-on" rather than
/// letting an oversize payload reach `Server::start` and fail
/// every client at handshake. Per issue #395 + `CodeRabbit`
/// round 1 on PR #406.
pub fn auth_key_from_hex(s: &str) -> Option<Vec<u8>> {
    const HEX_CHARS_PER_BYTE: usize = 2;
    /// Hex-encoded cap matching the backend's byte cap — two
    /// hex chars per byte. A hex string longer than this cannot
    /// decode to a valid auth key, so reject before we bother
    /// allocating.
    const MAX_HEX_CHARS: usize =
        sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN * HEX_CHARS_PER_BYTE;
    if s.is_empty()
        || !s.is_ascii()
        || !s.len().is_multiple_of(HEX_CHARS_PER_BYTE)
        || s.len() > MAX_HEX_CHARS
    {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / HEX_CHARS_PER_BYTE);
    for chunk in s.as_bytes().as_chunks::<HEX_CHARS_PER_BYTE>().0 {
        let hi = char::from(chunk[0]).to_digit(16)?;
        let lo = char::from(chunk[1]).to_digit(16)?;
        // `hi` and `lo` are each 0..=15 (validated by `to_digit(16)`),
        // so `(hi << 4) | lo` fits in u8 with the top 24 bits zero —
        // `u8::try_from` is infallible here but keeps clippy's
        // `cast_possible_truncation` quiet.
        let byte = u8::try_from((hi << 4) | lo).ok()?;
        out.push(byte);
    }
    Some(out)
}

/// Bind-address selector index: loopback-only (127.0.0.1). The
/// default — limits exposure to clients running on the same machine
/// until the user opts into broader access.
pub const BIND_LOOPBACK_IDX: u32 = 0;
/// Bind-address selector index: all interfaces (0.0.0.0).
pub const BIND_ALL_INTERFACES_IDX: u32 = 1;

/// Compression selector index: off — advertise `CodecMask::NONE_ONLY`.
/// Default; preserves wire compatibility with every existing
/// `rtl_tcp` client (vanilla clients never send a hello, and our own
/// client refuses to send one when the server's mDNS TXT says
/// `codecs=1`). See #307.
pub const COMPRESSION_OFF_IDX: u32 = 0;
/// Compression selector index: LZ4 available — advertise
/// `CodecMask::NONE_AND_LZ4`. The server still falls back to
/// uncompressed for clients that don't hello (legacy) or hello
/// without the LZ4 bit set (ours with `NONE_ONLY`).
pub const COMPRESSION_LZ4_IDX: u32 = 1;
/// Number of entries in the compression `StringList`. Load-bearing
/// for the persistence validator — indices `>=` this count are
/// dropped on restore so a future "Zstd" entry doesn't land as
/// garbage in an older build.
const COMPRESSION_COUNT: u32 = 2;

/// Server device-defaults: center frequency default (Hz) applied on
/// start, before the first client connects. Upstream `rtl_tcp.c:389`
/// default. Clients typically tune immediately after connecting, so
/// this only affects the "waiting for client" idle state and any
/// client that doesn't send `SetCenterFreq` before reading data.
const DEFAULT_CENTER_FREQ_HZ: f64 = 100_000_000.0;
/// Minimum tunable frequency (Hz). Real RTL-SDR dongles go lower
/// (~24 MHz native, down to DC in direct-sampling mode), but for
/// defaults-on-start the UI caps at 24 MHz to stay in the dongle's
/// documented range.
const MIN_CENTER_FREQ_HZ: f64 = 24_000_000.0;
/// Maximum tunable frequency (Hz). R820T / R828D top out ~1.7 GHz
/// depending on the tuner; 1.766 GHz is the driver's stated ceiling.
const MAX_CENTER_FREQ_HZ: f64 = 1_766_000_000.0;
/// Frequency spin-row step (1 kHz per click).
const CENTER_FREQ_STEP_HZ: f64 = 1_000.0;
/// Frequency spin-row page step (1 MHz per PgUp/PgDn).
const CENTER_FREQ_PAGE_HZ: f64 = 1_000_000.0;

/// Server device-defaults: sample-rate selector index (2.4 MHz = 7).
/// Same ordering as `source_panel::build_rtlsdr_rows` so keyboard
/// muscle memory matches.
const DEFAULT_SERVER_SAMPLE_RATE_INDEX: u32 = 7;
/// Number of entries in the sample-rate `StringList`. Load-bearing
/// for the persistence validator: any index `>=` this count is
/// treated as a corrupt / transient GTK value and dropped on both
/// restore and persist. Must match the list literal in
/// `build_device_defaults_rows`.
const SAMPLE_RATE_COUNT: u32 = 11;

/// Server device-defaults: gain default (dB). 0.0 dB matches
/// upstream's "auto" gain interpretation when the CLI passes `-g 0`.
/// UI treats 0.0 as auto; any positive value is a manual setting.
const DEFAULT_SERVER_GAIN_DB: f64 = 0.0;
/// Minimum server-gain spin-row value (dB).
const MIN_SERVER_GAIN_DB: f64 = 0.0;
/// Maximum server-gain spin-row value (dB) — widest R820T range.
const MAX_SERVER_GAIN_DB: f64 = 49.6;
/// Server-gain spin-row step (dB).
const SERVER_GAIN_STEP_DB: f64 = 0.1;
/// Server-gain spin-row page step (dB).
const SERVER_GAIN_PAGE_DB: f64 = 1.0;

/// Server device-defaults: PPM correction default. 0 is "no
/// correction" — the user can override if they know their crystal
/// offset.
const DEFAULT_SERVER_PPM: f64 = 0.0;
/// Minimum server PPM correction.
const MIN_SERVER_PPM: f64 = -200.0;
/// Maximum server PPM correction.
const MAX_SERVER_PPM: f64 = 200.0;
/// PPM spin-row step.
const SERVER_PPM_STEP: f64 = 1.0;
/// PPM spin-row page step.
const SERVER_PPM_PAGE: f64 = 10.0;

/// Default server nickname shown until the user edits it. Kept
/// generic — a hostname is substituted at `Server::start()` time in
/// `window.rs`, mirroring the CLI's `sdr-rtl-tcp` default-nickname
/// logic in `sdr-server-rtltcp/src/bin/sdr-rtl-tcp.rs`.
const DEFAULT_NICKNAME: &str = "sdr-rtl-tcp";

// Compile-time invariants for the port and frequency bounds. Moves
// "did I accidentally flip min/max or push the port into privileged
// space" checks from runtime-only test assertions (clippy flags them
// as tautologies on consts) to build-time hard errors.
const _: () = {
    assert!(
        MIN_SERVER_PORT >= 1024.0,
        "server port must be unprivileged"
    );
    assert!(MAX_SERVER_PORT <= 65_535.0, "server port must fit in a u16");
    assert!(MIN_SERVER_PORT <= DEFAULT_SERVER_PORT);
    assert!(DEFAULT_SERVER_PORT <= MAX_SERVER_PORT);
    assert!(MIN_CENTER_FREQ_HZ <= DEFAULT_CENTER_FREQ_HZ);
    assert!(DEFAULT_CENTER_FREQ_HZ <= MAX_CENTER_FREQ_HZ);
    assert!(BIND_LOOPBACK_IDX != BIND_ALL_INTERFACES_IDX);
};

/// Server-panel widget handles — packed into the sidebar as an
/// `AdwPreferencesGroup` and handed to `window.rs` for signal
/// wiring.
///
/// Every row except `widget` / `device_defaults_row` is a leaf
/// control; `window.rs` reads their values at `Server::start()`
/// time and disables them while the server is running so the user
/// can't mutate config out from under a live session.
pub struct ServerPanel {
    /// The `AdwPreferencesGroup` widget rendered under the Share
    /// (📡) activity on the left activity bar. Always visible;
    /// `window.rs` no longer gates it on hotplug state.
    pub widget: adw::PreferencesGroup,
    /// Master share-over-network switch. On → start Server. Off →
    /// stop Server.
    pub share_row: adw::SwitchRow,
    /// User-editable server nickname. Becomes the mDNS TXT
    /// `nickname` field when advertising is on.
    pub nickname_row: adw::EntryRow,
    /// TCP port the server binds to (1024-65535, default 1234).
    pub port_row: adw::SpinRow,
    /// Bind address selector (Loopback / All interfaces).
    pub bind_row: adw::ComboRow,
    /// Whether to announce the running server over mDNS. Defaults
    /// on; the user can turn it off to run locally without LAN
    /// advertisement.
    pub advertise_row: adw::SwitchRow,
    /// Compression-codec selector. Default `COMPRESSION_OFF_IDX`
    /// — wire-compatible with every `rtl_tcp` client. `COMPRESSION_LZ4_IDX`
    /// opts in to offering LZ4 to clients that send a hello; legacy
    /// clients and our own `NONE_ONLY` clients still get uncompressed
    /// via the mutual-codec intersection. See #307.
    pub compression_row: adw::ComboRow,
    /// Listener cap — maximum concurrent `Role::Listen` clients.
    /// 0 = "control only — no listeners allowed". Changes take
    /// effect on the next accept via
    /// [`sdr_server_rtltcp::Server::set_listener_cap`]; existing
    /// listeners are never kicked when the cap is lowered
    /// (surprise disconnection is rude, per #395).
    pub listener_cap_row: adw::SpinRow,
    /// "Require key" master switch. When on, the server generates
    /// (or reloads) a 32-byte pre-shared key and enforces it on
    /// every connecting client via the #394 auth gate. When off,
    /// the server reverts to the pre-#394 open-LAN posture. The
    /// keyring entry persists across toggle-off/on cycles so
    /// flipping back doesn't regenerate the key. Per issue #395.
    pub auth_require_row: adw::SwitchRow,
    /// Auth-key display row — hidden when `auth_require_row` is
    /// off. When on, shows the current key in either masked
    /// (default) or revealed form. Three suffix buttons: reveal
    /// toggle, copy-to-clipboard, regenerate. Wiring lives in
    /// `window.rs` where the running `Server` handle is available
    /// for live `set_auth_key` calls. Per issue #395.
    pub auth_key_row: adw::ActionRow,
    /// Reveal/hide toggle. Icon flips between
    /// `view-conceal-symbolic` (currently visible → click to hide)
    /// and `view-reveal-symbolic` (currently masked → click to
    /// reveal). Caller tracks the on/off state.
    pub auth_key_reveal_button: gtk4::Button,
    /// Copy-to-clipboard button. Always copies the FULL hex key
    /// regardless of whether the display is revealed — users
    /// typically click Copy without clicking Reveal first.
    pub auth_key_copy_button: gtk4::Button,
    /// Regenerate button. Replaces the stored key with a new
    /// `sdr_server_rtltcp::auth::generate_random_auth_key()`
    /// result, saves to keyring, and calls
    /// `Server::set_auth_key` on the running server so the old
    /// key stops working for future reconnects without kicking
    /// already-authenticated clients.
    pub auth_key_regenerate_button: gtk4::Button,
    /// Collapsible group of device-defaults (freq / sample rate /
    /// gain / PPM / bias tee / direct sampling) applied on server
    /// start. Clients override these live via the `rtl_tcp` command
    /// channel — these are just the "before first client" defaults.
    pub device_defaults_row: adw::ExpanderRow,
    /// Center-frequency default applied on server open.
    pub center_freq_row: adw::SpinRow,
    /// Sample-rate default applied on server open.
    pub sample_rate_row: adw::ComboRow,
    /// Tuner-gain default applied on server open. 0.0 = auto.
    pub gain_row: adw::SpinRow,
    /// PPM frequency-correction default applied on server open.
    pub ppm_row: adw::SpinRow,
    /// Bias-tee power-output toggle applied on server open.
    pub bias_tee_row: adw::SwitchRow,
    /// Direct-sampling toggle (Q-branch) applied on server open.
    /// Only useful for HF experimentation; off for normal use.
    pub direct_sampling_row: adw::SwitchRow,
    /// Collapsible "Server status" expander shown only while the
    /// server is running. Children below render the live state
    /// pulled from `ServerStats` every
    /// `STATUS_POLL_INTERVAL`.
    pub status_row: adw::ExpanderRow,
    /// "Client: …" — connected peer socket address or "Waiting for
    /// client" when the accept loop is idle.
    pub status_client_row: adw::ActionRow,
    /// "Uptime: …" — wall-clock time since the current client
    /// connected. Hidden when no client.
    pub status_uptime_row: adw::ActionRow,
    /// "Data rate: …" — rolling Mbps computed from `bytes_sent`
    /// deltas between status polls.
    pub status_data_rate_row: adw::ActionRow,
    /// "Tuned to: …" — reflects the client's most recent
    /// `SetCenterFreq` / `SetSampleRate` / `SetTunerGain` commands.
    pub status_commanded_row: adw::ActionRow,
    /// Stop button packed as a suffix on the expander row. Flips
    /// the master `share_row` switch off, which is the same control
    /// path the user would hit to stop manually.
    pub status_stop_button: gtk4::Button,
    /// Collapsible "Activity log" expander, listing the last
    /// `sdr_server_rtltcp::RECENT_COMMANDS_CAPACITY` commands the
    /// server has received with timestamps. Hidden while the
    /// server isn't running.
    pub activity_log_row: adw::ExpanderRow,
    /// `ListBox` child of `activity_log_row` where individual
    /// activity entries are appended. Held separately from the
    /// expander so the stats poller can rebuild it on updates
    /// without walking the expander's `AdwActionRow` children.
    pub activity_log_list: gtk4::ListBox,
    /// Collapsible "Connected clients" expander listing every
    /// connected client with role badge, duration, and drop
    /// counter. Sibling to `status_row` (which still shows
    /// aggregate "most-recent commander" + data rate state).
    /// Hidden while the server isn't running. Per issue #395.
    pub clients_row: adw::ExpanderRow,
    /// `ListBox` child of `clients_row`, one row per connected
    /// client. Rebuilt from scratch on each stats-poll tick when
    /// the client-id set has changed. Held separately from the
    /// expander so the poller doesn't have to walk the expander's
    /// children. Per issue #395.
    pub clients_list: gtk4::ListBox,
    /// Advisory caption shown when the device-default sample rate
    /// is at or above the "high bandwidth" threshold. Shared copy
    /// with the source panel's same-named row so the user sees a
    /// consistent warning whether they're commanding a high rate
    /// via the server or the client side.
    pub bandwidth_advisory_row: adw::ActionRow,
}

/// Subtitle shown on `status_client_row` when the accept loop is
/// idle. Kept as a const so future i18n can swap every occurrence
/// at once and the "no client yet" vs "some degraded state" render
/// can't drift.
pub const STATUS_WAITING_FOR_CLIENT_SUBTITLE: &str = "Waiting for client";
/// Subtitle shown on data-rate / uptime / commanded rows when the
/// accept loop is idle — same no-client state, different row.
pub const STATUS_IDLE_VALUE_SUBTITLE: &str = "—";

/// Subtitle shown on the activity-log expander when no commands
/// have been received yet. Empty-state text that distinguishes
/// "nothing to show" from "the ring buffer cleared after disconnect"
/// (which also renders as empty but is a different journey).
pub const ACTIVITY_LOG_EMPTY_SUBTITLE: &str = "No commands received yet";

/// Max height the activity-log `ScrolledWindow` grows before
/// scrolling kicks in. Small enough to fit inside the sidebar
/// without dominating it; the expander is collapsed by default so
/// users opt in to seeing the log at all.
const ACTIVITY_LOG_MAX_HEIGHT_PX: i32 = 240;

/// Subtitle shown on the `clients_row` expander header when no
/// clients are connected. Doubles as the placeholder text inside
/// the list itself. Per issue #395.
pub const CLIENTS_LIST_EMPTY_SUBTITLE: &str = "No clients connected";

/// Max height the connected-clients `ScrolledWindow` grows
/// before scrolling kicks in. Same tuning rationale as
/// `ACTIVITY_LOG_MAX_HEIGHT_PX`: fits inside the sidebar without
/// dominating it even when the listener cap is at max (32
/// clients × ~45 px per row ≈ 1,440 px uncapped; we cap at 240).
const CLIENTS_LIST_MAX_HEIGHT_PX: i32 = 240;

mod build;
mod persistence;

pub use build::build_server_panel;
pub use persistence::connect_server_panel_persistence;

#[cfg(test)]
mod tests {
    use super::{auth_key_from_hex, auth_key_to_hex};

    #[test]
    fn auth_key_to_hex_round_trips_through_from_hex() {
        // Every byte value 0..=255 must round-trip through
        // hex encode / decode without loss. Pins the
        // keyring-persistence contract — a key stored today
        // comes back as the exact same bytes on the next
        // launch.
        let bytes: Vec<u8> = (0u8..=255).collect();
        let hex = auth_key_to_hex(&bytes);
        assert_eq!(hex.len(), bytes.len() * 2);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "encoder must emit lowercase hex only"
        );
        let back = auth_key_from_hex(&hex).expect("round-trip decode must succeed");
        assert_eq!(back, bytes);
    }

    #[test]
    fn auth_key_from_hex_rejects_malformed_input() {
        // Empty, odd-length, and non-hex characters all
        // surface as `None` so the keyring reader can fall
        // back to regenerate without panicking. Non-ASCII
        // (the PR #405 regression vector) must also fail
        // cleanly rather than panicking on boundary slicing.
        assert!(auth_key_from_hex("").is_none());
        assert!(auth_key_from_hex("abc").is_none(), "odd length");
        assert!(auth_key_from_hex("xyz0").is_none(), "non-hex chars");
        assert!(auth_key_from_hex("💩💩").is_none(), "non-ASCII emoji");
    }

    #[test]
    fn auth_key_from_hex_rejects_oversize_decoded_length() {
        // Hex string encoding more than `MAX_AUTH_KEY_LEN`
        // bytes must be rejected up-front so a corrupt
        // keyring entry surfaces as "regenerate" rather than
        // reaching `Server::start` and failing every client
        // at handshake. Per `CodeRabbit` round 1 on PR #406.
        let max_bytes = sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN;
        // Exactly at cap: must decode.
        let at_cap = "a".repeat(max_bytes * 2);
        assert!(
            auth_key_from_hex(&at_cap).is_some(),
            "max-length hex must decode"
        );
        // One byte over cap: must reject.
        let over_cap = "a".repeat((max_bytes + 1) * 2);
        assert!(
            auth_key_from_hex(&over_cap).is_none(),
            "oversize hex must be rejected"
        );
    }

    #[test]
    fn auth_key_to_hex_empty_input_produces_empty_string() {
        // Edge case — empty slice is legal input (no key set);
        // encoder must produce an empty string, not panic.
        assert_eq!(auth_key_to_hex(&[]), "");
    }
}
