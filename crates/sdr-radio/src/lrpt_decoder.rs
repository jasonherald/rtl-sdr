//! Meteor-M LRPT receive driver — bridges the post-VFO IQ buffer
//! to the QPSK demod + FEC chain + image assembler, and pushes
//! newly-decoded scan lines into the shared [`LrptImage`] handle
//! the live viewer reads from.
//!
//! Mirrors the role of `sdr_core::controller::apt_decode_tap` for
//! LRPT — except APT runs on demodulated audio (post-FM, mono
//! downmix) while LRPT runs on the IQ that would have been fed
//! to the FM demod. The controller's
//! [`DemodMode::Lrpt`](sdr_types::DemodMode::Lrpt) branch reads
//! `radio_input` (IQ at 144 ksps thanks to the VFO + LRPT demod's
//! IF rate) and drives this decoder before
//! `RadioModule::process` runs (the LRPT mode's "demod" is a
//! silent passthrough).
//!
//! Lifecycle:
//!
//! - **Construction** — Caller passes the shared [`LrptImage`].
//!   `LrptDecoder::new` sets up the demod + pipeline + the
//!   per-APID line-watermark map.
//! - **Per chunk** — [`Self::process`] streams IQ through
//!   `LrptDemod::process` → `LrptPipeline::push_symbol`. After
//!   consuming the chunk, walks each channel in the pipeline's
//!   assembler and pushes any newly-appended scan lines to the
//!   shared [`LrptImage`]. The `last_pushed_lines: APID → count`
//!   map tracks what's already been forwarded so the same line
//!   never gets pushed twice.
//! - **Between passes** — Caller drops + reconstructs (or calls
//!   [`Self::reset`]) so the pipeline's internal state and the
//!   line-watermark map all flush cleanly. Same idiom the
//!   APT decoder uses.

use std::collections::HashMap;

use sdr_dsp::lrpt::{LrptDemod, LrptMode};
use sdr_lrpt::LrptPipeline;
use sdr_lrpt::image::{IMAGE_WIDTH, MCU_SIDE};
use sdr_types::{Complex, DspError};

use crate::lrpt_image::LrptImage;

/// Driver that ties IQ in to per-channel scan lines out.
/// Seconds of IQ without a channel growing before its held-back row
/// group is released anyway. A group normally completes within ~1 s
/// (14 packets); after the signal fades nothing will finish it, and the
/// LOS snapshot is taken before any reset (#725).
const STALE_GROUP_FLUSH_SECONDS: u64 = 2;

/// Per-satellite LRPT downlink profile: the modulation the demod
/// runs and whether the FEC chain applies differential pre-decoding
/// (#730). Built by the wiring layer from `KnownSatellite`'s
/// `lrpt_modulation` / `lrpt_differential` catalog fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LrptDownlink {
    /// QPSK (legacy METEOR-M N2) or OQPSK (METEOR-M2 3 / M2-4).
    pub mode: LrptMode,
    /// Differential precoding (legacy METEOR-M N2 only).
    pub differential: bool,
}

impl LrptDownlink {
    /// Profile from its two dimensions.
    #[must_use]
    pub const fn new(mode: LrptMode, differential: bool) -> Self {
        Self { mode, differential }
    }
}

pub struct LrptDecoder {
    demod: LrptDemod,
    pipeline: LrptPipeline,
    image: LrptImage,
    /// Downlink profile the decoder was built for. Stored so
    /// [`Self::reset`] can re-construct the inner chains the same
    /// way without the caller having to plumb it through again.
    downlink: LrptDownlink,
    /// Per-APID watermark — the last `channel.lines` count we
    /// pushed into the shared `LrptImage`. Indexed by APID
    /// (16-bit) since that's the identifier the demux stamps
    /// onto each [`sdr_lrpt::ccsds::ImagePacket`]. New lines
    /// (`channel.lines > watermark`) get pushed and the
    /// watermark advances.
    last_pushed_lines: HashMap<u16, usize>,
    /// Per-APID line count at the last growth check.
    seen_lines: HashMap<u16, usize>,
    /// Per-APID IQ samples consumed since that channel last grew.
    samples_since_growth: HashMap<u16, u64>,
}

