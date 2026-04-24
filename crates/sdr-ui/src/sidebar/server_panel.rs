//! Server panel — "Share over network" controls exposing a local
//! RTL-SDR dongle to remote `rtl_tcp` clients.
//!
//! This panel is hidden by default. It becomes visible when a local
//! RTL-SDR dongle is detected AND it's not currently the active
//! source — exposing the same dongle over `rtl_tcp` while also
//! receiving from it locally would cause a USB-device double-open,
//! so the UI gates the panel on the incompatible state.
//!
//! The panel itself only builds widgets; the wire-up (start/stop,
//! stats polling, activity log) lives in `window.rs` alongside the
//! rest of the DSP/UI bridge. Keeping this file widget-only mirrors
//! the pattern in `source_panel.rs` / `audio_panel.rs` / etc.

use std::sync::Arc;

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

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
/// — `rand::OsRng`-backed keys are arbitrary bytes, not text.
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
    for chunk in s.as_bytes().chunks_exact(HEX_CHARS_PER_BYTE) {
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
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    /// Hidden by default — `window.rs` toggles visibility based on
    /// USB hotplug + active-source state.
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

/// Aggregated status rows rendered under the "Server status"
/// expander. Grouped so the builder stays readable and the
/// top-level `build_server_panel` stays inside clippy's
/// `too_many_lines` limit.
#[allow(
    clippy::struct_field_names,
    reason = "all fields are GTK *Row widgets — shared suffix reads clearly at the call sites"
)]
struct StatusRows {
    expander: adw::ExpanderRow,
    client_row: adw::ActionRow,
    uptime_row: adw::ActionRow,
    data_rate_row: adw::ActionRow,
    commanded_row: adw::ActionRow,
    stop_button: gtk4::Button,
}

/// Build the "Activity log" expander plus its scrollable child
/// `ListBox`. The `ListBox` is wrapped in a `ScrolledWindow` with
/// an `ACTIVITY_LOG_MAX_HEIGHT_PX` cap so the expander doesn't grow
/// the sidebar taller than the viewport when the ring fills up.
fn build_activity_log_row() -> (adw::ExpanderRow, gtk4::ListBox) {
    let row = adw::ExpanderRow::builder()
        .title("Activity log")
        .subtitle(ACTIVITY_LOG_EMPTY_SUBTITLE)
        .visible(false)
        .build();
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(ACTIVITY_LOG_MAX_HEIGHT_PX)
        .child(&list)
        .build();
    // Wrap the scroll in an ActionRow so the expander's layout
    // machinery (which expects rows) renders it correctly. Empty
    // title/subtitle pushes the scroll widget into the row body.
    let wrapper = adw::ActionRow::builder().activatable(false).build();
    wrapper.add_prefix(&scroll);
    row.add_row(&wrapper);
    (row, list)
}

/// Build the "Connected clients" expander + its inner `ListBox`.
/// Mirrors `build_activity_log_row`'s scroll-wrapping pattern
/// so a server with a dozen listeners doesn't balloon the
/// sidebar height. Per issue #395.
fn build_clients_row() -> (adw::ExpanderRow, gtk4::ListBox) {
    let row = adw::ExpanderRow::builder()
        .title("Connected clients")
        .subtitle(CLIENTS_LIST_EMPTY_SUBTITLE)
        .expanded(true)
        .visible(false)
        .build();
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(CLIENTS_LIST_MAX_HEIGHT_PX)
        .child(&list)
        .build();
    let wrapper = adw::ActionRow::builder().activatable(false).build();
    wrapper.add_prefix(&scroll);
    row.add_row(&wrapper);
    (row, list)
}

fn build_status_rows() -> StatusRows {
    let expander = adw::ExpanderRow::builder()
        .title("Server status")
        .subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE)
        .expanded(true)
        .visible(false)
        .build();
    let client_row = adw::ActionRow::builder()
        .title("Client")
        .subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE)
        .build();
    let uptime_row = adw::ActionRow::builder()
        .title("Uptime")
        .subtitle(STATUS_IDLE_VALUE_SUBTITLE)
        .build();
    let data_rate_row = adw::ActionRow::builder()
        .title("Data rate")
        .subtitle(STATUS_IDLE_VALUE_SUBTITLE)
        .build();
    let commanded_row = adw::ActionRow::builder()
        .title("Tuned to")
        .subtitle(STATUS_IDLE_VALUE_SUBTITLE)
        .build();
    let stop_button = gtk4::Button::with_label("Stop");
    stop_button.add_css_class("destructive-action");
    stop_button.set_valign(gtk4::Align::Center);
    expander.add_suffix(&stop_button);
    expander.add_row(&client_row);
    expander.add_row(&uptime_row);
    expander.add_row(&data_rate_row);
    expander.add_row(&commanded_row);
    StatusRows {
        expander,
        client_row,
        uptime_row,
        data_rate_row,
        commanded_row,
        stop_button,
    }
}

