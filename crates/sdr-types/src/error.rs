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
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
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
