//! 32-bit attached-sync-marker (ASM) frame-sync correlator.
//!
//! CCSDS-standard ASM is `0x1ACFFC1D`. We slide a 32-bit window
//! over the bitstream from the Viterbi decoder, compute Hamming
//! distance against the ASM, and emit a "frame start" marker
//! whenever the distance falls at or below [`SYNC_THRESHOLD`] bits.
//!
//! Threshold of 4 bits matches medet's tolerance — anything wider
//! produces too many false syncs in noisy passes.
//!
//! Reference (read-only): `original/medet/correlator.pas`.

/// CCSDS attached sync marker.
pub const ASM: u32 = 0x1ACF_FC1D;

/// Bit width of the ASM correlation window. Pinned as a constant
/// so the decoder logic, helper code, and tests share one source
/// of truth — changing the ASM size (CCSDS doesn't, but in case a
/// derived spec ever does) lands in one place.
pub const ASM_BITS: usize = 32;

/// Maximum Hamming distance for a sync hit. 4/32 ≈ 87.5% match.
pub const SYNC_THRESHOLD: u32 = 4;

/// Streaming sync correlator. Pushes one bit at a time, emits the
/// 1-based bit-position of detected sync words.
pub struct SyncCorrelator {
    window: u32,
    bits_seen: u64,
}

impl Default for SyncCorrelator {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncCorrelator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            window: 0,
            bits_seen: 0,
        }
    }

    /// Push one bit. Returns the position of the sync end if the
    /// sliding 32-bit window hits the ASM within
    /// [`SYNC_THRESHOLD`] bit errors.
    pub fn push(&mut self, bit: u8) -> Option<u64> {
        self.window = (self.window << 1) | u32::from(bit & 1);
        self.bits_seen += 1;
        if self.bits_seen < ASM_BITS as u64 {
            return None;
        }
        if (self.window ^ ASM).count_ones() <= SYNC_THRESHOLD {
            Some(self.bits_seen)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push the 32 bits of `pattern` (MSB first) through the
    /// correlator, returning the first hit position (or None).
    fn push_pattern(s: &mut SyncCorrelator, pattern: u32) -> Option<u64> {
        let mut hit = None;
        for i in 0..ASM_BITS {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "shift index 0..ASM_BITS=32 is well-defined for u32"
            )]
            let bit = ((pattern >> (ASM_BITS - 1 - i)) & 1) as u8;
            if let Some(pos) = s.push(bit) {
                hit = Some(pos);
            }
        }
        hit
    }

    #[test]
    fn detects_clean_asm() {
        let mut s = SyncCorrelator::new();
        // Pre-fill with 1-bits so the first 32-bit window is
        // distinct from the ASM.
        for _ in 0..50 {
            s.push(1);
        }
        let hit = push_pattern(&mut s, ASM);
        assert!(hit.is_some(), "should detect clean ASM");
    }

    #[test]
    fn tolerates_threshold_bit_errors() {
        let mut s = SyncCorrelator::new();
        for _ in 0..50 {
            s.push(1);
        }
        // Flip exactly SYNC_THRESHOLD distinct bits in the ASM.
        let mut corrupted = ASM;
        for i in 0..SYNC_THRESHOLD as usize {
            corrupted ^= 1 << (i * 5);
        }
        let hit = push_pattern(&mut s, corrupted);
        assert!(
            hit.is_some(),
            "should tolerate {SYNC_THRESHOLD} bit errors in ASM",
        );
    }

    #[test]
    fn rejects_too_many_errors() {
        let mut s = SyncCorrelator::new();
        for _ in 0..50 {
            s.push(1);
        }
        // Flip SYNC_THRESHOLD+1 distinct bits — over the limit.
        let mut corrupted = ASM;
        for i in 0..=SYNC_THRESHOLD as usize {
            corrupted ^= 1 << (i * 5);
        }
        let hit = push_pattern(&mut s, corrupted);
        assert!(
            hit.is_none(),
            "should reject ASM with {} bit errors",
            SYNC_THRESHOLD + 1,
        );
    }

    #[test]
    fn emits_no_hits_during_first_window() {
        let mut s = SyncCorrelator::new();
        // Even if the first ASM_BITS input bits happen to spell
        // out ASM, the bits_seen guard prevents an emission until
        // the full window has been observed.
        for i in 0..ASM_BITS {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "shift index 0..ASM_BITS=32 is well-defined for u32"
            )]
            let bit = ((ASM >> (ASM_BITS - 1 - i)) & 1) as u8;
            // First (ASM_BITS - 1) pushes must return None; the
            // last push completes the window and may emit.
            let result = s.push(bit);
            if i < ASM_BITS - 1 {
                assert!(result.is_none(), "premature emission at bit {i}");
            }
        }
    }
}