/// Rows applied-on-start that live inside the "Device defaults"
/// expander. Return tuple grouped so the top-level builder stays
/// under clippy's `too_many_lines` limit.
#[allow(
    clippy::struct_field_names,
    reason = "all fields are GTK *Row widgets — the shared suffix matches the rest of sidebar/ and reads clearly at call sites"
)]
struct DeviceDefaultsRows {
    center_freq_row: adw::SpinRow,
    sample_rate_row: adw::ComboRow,
    gain_row: adw::SpinRow,
    ppm_row: adw::SpinRow,
    bias_tee_row: adw::SwitchRow,
    direct_sampling_row: adw::SwitchRow,
}

fn build_device_defaults_rows() -> DeviceDefaultsRows {
    let freq_adj = gtk4::Adjustment::new(
        DEFAULT_CENTER_FREQ_HZ,
        MIN_CENTER_FREQ_HZ,
        MAX_CENTER_FREQ_HZ,
        CENTER_FREQ_STEP_HZ,
        CENTER_FREQ_PAGE_HZ,
        0.0,
    );
    let center_freq_row = adw::SpinRow::builder()
        .title("Center frequency (Hz)")
        .adjustment(&freq_adj)
        .digits(0)
        .build();

    // Sample-rate list mirrors the client-side source panel so a
    // user familiar with one knows the other. 2.4 MHz is the default
    // to stay inside RTL-SDR's stable-without-dropouts range.
    let sample_rate_model = gtk4::StringList::new(&[
        "250 kHz",
        "1.024 MHz",
        "1.536 MHz",
        "1.792 MHz",
        "1.920 MHz",
        "2.048 MHz",
        "2.160 MHz",
        "2.4 MHz",
        "2.560 MHz",
        "2.880 MHz",
        "3.2 MHz",
    ]);
    let sample_rate_row = adw::ComboRow::builder()
        .title("Sample rate")
        .model(&sample_rate_model)
        .selected(DEFAULT_SERVER_SAMPLE_RATE_INDEX)
        .build();

    let gain_adj = gtk4::Adjustment::new(
        DEFAULT_SERVER_GAIN_DB,
        MIN_SERVER_GAIN_DB,
        MAX_SERVER_GAIN_DB,
        SERVER_GAIN_STEP_DB,
        SERVER_GAIN_PAGE_DB,
        0.0,
    );
    let gain_row = adw::SpinRow::builder()
        .title("Tuner gain (dB)")
        .subtitle("0 = auto gain")
        .adjustment(&gain_adj)
        .digits(1)
        .build();

    let ppm_adj = gtk4::Adjustment::new(
        DEFAULT_SERVER_PPM,
        MIN_SERVER_PPM,
        MAX_SERVER_PPM,
        SERVER_PPM_STEP,
        SERVER_PPM_PAGE,
        0.0,
    );
    let ppm_row = adw::SpinRow::builder()
        .title("Frequency correction (PPM)")
        .adjustment(&ppm_adj)
        .digits(0)
        .build();

    let bias_tee_row = adw::SwitchRow::builder()
        .title("Bias tee")
        .subtitle("Power remote LNA via antenna connector")
        .build();

    let direct_sampling_row = adw::SwitchRow::builder()
        .title("Direct sampling (Q branch)")
        .subtitle("HF mode — bypasses the tuner")
        .build();

    DeviceDefaultsRows {
        center_freq_row,
        sample_rate_row,
        gain_row,
        ppm_row,
        bias_tee_row,
        direct_sampling_row,
    }
}