impl LrptDecoder {
    /// Build a fresh decoder around the given shared image and
    /// downlink profile. METEOR-M N2 is QPSK with differential
    /// precoding; METEOR-M2 3 / METEOR-M2 4 are OQPSK without — the
    /// mode picks the inner demod chain (per
    /// [`LrptDemod::new_with_mode`]) and the precoding flag picks
    /// the FEC chain (per [`LrptPipeline::new_with_differential`]).
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the underlying
    /// [`LrptDemod`] constructor rejects its parameters
    /// (practically unreachable — see that constructor's docs).
    pub fn new(image: LrptImage, downlink: LrptDownlink) -> Result<Self, DspError> {
        Ok(Self {
            demod: LrptDemod::new_with_mode(downlink.mode)?,
            pipeline: LrptPipeline::new_with_differential(downlink.differential),
            image,
            downlink,
            last_pushed_lines: HashMap::new(),
            seen_lines: HashMap::new(),
            samples_since_growth: HashMap::new(),
        })
    }

    /// Stream one chunk of post-VFO IQ samples through the
    /// chain. Each input sample feeds the QPSK demod; emitted
    /// soft-symbol pairs feed the FEC chain (Viterbi → ASM
    /// sync → derand → RS → demux → JPEG decode → image
    /// assembly). After the loop, harvest any new scan lines
    /// from the pipeline's internal assembler and forward them
    /// to the shared [`LrptImage`] for the live viewer to read.
    ///
    /// `samples` is at the LRPT working sample rate
    /// ([`sdr_dsp::lrpt::SAMPLE_RATE_HZ`] = 144 ksps); the
    /// caller is responsible for the VFO + IF-chain plumbing
    /// that delivers IQ at that rate.
    pub fn process(&mut self, samples: &[Complex]) {
        for &sample in samples {
            if let Some(soft) = self.demod.process(sample) {
                self.pipeline.push_symbol(soft);
            }
        }
        self.note_growth(samples.len() as u64);
        self.harvest_new_lines();
    }

    /// Track, per channel, how much IQ has passed since its line count
    /// last grew — the staleness clock for the held-back row group.
    fn note_growth(&mut self, samples: u64) {
        for (&apid, channel) in self.pipeline.assembler().channels() {
            let seen = self.seen_lines.entry(apid).or_insert(0);
            let since = self.samples_since_growth.entry(apid).or_insert(0);
            if channel.lines > *seen {
                *seen = channel.lines;
                *since = 0;
            } else {
                *since = since.saturating_add(samples);
            }
        }
    }

    /// Walk the pipeline's assembler and push every line that's
    /// new since the last harvest into the shared
    /// [`LrptImage`]. Tracks per-APID watermarks so the same
    /// line is never pushed twice — `push_line` in the shared
    /// image is append-only, so a duplicate would show up as a
    /// repeated row in the rendered viewer.
    fn harvest_new_lines(&mut self) {
        self.harvest(false);
    }

    /// Mutable access to the pipeline's image assembler — for tests
    /// that inject decoded blocks without a full IQ → CADU path.
    pub fn assembler_mut(&mut self) -> &mut sdr_lrpt::image::ImageAssembler {
        self.pipeline.assembler_mut()
    }

    /// Push every line the assembler holds, including the row group
    /// still in progress. For LOS / end of pass, when no further MCUs
    /// will complete it.
    pub fn flush_pending_lines(&mut self) {
        self.harvest(true);
    }

