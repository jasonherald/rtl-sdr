#![allow(
    // C API surface has many names (`dongle_info_t`, `rtl_tcp`, `ntohl`)
    // that don't benefit from markdown backticks in our prose.
    clippy::doc_markdown,
    // Arc<Mutex<...>> is moved into thread::spawn closures then consumed
    // by worker fns; the pass-by-value is semantic ownership transfer,
    // not an oversight.
    clippy::needless_pass_by_value,
    // Wire protocol layer has tight u32 ↔ i32 reinterprets that match
    // upstream `ntohl` + C int/short casts exactly. Faithful port.
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
//! rtl_tcp-compatible server.
//!
//! Faithful port of `original/librtlsdr/src/rtl_tcp.c` — a TCP server
//! that streams 8-bit unsigned-offset I/Q samples from a locally-connected
//! RTL-SDR dongle and accepts tuning commands from the client over the
//! same socket.
//!
//! Used by:
//! - GQRX / SDR++ / SDR# / SoapySDR clients (as the server side)
//! - Our own [`sdr-source-network`] in rtl_tcp mode (on the client side)
//!
//! The wire protocol lives in [`protocol`]; the server lifecycle and
//! threading model in [`server`]; command-to-device translation in
//! [`dispatch`]. See module docs for the upstream line-number references.
//!
//! The multi-client fan-out infrastructure lives in [`broadcaster`] —
//! `ClientRegistry` + `ClientSlot` + per-client stats. Introduced by
//! #391 as the first step of the #390 multi-client epic; the data
//! path flip that consumes it ships in the next commit.

pub mod broadcaster;
pub mod codec;
pub mod dispatch;
pub mod error;
pub mod extension;
pub mod protocol;
pub mod server;

pub use broadcaster::{ClientId, ClientInfo};
pub use error::ServerError;
pub use protocol::{
    COMMAND_LEN, Command, CommandOp, DEFAULT_PORT, DONGLE_INFO_LEN, DONGLE_MAGIC, DongleInfo,
    TunerTypeCode,
};
pub use server::{
    DEFAULT_BUFFER_CAPACITY, DEFAULT_CENTER_FREQ_HZ, DEFAULT_SAMPLE_RATE_HZ, InitialDeviceState,
    READ_BUFFER_LEN, Server, ServerConfig, ServerStats, TunerAdvertiseInfo,
};