/// Build the server-panel widgets. Always visible — the Share
/// activity icon in the left activity bar is the user's opt-in
/// gesture, so the panel no longer hides itself based on hotplug
/// state. When no dongle is plugged in the Start switch errors
/// gracefully; the panel's presence under its dedicated icon is
/// the right UX regardless of current dongle availability.
#[allow(
    clippy::too_many_lines,
    reason = "widget-assembly function — splitting scatters one-time wire-up across many helpers with no readability win"
)]
pub fn build_server_panel() -> ServerPanel {
    let widget = adw::PreferencesGroup::builder()
        .title("Share over network")
        .description("Expose this machine's RTL-SDR dongle to remote rtl_tcp clients")
        .build();

    let share_row = adw::SwitchRow::builder()
        .title("Share over network")
        .subtitle("Start the rtl_tcp server and advertise it on the LAN")
        .build();

    let nickname_row = adw::EntryRow::builder()
        .title("Server nickname")
        .text(DEFAULT_NICKNAME)
        .build();

    let port_adj = gtk4::Adjustment::new(
        DEFAULT_SERVER_PORT,
        MIN_SERVER_PORT,
        MAX_SERVER_PORT,
        SERVER_PORT_STEP,
        SERVER_PORT_PAGE,
        0.0,
    );
    let port_row = adw::SpinRow::builder()
        .title("Port")
        .adjustment(&port_adj)
        .digits(0)
        .build();

    // Order is load-bearing — matches `BIND_LOOPBACK_IDX` /
    // `BIND_ALL_INTERFACES_IDX`. A third "specific interface" option
    // is deferred to #323 because it needs an interface enumerator
    // we haven't wired up yet.
    let bind_model = gtk4::StringList::new(&["Loopback only", "All interfaces"]);
    let bind_row = adw::ComboRow::builder()
        .title("Bind address")
        .model(&bind_model)
        .selected(BIND_LOOPBACK_IDX)
        .build();

    let advertise_row = adw::SwitchRow::builder()
        .title("Announce via mDNS")
        .subtitle("Let LAN clients discover this server by name")
        .active(true)
        .build();

    // Compression model — order matches COMPRESSION_OFF_IDX / _LZ4_IDX.
    // Default is Off to stay wire-compatible with every existing
    // rtl_tcp client on the LAN; opting in is a deliberate click,
    // and even then vanilla clients fall through to uncompressed
    // via the mutual-codec intersection on the server side.
    let compression_model = gtk4::StringList::new(&["Off", "LZ4 (if client supports)"]);
    let compression_row = adw::ComboRow::builder()
        .title("Compression")
        .subtitle("Negotiated per client — legacy clients always get uncompressed")
        .model(&compression_model)
        .selected(COMPRESSION_OFF_IDX)
        .build();

    // Listener cap — per #395. Default pulled from the backend's
    // `DEFAULT_LISTENER_CAP` so a UI-backend drift would surface as
    // a test / build failure rather than a quiet divergence. The
    // `usize` → `f64` cast is lossless on every realistic value
    // (cap is always < 32, and f64 is exact for integers up to
    // `2^53`), but clippy's `cast_precision_loss` lint fires on
    // any `usize as f64` conversion regardless — allow inline
    // with a reason rather than adding a workspace-wide exception.
    #[allow(
        clippy::cast_precision_loss,
        reason = "listener cap is bounded << 2^53, f64 represents it exactly"
    )]
    let default_cap = sdr_server_rtltcp::DEFAULT_LISTENER_CAP as f64;
    let listener_cap_adj = gtk4::Adjustment::new(
        default_cap,
        MIN_LISTENER_CAP,
        MAX_LISTENER_CAP,
        LISTENER_CAP_STEP,
        LISTENER_CAP_PAGE,
        0.0,
    );
    let listener_cap_row = adw::SpinRow::builder()
        .title("Listener cap")
        .subtitle(
            "Max simultaneous Listen clients — 0 disables listeners, change applies on next client",
        )
        .adjustment(&listener_cap_adj)
        .numeric(true)
        .snap_to_ticks(true)
        .build();

    // Auth-key controls (#394/#395). Three widgets: master
    // "Require key" switch, a key-display row that only shows
    // when auth is on, and three suffix buttons for
    // reveal / copy / regenerate. State (current key bytes,
    // currently-revealed flag) lives in `window.rs` where the
    // running `Server` + keyring store are accessible.
    let auth_require_row = adw::SwitchRow::builder()
        .title("Require key")
        .subtitle("Clients must present a pre-shared key to connect — LAN-grade only, not WAN-safe")
        .active(false)
        .build();

    // Auth-key display row — hidden until `auth_require_row` is
    // on. `subtitle_selectable(true)` lets users triple-click the
    // revealed key to copy it without using the Copy button.
    let auth_key_row = adw::ActionRow::builder()
        .title("Key")
        .subtitle(AUTH_KEY_MASKED_PLACEHOLDER)
        .subtitle_selectable(true)
        .visible(false)
        .build();

    // Reveal-toggle button. Icon starts as `view-reveal-symbolic`
    // (masked → click to reveal); window.rs flips it to
    // `view-conceal-symbolic` when the subtitle shows the real
    // key. `.flat()` keeps it visually aligned with the row.
    let auth_key_reveal_button = gtk4::Button::builder()
        .icon_name("view-reveal-symbolic")
        .tooltip_text("Reveal key")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    // Icon-only buttons need an explicit accessible label —
    // screen readers read the label, not the tooltip. The reveal
    // button's label flips in `window.rs` alongside icon_name when
    // toggled. Matches the established pattern in this crate
    // (source_panel, navigation_panel, radio_panel). Per
    // `CodeRabbit` round 1 on PR #406.
    auth_key_reveal_button.update_property(&[gtk4::accessible::Property::Label("Reveal key")]);
    let auth_key_copy_button = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy key to clipboard")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    auth_key_copy_button
        .update_property(&[gtk4::accessible::Property::Label("Copy key to clipboard")]);
    let auth_key_regenerate_button = gtk4::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Regenerate key — old key stops working for future reconnects")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    auth_key_regenerate_button
        .update_property(&[gtk4::accessible::Property::Label("Regenerate key")]);
    auth_key_row.add_suffix(&auth_key_reveal_button);
    auth_key_row.add_suffix(&auth_key_copy_button);
    auth_key_row.add_suffix(&auth_key_regenerate_button);

    let device_defaults_row = adw::ExpanderRow::builder()
        .title("Device defaults")
        .subtitle("Applied when the server opens the dongle — clients override live")
        .build();

    let DeviceDefaultsRows {
        center_freq_row,
        sample_rate_row,
        gain_row,
        ppm_row,
        bias_tee_row,
        direct_sampling_row,
    } = build_device_defaults_rows();

    device_defaults_row.add_row(&center_freq_row);
    device_defaults_row.add_row(&sample_rate_row);
    device_defaults_row.add_row(&gain_row);
    device_defaults_row.add_row(&ppm_row);
    device_defaults_row.add_row(&bias_tee_row);
    device_defaults_row.add_row(&direct_sampling_row);

    let StatusRows {
        expander: status_row,
        client_row: status_client_row,
        uptime_row: status_uptime_row,
        data_rate_row: status_data_rate_row,
        commanded_row: status_commanded_row,
        stop_button: status_stop_button,
    } = build_status_rows();

    let (activity_log_row, activity_log_list) = build_activity_log_row();
    let (clients_row, clients_list) = build_clients_row();

    // Bandwidth advisory — hidden initially. Visibility is toggled
    // on sample-rate changes via the wiring in window.rs, mirroring
    // the source-panel path. Copy is intentionally identical to the
    // source-panel version (shared consts) so users see the same
    // warning wording no matter which side they're configuring.
    let bandwidth_advisory_row = adw::ActionRow::builder()
        .title(crate::sidebar::source_panel::HIGH_BANDWIDTH_ADVISORY_TITLE)
        .subtitle(crate::sidebar::source_panel::HIGH_BANDWIDTH_ADVISORY_SUBTITLE)
        .visible(false)
        .build();
    bandwidth_advisory_row.add_prefix(&gtk4::Image::from_icon_name("dialog-information-symbolic"));

    widget.add(&share_row);
    widget.add(&nickname_row);
    widget.add(&port_row);
    widget.add(&bind_row);
    widget.add(&advertise_row);
    widget.add(&compression_row);
    widget.add(&listener_cap_row);
    widget.add(&auth_require_row);
    widget.add(&auth_key_row);
    widget.add(&device_defaults_row);
    widget.add(&status_row);
    widget.add(&clients_row);
    widget.add(&activity_log_row);
    widget.add(&bandwidth_advisory_row);

    ServerPanel {
        widget,
        share_row,
        nickname_row,
        port_row,
        bind_row,
        advertise_row,
        compression_row,
        listener_cap_row,
        auth_require_row,
        auth_key_row,
        auth_key_reveal_button,
        auth_key_copy_button,
        auth_key_regenerate_button,
        device_defaults_row,
        center_freq_row,
        sample_rate_row,
        gain_row,
        ppm_row,
        bias_tee_row,
        direct_sampling_row,
        status_row,
        status_client_row,
        status_uptime_row,
        status_data_rate_row,
        status_commanded_row,
        status_stop_button,
        activity_log_row,
        activity_log_list,
        clients_row,
        clients_list,
        bandwidth_advisory_row,
    }
}