    /// `place_mcu` grows a channel by a whole 8-row group on that
    /// group's *first* MCU, and the other 13 packets fill it over the
    /// next ~1 s. Pushing rows as soon as they exist handed the viewer
    /// (and the LOS export) 8-row bands holding only the MCUs decoded
    /// so far (#725). A group is therefore pushed only once the next
    /// group has started — i.e. the last `MCU_SIDE` rows are held back —
    /// unless `include_in_progress` (an explicit flush) or the channel
    /// has not grown for `STALE_GROUP_FLUSH_SECONDS` (signal gone; the
    /// LOS snapshot must still see the last group).
    fn harvest(&mut self, include_in_progress: bool) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let stale_after = STALE_GROUP_FLUSH_SECONDS * sdr_dsp::lrpt::SAMPLE_RATE_HZ as u64;
        let assembler = self.pipeline.assembler();
        for (&apid, channel) in assembler.channels() {
            let already = self.last_pushed_lines.get(&apid).copied().unwrap_or(0);
            // Signal gone: nothing will finish this group, and the LOS
            // snapshot is taken before any reset — release it.
            let stale = self
                .samples_since_growth
                .get(&apid)
                .is_some_and(|&since| since >= stale_after);
            let complete = if include_in_progress || stale {
                channel.lines
            } else {
                channel.lines.saturating_sub(MCU_SIDE)
            };
            if complete <= already {
                continue;
            }
            // Track lines actually pushed so that if the bounds
            // guard below trips, the watermark doesn't jump past
            // un-pushed rows and permanently drop them. Per
            // CodeRabbit round 1 on PR #543.
            let mut pushed = already;
            for line_idx in already..complete {
                let start = line_idx * IMAGE_WIDTH;
                let end = start + IMAGE_WIDTH;
                // Defensive bounds check — `place_mcu` always
                // grows pixels by full-line increments, so this
                // is structurally guaranteed; the explicit guard
                // protects against a future refactor of the
                // composite buffer that drops the invariant.
                if end > channel.pixels.len() {
                    tracing::warn!(
                        "LrptDecoder: channel {apid} pixel buffer shorter than `lines * IMAGE_WIDTH`; skipping line {line_idx}",
                    );
                    break;
                }
                self.image.push_line(apid, &channel.pixels[start..end]);
                pushed = line_idx + 1;
            }
            self.last_pushed_lines.insert(apid, pushed);
        }
    }

    /// Flush all chain state. Called between passes so the
    /// next pass starts on a clean Viterbi traceback / sync
    /// window / RS path. The shared image is also cleared so
    /// the next pass paints onto a fresh canvas; the watermark
    /// map resets so the harvest loop forwards every line of
    /// the new pass.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the demod re-construction fails
    /// (see [`LrptDemod::new`]).
    /// Downlink profile this decoder was built for.
    #[must_use]
    pub fn downlink(&self) -> LrptDownlink {
        self.downlink
    }

    pub fn reset(&mut self) -> Result<(), DspError> {
        self.demod = LrptDemod::new_with_mode(self.downlink.mode)?;
        self.pipeline.reset();
        self.image.clear();
        self.last_pushed_lines.clear();
        self.seen_lines.clear();
        self.samples_since_growth.clear();
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sdr_lrpt::image::IMAGE_WIDTH;

    /// APID used in the harvest test. AVHRR convention: 64 is
    /// the conventional "channel 0" — same value the rest of
    /// the test suite uses for single-channel cases.
    const APID_TEST: u16 = 64;

    #[test]
    fn lrpt_decoder_constructible() {
        let image = LrptImage::new();
        let _decoder = LrptDecoder::new(image, LrptDownlink::new(LrptMode::Qpsk, false))
            .expect("LrptDemod must construct");
    }

    /// The decoder builds its FEC chain from the downlink profile's
    /// precoding flag, not a hard-coded `false` (#730).
    #[test]
    fn decoder_honours_the_profiles_differential_precoding() {
        let plain = LrptDecoder::new(LrptImage::new(), LrptDownlink::new(LrptMode::Oqpsk, false))
            .expect("construct");
        assert!(!plain.pipeline.is_differential());
        let diff = LrptDecoder::new(LrptImage::new(), LrptDownlink::new(LrptMode::Oqpsk, true))
            .expect("construct");
        assert!(diff.pipeline.is_differential());
        assert_eq!(diff.downlink(), LrptDownlink::new(LrptMode::Oqpsk, true));
    }

    #[test]
    fn process_zero_iq_does_not_crash_or_push_lines() {
        // Zero IQ produces no symbol locks → no VCDUs → no
        // image lines. The decoder should run cleanly and the
        // shared image should stay empty.
        let image = LrptImage::new();
        let mut decoder =
            LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Qpsk, false)).unwrap();
        let zeros = vec![Complex::default(); 1_000];
        decoder.process(&zeros);
        assert!(image.snapshot_channel(APID_TEST).is_none());
    }

    #[test]
    fn harvest_pushes_only_new_lines() {
        // Drive the harvest path directly by hand-feeding the
        // pipeline's assembler via an internal route the test
        // can simulate: place a synthetic line into the
        // pipeline's assembler, run harvest, confirm it lands
        // in the shared image.
        //
        // We can't easily synthesize a CADU here (that's what
        // the FecChain / golden-fixture tests cover), so we
        // exercise the harvest watermark logic in isolation
        // via `pipeline.push_vcdu(empty)` — the existing
        // pipeline tests confirm that path stays a no-op for
        // garbage input. Instead, we validate the watermark
        // ledger directly: an LrptDecoder constructed against
        // an empty pipeline harvests nothing on the first
        // call.
        let image = LrptImage::new();
        let mut decoder =
            LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Qpsk, false)).unwrap();
        // Empty pipeline → empty assembler → harvest is a no-op.
        decoder.harvest_new_lines();
        assert!(decoder.last_pushed_lines.is_empty());
        assert!(image.snapshot_channel(APID_TEST).is_none());
    }

    #[test]
    fn reset_clears_state_and_image() {
        // Pre-populate the shared image with a line so we can
        // verify reset() clears it.
        let image = LrptImage::new();
        image.push_line(APID_TEST, &vec![42_u8; IMAGE_WIDTH]);
        assert!(image.snapshot_channel(APID_TEST).is_some());
        let mut decoder =
            LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Qpsk, false)).unwrap();
        decoder.last_pushed_lines.insert(APID_TEST, 7);
        decoder.reset().expect("reset must succeed");
        assert!(decoder.last_pushed_lines.is_empty());
        // Reset clears the shared image too — between-pass
        // invariant: the next pass paints on a clean canvas.
        assert!(
            image.snapshot_channel(APID_TEST).is_none(),
            "reset must clear the shared image",
        );
    }

    /// #725 — `place_mcu` grows a channel to the full 8-row group on
    /// the group's first MCU, while the remaining 13 packets fill it
    /// over the next ~1 s. Harvesting immediately pushed 8-row bands
    /// holding only the MCUs decoded so far (slivers in the live
    /// viewer and the LOS export). A group is pushed once the next
    /// group has started; `flush_pending_lines` pushes the rest at LOS.
    #[test]
    fn harvest_holds_back_the_in_progress_row_group() {
        let image = LrptImage::new();
        let mut decoder =
            LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Qpsk, false)).unwrap();
        let block = [[200_u8; MCU_SIDE]; MCU_SIDE];

        // First MCU of row group 0: the group is in progress.
        decoder
            .pipeline
            .assembler_mut()
            .place_mcu(APID_TEST, 0, 0, &block);
        decoder.harvest_new_lines();
        assert!(
            image.snapshot_channel(APID_TEST).is_none(),
            "an in-progress group must not reach the viewer"
        );

        // Row group 1 starts: group 0 is complete and is pushed.
        decoder
            .pipeline
            .assembler_mut()
            .place_mcu(APID_TEST, 1, 0, &block);
        decoder.harvest_new_lines();
        let snap = image.snapshot_channel(APID_TEST).expect("group 0 pushed");
        assert_eq!(snap.lines, MCU_SIDE, "exactly one complete group");

        // LOS: the final (possibly partial) group is flushed.
        decoder.flush_pending_lines();
        let snap = image.snapshot_channel(APID_TEST).expect("flushed");
        assert_eq!(snap.lines, 2 * MCU_SIDE);
        // And a further harvest pushes nothing twice.
        decoder.harvest_new_lines();
        assert_eq!(
            image
                .snapshot_channel(APID_TEST)
                .expect("still there")
                .lines,
            2 * MCU_SIDE
        );
    }

    /// CR round 1 on PR #802 — the LOS snapshot is taken before any
    /// reset, so a group that has stopped growing must be released on
    /// its own: after `STALE_GROUP_FLUSH_SECONDS` of samples without
    /// new lines on that channel, the harvest pushes it.
    #[test]
    fn stale_in_progress_group_is_released_after_the_flush_timeout() {
        let image = LrptImage::new();
        let mut decoder =
            LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Qpsk, false)).unwrap();
        let block = [[200_u8; MCU_SIDE]; MCU_SIDE];
        decoder
            .pipeline
            .assembler_mut()
            .place_mcu(APID_TEST, 0, 0, &block);
        decoder.harvest_new_lines();
        assert!(image.snapshot_channel(APID_TEST).is_none());

        // Silence (zero IQ) for just under the timeout: still held.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let one_second = sdr_dsp::lrpt::SAMPLE_RATE_HZ as usize;
        let silence = vec![Complex::default(); one_second];
        for _ in 0..STALE_GROUP_FLUSH_SECONDS - 1 {
            decoder.process(&silence);
        }
        assert!(
            image.snapshot_channel(APID_TEST).is_none(),
            "held until the timeout"
        );
        // Crossing the timeout releases the group.
        decoder.process(&silence);
        decoder.process(&silence);
        let snap = image.snapshot_channel(APID_TEST).expect("released");
        assert_eq!(snap.lines, MCU_SIDE);
    }
}
