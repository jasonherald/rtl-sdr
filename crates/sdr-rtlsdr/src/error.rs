//! Error types for the RTL-SDR driver.

/// Errors from RTL-SDR USB operations.
#[derive(Debug, thiserror::Error)]
pub enum RtlSdrError {
    /// USB communication error.
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),

    /// Device not found at the specified index.
    #[error("device not found at index {0}")]
    DeviceNotFound(u32),

    /// No supported tuner detected on the device.
    #[error("no supported tuner found")]
    NoTuner,

    /// Tuner operation failed.
    #[error("tuner error: {0}")]
    Tuner(String),

    /// Invalid sample rate.
    #[error("invalid sample rate: {0} Hz")]
    InvalidSampleRate(u32),

    /// Invalid parameter.
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),

    /// Device is busy (async read in progress).
    #[error("device busy")]
    DeviceBusy,

    /// Device was lost (USB disconnect).
    #[error("device lost")]
    DeviceLost,

    /// Register write/read failed.
    #[error("register access failed")]
    RegisterAccess,
}