/// Load saved server-panel values from `config` and wire every
/// editable row to re-persist on change. Called from `window.rs`
/// after the panel is built. Two-phase:
///
/// 1. **Restore** — read each key, fall back to the widget's
///    existing default if the key is absent or of the wrong type.
///    Unknown / corrupt types are silently dropped (the restore
///    path is fire-and-forget — `serde_json`'s `as_*` helpers
///    return `None` on a type mismatch, the `if let Some` guard
///    skips the apply, and the widget keeps its build-time
///    default). No panic path.
/// 2. **Subscribe** — install a notify handler on each editable
///    widget that writes its current value back to `config`. The
///    config manager's auto-save thread picks up the change on
///    its ~1 s tick.
///
/// `GObject` weak refs on the capture side would over-complicate
/// this signal-handler block; `clone()` is fine here because the
/// panel's widgets are all held strongly by the sidebar (= window)
/// lifetime anyway, and the notify handlers only fire on user
/// action — no leak risk from a long-running timer.
#[allow(
    clippy::too_many_lines,
    reason = "linear sequence of 10 persistence bindings — splitting would just fragment a straightforward contract"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "persisted numeric fields (port / freq Hz / ppm) fit well below f64's 52-bit mantissa; the spin rows clamp to u16/u32 ranges at the widget level"
)]
pub fn connect_server_panel_persistence(panel: &ServerPanel, config: &Arc<ConfigManager>) {
    // ---- Phase 1: restore ----
    config.read(|v| {
        if let Some(nickname) = v
            .get(KEY_SERVER_NICKNAME)
            .and_then(serde_json::Value::as_str)
        {
            panel.nickname_row.set_text(nickname);
        }
        if let Some(port) = v.get(KEY_SERVER_PORT).and_then(serde_json::Value::as_u64) {
            let clamped = (port as f64).clamp(MIN_SERVER_PORT, MAX_SERVER_PORT);
            panel.port_row.set_value(clamped);
        }
        if let Some(bind_idx) = v
            .get(KEY_SERVER_BIND_IDX)
            .and_then(serde_json::Value::as_u64)
        {
            // Accept only the legal indices; anything else falls
            // back to loopback (safest default — never silently
            // widens exposure).
            let idx = u32::try_from(bind_idx).unwrap_or(BIND_LOOPBACK_IDX);
            let legal = if idx == BIND_ALL_INTERFACES_IDX {
                BIND_ALL_INTERFACES_IDX
            } else {
                BIND_LOOPBACK_IDX
            };
            panel.bind_row.set_selected(legal);
        }
        if let Some(advertise) = v
            .get(KEY_SERVER_ADVERTISE)
            .and_then(serde_json::Value::as_bool)
        {
            panel.advertise_row.set_active(advertise);
        }
        if let Some(freq) = v
            .get(KEY_SERVER_DEFAULT_FREQ_HZ)
            .and_then(serde_json::Value::as_u64)
        {
            let clamped = (freq as f64).clamp(MIN_CENTER_FREQ_HZ, MAX_CENTER_FREQ_HZ);
            panel.center_freq_row.set_value(clamped);
        }
        if let Some(idx) = v
            .get(KEY_SERVER_DEFAULT_SR_IDX)
            .and_then(serde_json::Value::as_u64)
            && let Ok(idx_u32) = u32::try_from(idx)
            && idx_u32 < SAMPLE_RATE_COUNT
        {
            // Strict bounds check on the stored index: anything
            // past the StringList's last entry is discarded (not
            // silently clamped) so a corrupt config leaves the
            // widget on its build-time default instead of flipping
            // to an arbitrary rate. Same policy as `bind_row`.
            panel.sample_rate_row.set_selected(idx_u32);
        }
        if let Some(gain) = v
            .get(KEY_SERVER_DEFAULT_GAIN_DB)
            .and_then(serde_json::Value::as_f64)
        {
            let clamped = gain.clamp(MIN_SERVER_GAIN_DB, MAX_SERVER_GAIN_DB);
            panel.gain_row.set_value(clamped);
        }
        if let Some(ppm) = v
            .get(KEY_SERVER_DEFAULT_PPM)
            .and_then(serde_json::Value::as_i64)
        {
            let clamped = (ppm as f64).clamp(MIN_SERVER_PPM, MAX_SERVER_PPM);
            panel.ppm_row.set_value(clamped);
        }
        if let Some(bias_tee) = v
            .get(KEY_SERVER_DEFAULT_BIAS_TEE)
            .and_then(serde_json::Value::as_bool)
        {
            panel.bias_tee_row.set_active(bias_tee);
        }
        if let Some(ds) = v
            .get(KEY_SERVER_DEFAULT_DIRECT_SAMPLING)
            .and_then(serde_json::Value::as_bool)
        {
            panel.direct_sampling_row.set_active(ds);
        }
        if let Some(idx) = v
            .get(KEY_SERVER_COMPRESSION_IDX)
            .and_then(serde_json::Value::as_u64)
            && let Ok(idx_u32) = u32::try_from(idx)
            && idx_u32 < COMPRESSION_COUNT
        {
            // Strict bounds check: unknown stored indices fall
            // back to the widget's build-time default (`Off`) so
            // a corrupt config can't silently enable compression.
            panel.compression_row.set_selected(idx_u32);
        }
        if let Some(cap) = v
            .get(KEY_SERVER_LISTENER_CAP)
            .and_then(serde_json::Value::as_u64)
        {
            // Clamp to the UI's advertised range on restore. An
            // out-of-range stored value would have been saved by
            // some other client talking to the same config file
            // (e.g. `sdr-rtl-tcp --listener-cap 999`); the widget
            // still needs to be a valid spin-row value so pin it
            // into [MIN_LISTENER_CAP, MAX_LISTENER_CAP]. Per #395.
            let clamped = (cap as f64).clamp(MIN_LISTENER_CAP, MAX_LISTENER_CAP);
            panel.listener_cap_row.set_value(clamped);
        }
        if let Some(require) = v
            .get(KEY_SERVER_REQUIRE_AUTH)
            .and_then(serde_json::Value::as_bool)
        {
            // Restore the "Require key" toggle state. The key
            // itself lives in the OS keyring; window.rs loads /
            // creates it on toggle-on. Just restore the bool
            // here so the widget reflects the user's last
            // choice; window.rs's connect-active handler
            // kicks off the keyring/server wiring if it was on.
            // Per #395.
            panel.auth_require_row.set_active(require);
        }
    });

    // ---- Phase 2: subscribe ----
    // Nickname: AdwEntryRow fires `connect_changed` on every edit.
    let cfg_nick = Arc::clone(config);
    panel.nickname_row.connect_changed(move |row| {
        let text = row.text();
        cfg_nick.write(|v| {
            v[KEY_SERVER_NICKNAME] = serde_json::json!(text.as_str());
        });
    });
    // Port spin row.
    let cfg_port = Arc::clone(config);
    panel.port_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "spin row bounded to 1024..=65535 at the widget level"
        )]
        let port = row.value() as u64;
        cfg_port.write(|v| {
            v[KEY_SERVER_PORT] = serde_json::json!(port);
        });
    });
    // Bind-address combo. Only persist legal indices — GTK's
    // ComboRow can emit transient out-of-range values during
    // widget-model churn (e.g. a repopulation mid-drag). Writing
    // those verbatim would corrupt the next startup's restore,
    // which would then silently fall back to loopback and hide
    // the drift. Strict gate here + on the restore side keeps
    // the persisted state well-formed.
    let cfg_bind = Arc::clone(config);
    panel.bind_row.connect_selected_notify(move |row| {
        let selected = row.selected();
        if selected == BIND_LOOPBACK_IDX || selected == BIND_ALL_INTERFACES_IDX {
            cfg_bind.write(|v| {
                v[KEY_SERVER_BIND_IDX] = serde_json::json!(selected);
            });
        }
    });
    // Advertise switch.
    let cfg_adv = Arc::clone(config);
    panel.advertise_row.connect_active_notify(move |row| {
        cfg_adv.write(|v| {
            v[KEY_SERVER_ADVERTISE] = serde_json::json!(row.is_active());
        });
    });
    // Center frequency spin row (device default).
    let cfg_freq = Arc::clone(config);
    panel.center_freq_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "spin row bounded to u32-valid Hz range at the widget level"
        )]
        let hz = row.value() as u64;
        cfg_freq.write(|v| {
            v[KEY_SERVER_DEFAULT_FREQ_HZ] = serde_json::json!(hz);
        });
    });
    // Sample-rate combo (device default). Same strict-gate policy
    // as `bind_row` — don't persist transient out-of-range values
    // from GTK widget-model churn.
    let cfg_sr = Arc::clone(config);
    panel.sample_rate_row.connect_selected_notify(move |row| {
        let selected = row.selected();
        if selected < SAMPLE_RATE_COUNT {
            cfg_sr.write(|v| {
                v[KEY_SERVER_DEFAULT_SR_IDX] = serde_json::json!(selected);
            });
        }
    });
    // Gain spin row (device default).
    let cfg_gain = Arc::clone(config);
    panel.gain_row.connect_value_notify(move |row| {
        cfg_gain.write(|v| {
            v[KEY_SERVER_DEFAULT_GAIN_DB] = serde_json::json!(row.value());
        });
    });
    // PPM spin row (device default).
    let cfg_ppm = Arc::clone(config);
    panel.ppm_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "spin row bounded to ±200 at the widget level"
        )]
        let ppm = row.value() as i64;
        cfg_ppm.write(|v| {
            v[KEY_SERVER_DEFAULT_PPM] = serde_json::json!(ppm);
        });
    });
    // Bias-tee switch.
    let cfg_bt = Arc::clone(config);
    panel.bias_tee_row.connect_active_notify(move |row| {
        cfg_bt.write(|v| {
            v[KEY_SERVER_DEFAULT_BIAS_TEE] = serde_json::json!(row.is_active());
        });
    });
    // Direct-sampling switch.
    let cfg_ds = Arc::clone(config);
    panel.direct_sampling_row.connect_active_notify(move |row| {
        cfg_ds.write(|v| {
            v[KEY_SERVER_DEFAULT_DIRECT_SAMPLING] = serde_json::json!(row.is_active());
        });
    });
    // Compression codec combo. Same strict-gate policy as
    // `bind_row` / `sample_rate_row` — only persist in-range
    // indices so widget-model churn can't corrupt the stored value.
    let cfg_comp = Arc::clone(config);
    panel.compression_row.connect_selected_notify(move |row| {
        let selected = row.selected();
        if selected < COMPRESSION_COUNT {
            cfg_comp.write(|v| {
                v[KEY_SERVER_COMPRESSION_IDX] = serde_json::json!(selected);
            });
        }
    });
    // Listener cap spin row. Persist on every change so the next
    // session restores the same cap. Applying the new value to a
    // running server (`Server::set_listener_cap`) is wired
    // separately in `window.rs` where the live `Server` handle
    // lives. Per #395.
    let cfg_cap = Arc::clone(config);
    panel.listener_cap_row.connect_value_notify(move |row| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "spin row bounded to [MIN_LISTENER_CAP, MAX_LISTENER_CAP] at the widget level"
        )]
        let cap = row.value() as u64;
        cfg_cap.write(|v| {
            v[KEY_SERVER_LISTENER_CAP] = serde_json::json!(cap);
        });
    });
    // "Require key" switch — persist the bool to sdr_config. The
    // key bytes themselves live in the OS keyring, managed by
    // window.rs. Per #395.
    let cfg_auth = Arc::clone(config);
    panel.auth_require_row.connect_active_notify(move |row| {
        cfg_auth.write(|v| {
            v[KEY_SERVER_REQUIRE_AUTH] = serde_json::json!(row.is_active());
        });
    });
}

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
