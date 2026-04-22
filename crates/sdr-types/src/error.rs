/// Errors from DSP operations.
#[derive(Debug, thiserror::Error)]
pub enum DspError {
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),
    #[error("buffer too small: need {need}, got {got}")]
    BufferTooSmall { need: usize, got: usize },
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
    /// by retrying (bad server, wrong protocol version, auth rejection).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Transient, retryable failure from a network source — e.g. the
    /// `rtl_tcp` server's extended handshake returned `ControllerBusy`
    /// because another client is currently controlling the dongle.
    /// The condition is expected to resolve without user action once
    /// the other client disconnects, so the connection manager keeps
    /// retrying on the normal backoff schedule rather than
    /// transitioning to `Failed`. Distinct from `Protocol` so callers
    /// can route terminal vs. retry-worthy rejections correctly.
    #[error("temporarily unavailable: {0}")]
    TemporarilyUnavailable(String),
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
