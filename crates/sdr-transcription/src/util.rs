//! Shared utilities for transcription backends.
//!
//! Centralizes the unsafe libc calls for wall-clock time formatting so
//! both backends share one reviewed implementation instead of each
//! adding its own `#[allow(unsafe_code)]` block.

/// Return the current wall-clock time formatted as "HH:MM:SS" in local time.
///
/// Uses `libc::localtime_r` for timezone-aware formatting without pulling in
/// the `chrono` crate. Both `WhisperBackend` and `SherpaBackend` call this
/// to timestamp committed transcription events.
///
/// # Safety
///
/// This function contains the only `unsafe` blocks in `sdr-transcription`.
/// The intentional exception to the workspace `unsafe_code = "deny"` lint
/// is tracked in <https://github.com/jasonherald/rtl-sdr/issues/250> — the
/// rationale is that pulling in `chrono` or `time` for one `strftime` call
/// is overkill for a 200 KB-plus dep tree, and the libc shim has detailed
/// SAFETY comments on each unsafe block. Re-evaluate if we ever add chrono
/// transitively for another reason.
#[allow(unsafe_code)]
pub fn wall_clock_timestamp() -> String {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };

    // SAFETY: gettimeofday writes into the provided buffer and is thread-safe.
    // We pass null for the timezone (deprecated parameter).
    // Tracking issue for this unsafe_code exception: #250
    #[allow(unsafe_code)]
    let epoch = unsafe {
        libc::gettimeofday(&raw mut tv, std::ptr::null_mut());
        tv.tv_sec
    };

    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();

    // SAFETY: localtime_r is the reentrant (thread-safe) variant.
    // We provide a valid `time_t` and a valid output buffer.
    // Returns null on failure, in which case we fall back to UTC via gmtime_r.
    // Tracking issue for this unsafe_code exception: #250
    #[allow(unsafe_code)]
    let tm = unsafe {
        let result = libc::localtime_r(&raw const epoch, tm.as_mut_ptr());
        let result = if result.is_null() {
            libc::gmtime_r(&raw const epoch, tm.as_mut_ptr())
        } else {
            result
        };
        if result.is_null() {
            return "00:00:00".to_owned();
        }
        tm.assume_init()
    };

    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_format_is_hhmmss() {
        let ts = wall_clock_timestamp();
        // Expect "HH:MM:SS" — 8 chars, two colons.
        assert_eq!(ts.len(), 8);
        assert_eq!(ts.chars().filter(|&c| c == ':').count(), 2);
    }
}
