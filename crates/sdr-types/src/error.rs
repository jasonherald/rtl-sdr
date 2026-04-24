/// Errors from DSP operations.
#[derive(Debug, thiserror::Error)]
pub enum DspError {
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("buffer too small: need {need}, got {got}")]
    BufferTooSmall { need: usize, got: usize },
    /// GPU-backed DSP path failed to initialise or run. Covers "no
    /// compatible adapter", "device request failed", "shader
    /// compile rejected by driver", and mid-transform mapping
    /// failures. Carries a human-readable message so the caller
    /// can log it or fall back to a CPU engine.
    #[error("gpu unavailable: {0}")]
    GpuUnavailable(String),
}

/// Errors from pipeline/streaming operations.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("stream stopped")]
    StreamStopped,
    #[error("block not running")]
    BlockNotRunning,
    #[error("source error: {0}")]
    Source(#[from] SourceError),
    #[error("sink error: {0}")]
    Sink(#[from] SinkError),
}

/// Errors from source modules.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("device open failed: {0}")]
    OpenFailed(String),
    #[error("tune failed: {0}")]
    TuneFailed(String),
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("not running")]
    NotRunning,
    #[error("already running")]
    AlreadyRunning,
    #[error("read failed: {0}")]
    ReadFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Wire-protocol-level failure from a network source — e.g. a stream
    /// that was supposed to speak `rtl_tcp` but didn't return the expected
    /// 12-byte `RTL0` header. Distinct from `Io` so UI can surface
    /// "not an `rtl_tcp` server" rather than a generic socket error.
    ///
    /// Treated as **terminal** by the `rtl_tcp` client's connection manager:
    /// the backoff loop exits and the state transitions to
    /// `ConnectionState::Failed`. Use this for errors that won't be fixed
    /// by retrying and that don't have a dedicated variant below (bad
    /// server, wrong protocol version, unexpected handshake status codes
    /// like `Status::ListenerCapReached`, LZ4 mid-stream decode failure).
    ///
    /// **Not** for role-denial errors (pre-#396 those folded in here with
    /// a `"not an rtl_tcp server: ..."` or similar reason string — now
    /// routed to [`Self::ControllerBusy`] / [`Self::AuthRequired`] /
    /// [`Self::AuthFailed`] so the connection manager can publish a
    /// distinct [`crate::RtlTcpConnectionState`] variant and the UI
    /// can offer a specific recovery action instead of a generic
    /// "Failed — \<reason\>" toast).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Transient, retryable failure from a network source. Used for
    /// conditions the connection manager expects to resolve without
    /// user action — currently none of the `rtl_tcp` extended
    /// handshake status codes. The backoff loop keeps retrying on
    /// the normal schedule rather than transitioning to a terminal
    /// state.
    ///
    /// **Not** for `Status::ControllerBusy`: pre-#396 this variant
    /// wrapped busy rejections with silent auto-retry, which hid the
    /// decision point (Take control? / Connect as Listener? / give
    /// up?) from the user. Per #396, `ControllerBusy` now routes to
    /// the dedicated [`Self::ControllerBusy`] variant (terminal,
    /// no auto-retry) so the UI can surface the choice explicitly
    /// via a toast.
    #[error("temporarily unavailable: {0}")]
    TemporarilyUnavailable(String),

    /// Server denied the connect with `Status::ControllerBusy`
    /// (#392) and the user needs to explicitly decide what to
    /// do next — either connect as Listener, force a takeover,
    /// or give up. Treated as terminal by the connection
    /// manager (no auto-retry) per #396; the UI surfaces
    /// the choice via a toast with "Take control" / "Connect
    /// as Listener" action buttons. Distinct from
    /// [`Self::TemporarilyUnavailable`] (which auto-retries)
    /// because a user-facing decision is needed. Per #396.
    #[error("controller slot is occupied")]
    ControllerBusy,

    /// Server requires a pre-shared key (#394) and the client
    /// didn't send one. Terminal from the connection manager's
    /// perspective (no auto-retry); the UI re-prompts for a
    /// key and triggers a fresh connect. Distinct from
    /// [`Self::Protocol`] (which is where this folded pre-#396,
    /// as `"protocol error: server requires auth"`) so the UI
    /// can reveal / focus the Server-key entry row instead of
    /// showing a generic failure toast. Per #396.
    #[error("server requires an authentication key")]
    AuthRequired,

    /// Server required a key and the client's attempt was
    /// rejected (`Status::AuthFailed`). Terminal from the
    /// connection manager's perspective; the UI re-prompts
    /// with "Key rejected" copy. Distinct from
    /// [`Self::AuthRequired`] (which means the client never
    /// sent a key) so the toast can tell "never tried" vs
    /// "wrong key" apart. Per #396.
    #[error("authentication key rejected")]
    AuthFailed,
}

/// Errors from sink modules.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("device open failed: {0}")]
    OpenFailed(String),
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("not running")]
    NotRunning,
    #[error("already running")]
    AlreadyRunning,
    #[error("channel disconnected")]
    Disconnected,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors from configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(String),
    #[error("missing key: {0}")]
    MissingKey(String),
}

/// Errors from RTL-SDR USB operations.
#[derive(Debug, thiserror::Error)]
pub enum RtlsdrError {
    #[error("device not found")]
    DeviceNotFound,
    #[error("USB error: {0}")]
    Usb(String),
    #[error("tuner error: {0}")]
    Tuner(String),
    #[error("invalid sample rate: {0}")]
    InvalidSampleRate(u32),
    #[error("device busy")]
    DeviceBusy,
    #[error("timeout")]
    Timeout,
}
