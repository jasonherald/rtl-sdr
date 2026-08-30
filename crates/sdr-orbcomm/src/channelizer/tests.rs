use super::*;
use crate::demod::modulate_sdpsk_at_sps;
use crate::packet::{PacketType, fletcher16_check_bytes};
use crate::{ORBCOMM_CHANNELS_HZ, SYMBOL_RATE_HZ};

/// Wideband source rate used by the synthesis tests.
const SOURCE_RATE_HZ: f64 = 2_400_000.0;
/// Tune centre: midway between the two channels under test.
const CENTER_HZ: f64 = 137_512_500.0;
/// Test channel at `CENTER_HZ + 100 kHz`.
const CHANNEL_A_HZ: f64 = 137_612_500.0;
/// Test channel at `CENTER_HZ − 150 kHz`.
const CHANNEL_B_HZ: f64 = 137_362_500.0;
/// Samples per symbol at the source rate: 2 400 000 / 4800.
const TX_SPS: usize = 500;
/// Amplitude scaling for the unit-energy RRC taps at [`TX_SPS`], so the
/// synthesised waveform peaks near 1.0 regardless of the oversampling.
const TX_GAIN: f32 = 22.360_68; // sqrt(500)
/// Sync-packet repeats in a synthesised burst. 25 × 96 bits ≈ 0.5 s of
/// air time: enough for the resampler, the FLL and the timing loop to
/// settle and still leave a dozen packets for the deframer.
const PACKET_REPEATS: usize = 25;
/// Block size the bank is fed with in the synthesis tests.
const FEED_BLOCK: usize = 65_536;

/// A checksum-valid 12-byte Sync packet carrying `sat_id`.
fn sync_packet(sat_id: u8) -> Vec<u8> {
    let mut p = vec![
        PacketType::Sync.header_byte(),
        0xAA,
        0xBB,
        sat_id,
        0x01,
        0x02,
        0x03,
        0x04,
        0x05,
        0x06,
    ];
    let (c0, c1) = fletcher16_check_bytes(&p);
    p.push(c0);
    p.push(c1);
    p
}

/// A checksum-valid 12-byte Message fragment. `seq` is zero-based, and
/// byte 1 is `total` in the high nibble — see
/// [`crate::reassembly::msg_total_len`].
fn message_fragment(seq: u8, total: u8, fill: u8) -> Vec<u8> {
    let mut p = vec![
        PacketType::Message.header_byte(),
        (total << 4) | (seq & 0x0F),
    ];
    p.extend_from_slice(&[fill; 8]);
    let (c0, c1) = fletcher16_check_bytes(&p);
    p.push(c0);
    p.push(c1);
    p
}

/// `repeats` back-to-back copies of `bytes` as wire-order bits (LSB first
/// within each byte — the convention the deframer assembles bytes with).
fn repeated_bits(bytes: &[u8], repeats: usize) -> Vec<bool> {
    let mut bits = Vec::with_capacity(repeats * bytes.len() * 8);
    for _ in 0..repeats {
        for b in bytes {
            for k in 0..8 {
                bits.push((b >> k) & 1 == 1);
            }
        }
    }
    bits
}

/// Modulate `bits` at the source rate, shift to `offset_hz` and sum into
/// `dst` — the exact inverse of the channelizer's own mix-and-decimate.
fn transmit_into(dst: &mut Vec<Complex>, bits: &[bool], offset_hz: f64) {
    let base = modulate_sdpsk_at_sps(bits, TX_SPS);
    if dst.len() < base.len() {
        dst.resize(base.len(), Complex::default());
    }
    let step = std::f64::consts::TAU * offset_hz / SOURCE_RATE_HZ;
    let mut phase = 0.0_f64;
    for (slot, &s) in dst.iter_mut().zip(base.iter()) {
        let (sin, cos) = phase.sin_cos();
        *slot += s * Complex::new(cos as f32, sin as f32) * TX_GAIN;
        phase = wrap_phase(phase + step);
    }
}

/// Feed `iq` to `bank` in [`FEED_BLOCK`]-sample blocks.
fn run(bank: &mut ChannelBank, iq: &[Complex]) -> Vec<OrbcommEvent> {
    let mut events = Vec::new();
    for block in iq.chunks(FEED_BLOCK) {
        bank.process(block, &mut events);
    }
    events
}

/// Sync `sat_id`s reported on `channel_hz`.
fn sat_ids_on(events: &[OrbcommEvent], channel_hz: f64) -> Vec<u8> {
    events
        .iter()
        .filter(|e| e.channel_hz.to_bits() == channel_hz.to_bits())
        .filter_map(|e| match &e.kind {
            OrbcommEventKind::Packet {
                packet: OrbcommPacket::Sync { sat_id, .. },
                ..
            } => Some(*sat_id),
            _ => None,
        })
        .collect()
}

#[test]
fn two_channels_decode_independently() {
    let bits_a = repeated_bits(&sync_packet(0x2C), PACKET_REPEATS);
    let bits_b = repeated_bits(&sync_packet(0x51), PACKET_REPEATS);
    let mut iq = Vec::new();
    transmit_into(&mut iq, &bits_a, CHANNEL_A_HZ - CENTER_HZ);
    transmit_into(&mut iq, &bits_b, CHANNEL_B_HZ - CENTER_HZ);

    let mut bank = ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ, CHANNEL_B_HZ])
        .expect("both channels are inside a 2.4 Msps span");
    let events = run(&mut bank, &iq);

    let a = sat_ids_on(&events, CHANNEL_A_HZ);
    let b = sat_ids_on(&events, CHANNEL_B_HZ);
    assert!(a.len() >= 4, "channel A produced {} sync packets", a.len());
    assert!(b.len() >= 4, "channel B produced {} sync packets", b.len());
    assert!(a.iter().all(|&id| id == 0x2C), "channel A leaked: {a:?}");
    assert!(b.iter().all(|&id| id == 0x51), "channel B leaked: {b:?}");

    let stats = bank.stats();
    assert_eq!(stats.len(), 2);
    assert!(stats.iter().all(|s| s.in_span));
    assert!(stats[0].packets_ok >= 4 && stats[1].packets_ok >= 4);
    // On a clean link nothing is rejected and nothing needs repairing —
    // both stats are wired to the deframer, not to a spurious source.
    assert!(
        stats
            .iter()
            .all(|s| s.checksum_fail == 0 && s.repaired == 0)
    );
}

#[test]
fn multi_packet_message_reassembles_end_to_end() {
    // Two-fragment sequences over the air: only checksum-valid Message
    // packets reach the reassembler, and its completions surface as their
    // own event kind alongside the packet events that produced them.
    let mut wire = message_fragment(0, 2, 0xA1);
    wire.extend(message_fragment(1, 2, 0xB2));
    let bits = repeated_bits(&wire, 13);
    let mut iq = Vec::new();
    transmit_into(&mut iq, &bits, CHANNEL_A_HZ - CENTER_HZ);

    let mut bank =
        ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("channel in span");
    let events = run(&mut bank, &iq);

    let mut expected = vec![0xA1_u8; 8];
    expected.extend_from_slice(&[0xB2; 8]);
    let completed: Vec<&OrbcommEvent> = events
        .iter()
        .filter(|e| matches!(e.kind, OrbcommEventKind::MessageComplete { .. }))
        .collect();
    assert!(completed.len() >= 3, "got {} messages", completed.len());
    for event in completed {
        assert_eq!(event.channel_hz.to_bits(), CHANNEL_A_HZ.to_bits());
        let OrbcommEventKind::MessageComplete { bytes, partial } = &event.kind else {
            unreachable!("filtered above")
        };
        assert!(!partial, "fragment lost on a clean link");
        assert_eq!(bytes, &expected);
    }
}

#[test]
fn doppler_shifted_channel_still_decodes() {
    // +3 kHz on top of the channel offset — worst-case 137 MHz Doppler,
    // nearly 4× the demodulator's ±800 Hz residual contract. Only the FLL
    // can bring this inside it.
    const DOPPLER_HZ: f64 = 3000.0;
    let bits = repeated_bits(&sync_packet(0x7A), PACKET_REPEATS);
    let mut iq = Vec::new();
    transmit_into(&mut iq, &bits, CHANNEL_A_HZ - CENTER_HZ + DOPPLER_HZ);

    let mut bank =
        ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("channel in span");
    let events = run(&mut bank, &iq);

    let ids = sat_ids_on(&events, CHANNEL_A_HZ);
    assert!(ids.len() >= 4, "only {} packets under Doppler", ids.len());
    assert!(ids.iter().all(|&id| id == 0x7A), "got {ids:?}");
}

#[test]
fn adjacent_channels_at_real_spacing_do_not_leak() {
    // The real Orbcomm grid puts 137.440 and 137.460 MHz just 20 kHz apart,
    // barely wider than the 19.2 kHz each channel's decimation keeps. This
    // is the module doc's "a neighbour at the real spacing lands in the
    // Nuttall stopband" claim, and it is what a real capture will stress.
    const LOW_HZ: f64 = 137_440_000.0;
    const HIGH_HZ: f64 = 137_460_000.0;
    const PAIR_CENTER_HZ: f64 = 137_450_000.0;

    let bits_low = repeated_bits(&sync_packet(0x11), PACKET_REPEATS);
    let bits_high = repeated_bits(&sync_packet(0x22), PACKET_REPEATS);
    let mut iq = Vec::new();
    transmit_into(&mut iq, &bits_low, LOW_HZ - PAIR_CENTER_HZ);
    transmit_into(&mut iq, &bits_high, HIGH_HZ - PAIR_CENTER_HZ);

    let mut bank = ChannelBank::new(SOURCE_RATE_HZ, PAIR_CENTER_HZ, &[LOW_HZ, HIGH_HZ])
        .expect("both channels in span");
    let events = run(&mut bank, &iq);

    let low = sat_ids_on(&events, LOW_HZ);
    let high = sat_ids_on(&events, HIGH_HZ);
    assert!(low.len() >= 4, "137.440 produced {} packets", low.len());
    assert!(high.len() >= 4, "137.460 produced {} packets", high.len());
    assert!(
        low.iter().all(|&id| id == 0x11),
        "137.460 leaked into 137.440: {low:?}"
    );
    assert!(
        high.iter().all(|&id| id == 0x22),
        "137.440 leaked into 137.460: {high:?}"
    );
}

#[test]
fn repaired_packets_are_a_subset_of_parsed_packets() {
    // One flipped bit inside an otherwise clean burst: the deframer is
    // already locked when that stride arrives, repairs it, and emits the
    // original packet with `repaired: true`. Both counters must move
    // together — `repaired` is documented as a subset of `packets_ok`.
    const CORRUPTED_PACKET: usize = 10;
    let packet = sync_packet(0x2C);
    let mut bits = repeated_bits(&packet, PACKET_REPEATS);
    let flip = CORRUPTED_PACKET * packet.len() * 8 + 40;
    bits[flip] = !bits[flip];

    let mut iq = Vec::new();
    transmit_into(&mut iq, &bits, CHANNEL_A_HZ - CENTER_HZ);
    let mut bank =
        ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("channel in span");
    let events = run(&mut bank, &iq);

    let stats = bank.stats();
    assert_eq!(stats.len(), 1);
    assert!(stats[0].repaired >= 1, "the flipped bit was never repaired");
    assert!(
        stats[0].repaired <= stats[0].packets_ok,
        "repaired {} exceeds packets_ok {}",
        stats[0].repaired,
        stats[0].packets_ok
    );

    // Every repaired event still carries the *original* packet.
    let repaired: Vec<&OrbcommPacket> = events
        .iter()
        .filter_map(|e| match &e.kind {
            OrbcommEventKind::Packet {
                packet,
                repaired: true,
            } => Some(packet),
            _ => None,
        })
        .collect();
    assert_eq!(repaired.len() as u64, stats[0].repaired);
    for packet in repaired {
        assert!(
            matches!(packet, OrbcommPacket::Sync { sat_id: 0x2C, .. }),
            "repair produced {packet:?}"
        );
    }
}

// --- Source-rate coverage (final review, C1) ---------------------------

/// Airspy R2's low native rate. `2_500_000 / 4800 = 520.83` samples per
/// symbol is not an integer, so [`modulate_sdpsk_at_sps`] cannot synthesise
/// at it directly — see [`transmit_at_airspy_rate`].
const AIRSPY_RATE_HZ: f64 = 2_500_000.0;

#[test]
fn bank_constructs_at_airspy_rates() {
    // 2.5 and 10 Msps are the Airspy R2's native rates
    // (`sdr-source-airspy::DEFAULT_SAMPLE_RATES`); 5 Msps is the Mini's
    // middle step. All three used to blow up inside `RationalResampler`
    // with a 1 484 375-tap prototype, because its power-of-two
    // pre-decimation leaves a fractional 19 531.25 Hz intermediate rate
    // that shares no factor with 19 200. The three RTL-SDR rates below
    // them must stay green — they take the unchanged direct chain.
    for rate_hz in [
        2_500_000.0_f64,
        5_000_000.0,
        10_000_000.0,
        2_400_000.0,
        3_200_000.0,
        250_000.0,
    ] {
        let bank = ChannelBank::new(rate_hz, CENTER_HZ, &ORBCOMM_CHANNELS_HZ);
        assert!(
            bank.is_ok(),
            "ChannelBank::new failed at {rate_hz} Hz: {:?}",
            bank.err()
        );
    }
}

#[test]
fn predecimation_engages_only_where_the_direct_chain_fails() {
    // Rates that already worked must keep the exact chain they had —
    // notably 1.2288 Msps, the reference captures' rate, where the direct
    // plan reduces to a pure power-of-two decimation with no polyphase
    // stage at all.
    for rate_hz in [
        250_000.0_f64,
        1_228_800.0,
        2_400_000.0,
        3_200_000.0,
        SOURCE_RATE_HZ,
    ] {
        let plan = plan_resampling(rate_hz).expect("direct chain constructs");
        assert!(
            plan.predecim.is_none(),
            "{rate_hz} Hz gained a pre-decimation stage it did not need"
        );
        assert!((plan.resampler_in_rate_hz - rate_hz).abs() < f64::EPSILON);
    }

    // The three broken rates all settle on the same 100 kHz intermediate:
    // the largest D with `rate / D >= MIN_INTERMEDIATE_RATE_HZ` for which
    // both stages construct.
    for rate_hz in [AIRSPY_RATE_HZ, 5_000_000.0, 10_000_000.0] {
        let plan = plan_resampling(rate_hz).expect("pre-decimated chain constructs");
        let Some(pre) = plan.predecim.as_ref() else {
            unreachable!("{rate_hz} Hz must gain a pre-decimation stage")
        };
        assert!(
            (plan.resampler_in_rate_hz - 100_000.0).abs() < f64::EPSILON,
            "{rate_hz} Hz picked a {} Hz intermediate",
            plan.resampler_in_rate_hz
        );
        assert!(plan.resampler_in_rate_hz >= MIN_INTERMEDIATE_RATE_HZ);
        // `out_per_in` is the stage's own ratio, and the two stages'
        // ratios must multiply back to the end-to-end 19.2 kHz / source.
        let end_to_end = pre.out_per_in * (CHANNEL_SAMPLE_RATE_HZ / plan.resampler_in_rate_hz);
        assert!((end_to_end - CHANNEL_SAMPLE_RATE_HZ / rate_hz).abs() < 1e-12);
    }
}

/// Synthesise `bits` at [`AIRSPY_RATE_HZ`] and shift to `offset_hz`.
///
/// The modulator only takes an integer samples-per-symbol, and 2.5 Msps
/// is 520.83 samples per 4800 baud symbol. So the waveform is built at
/// [`SOURCE_RATE_HZ`] (an exact 500 sps) and resampled 24:25 — an exact
/// integer ratio, so no timing error is introduced — before the channel
/// offset is applied at the destination rate.
fn transmit_at_airspy_rate(bits: &[bool], offset_hz: f64) -> Vec<Complex> {
    let base = modulate_sdpsk_at_sps(bits, TX_SPS);
    let mut up = RationalResampler::new(SOURCE_RATE_HZ, AIRSPY_RATE_HZ)
        .expect("2.4 -> 2.5 Msps is a 24:25 upsample");
    let mut out = vec![Complex::default(); base.len() * 2 + RESAMPLER_OUTPUT_MARGIN];
    let count = up.process(&base, &mut out).expect("output buffer is 2x");
    out.truncate(count);

    let step = std::f64::consts::TAU * offset_hz / AIRSPY_RATE_HZ;
    let mut phase = 0.0_f64;
    for slot in &mut out {
        let (sin, cos) = phase.sin_cos();
        *slot = *slot * Complex::new(cos as f32, sin as f32) * TX_GAIN;
        phase = wrap_phase(phase + step);
    }
    out
}

#[test]
fn decodes_at_airspy_rate_through_predecimation() {
    // End-to-end proof that the pre-decimated chain is not merely
    // constructible: a real burst at the Airspy's 2.5 Msps has to come
    // out the far side as checksum-valid Sync packets carrying the right
    // satellite id.
    let bits = repeated_bits(&sync_packet(0x2C), PACKET_REPEATS);
    let iq = transmit_at_airspy_rate(&bits, CHANNEL_A_HZ - CENTER_HZ);

    let mut bank = ChannelBank::new(AIRSPY_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ])
        .expect("channel in span at 2.5 Msps");
    let events = run(&mut bank, &iq);

    let ids = sat_ids_on(&events, CHANNEL_A_HZ);
    assert!(
        ids.len() >= 4,
        "only {} packets decoded through the pre-decimation path at {AIRSPY_RATE_HZ} Hz",
        ids.len()
    );
    assert!(ids.iter().all(|&id| id == 0x2C), "got {ids:?}");
    let stats = bank.stats();
    assert_eq!(stats[0].packets_ok as usize, ids.len());
}

#[test]
fn out_of_span_channel_flagged() {
    // 240 kHz of span around 137.5 MHz reaches ±120 kHz, so only the
    // 137.44 / 137.46 MHz pair fits (with their ±9.6 kHz of bandwidth).
    let bank = ChannelBank::new(240_000.0, 137_500_000.0, &ORBCOMM_CHANNELS_HZ)
        .expect("two channels are in span");
    let stats = bank.stats();
    assert_eq!(stats.len(), ORBCOMM_CHANNELS_HZ.len());
    for (s, &f) in stats.iter().zip(ORBCOMM_CHANNELS_HZ.iter()) {
        assert_eq!(s.freq_hz.to_bits(), f.to_bits());
        let expect = (f - 137_500_000.0).abs() + CHANNEL_HALF_BANDWIDTH_HZ <= 120_000.0;
        assert_eq!(s.in_span, expect, "channel {f} in_span");
    }
    assert_eq!(stats.iter().filter(|s| s.in_span).count(), 2);
}

#[test]
fn out_of_span_channels_ignore_input() {
    let mut bank = ChannelBank::new(240_000.0, 137_500_000.0, &ORBCOMM_CHANNELS_HZ)
        .expect("two channels are in span");
    let iq = vec![Complex::new(0.5, -0.25); 4096];
    let mut events = Vec::new();
    bank.process(&iq, &mut events);
    for (s, &f) in bank.stats().iter().zip(ORBCOMM_CHANNELS_HZ.iter()) {
        if !s.in_span {
            assert_eq!(s.packets_ok, 0, "channel {f} decoded while out of span");
            assert_eq!(s.checksum_fail, 0);
            assert_eq!(s.repaired, 0);
        }
    }
    // Anything emitted must carry an *in-span* channel's frequency.
    // (Asserting membership of ORBCOMM_CHANNELS_HZ would be vacuous —
    // every event's `channel_hz` is copied from the requested list.)
    let in_span: Vec<f64> = bank
        .stats()
        .iter()
        .filter(|s| s.in_span)
        .map(|s| s.freq_hz)
        .collect();
    assert_eq!(in_span.len(), 2);
    for event in &events {
        assert!(
            in_span
                .iter()
                .any(|f| f.to_bits() == event.channel_hz.to_bits()),
            "event from out-of-span channel {}",
            event.channel_hz
        );
    }
}

#[test]
fn no_channels_in_span_errors() {
    // Tuned 37 MHz away: nothing fits.
    let err = ChannelBank::new(240_000.0, 100_000_000.0, &ORBCOMM_CHANNELS_HZ);
    assert!(matches!(err, Err(OrbcommError::NoChannelsInSpan { .. })));
    // An empty request has nothing in span either.
    assert!(matches!(
        ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[]),
        Err(OrbcommError::NoChannelsInSpan { .. })
    ));
    // A degenerate source rate must not be treated as infinite span.
    assert!(matches!(
        ChannelBank::new(f64::NAN, CENTER_HZ, &ORBCOMM_CHANNELS_HZ),
        Err(OrbcommError::NoChannelsInSpan { .. })
    ));
}

#[test]
fn block_fragmentation_does_not_change_the_output() {
    // Every stage carries state across calls — NCO phase, resampler delay
    // lines, the FLL's accumulator and block counter, the demodulator and
    // the deframer — so ragged blocks must be bit-for-bit invisible.
    let bits = repeated_bits(&sync_packet(0x2C), 12);
    let mut iq = Vec::new();
    transmit_into(&mut iq, &bits, CHANNEL_A_HZ - CENTER_HZ);

    let mut whole = ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("in span");
    let mut whole_events = Vec::new();
    whole.process(&iq, &mut whole_events);

    let mut ragged = ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[CHANNEL_A_HZ]).expect("in span");
    let mut ragged_events = Vec::new();
    let mut start = 0;
    let mut size = 1;
    while start < iq.len() {
        let end = (start + size).min(iq.len());
        ragged.process(&iq[start..end], &mut ragged_events);
        start = end;
        size = size % 4099 + 1;
    }

    assert_eq!(whole_events, ragged_events);
    assert!(!whole_events.is_empty(), "the harness decoded nothing");
}

#[test]
fn sample_rate_ratio_matches_the_test_modulator() {
    assert!((SOURCE_RATE_HZ / SYMBOL_RATE_HZ - TX_SPS as f64).abs() < f64::EPSILON);
    assert!((TX_GAIN - (TX_SPS as f32).sqrt()).abs() < 1e-4);
}

// --- FLL ---------------------------------------------------------------

/// Deterministic xorshift64* PRNG — tests must never be flaky.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `(0, 1)`.
    fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / 9_007_199_254_740_992.0
    }
    /// Standard normal via Box–Muller.
    fn next_normal(&mut self) -> f64 {
        let u1 = self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Add complex AWGN at the requested per-sample SNR, the same convention
/// `demod.rs`'s loopback tests use (in-band, at the channel rate — so
/// 10 dB here reads as roughly 16 dB of Es/N0 at 4 samples/symbol).
fn add_awgn(samples: &mut [Complex], snr_db: f64, seed: u64) {
    if samples.is_empty() {
        return;
    }
    let signal_power = samples
        .iter()
        .map(|s| f64::from(s.re) * f64::from(s.re) + f64::from(s.im) * f64::from(s.im))
        .sum::<f64>()
        / samples.len() as f64;
    let sigma = (signal_power / 10.0_f64.powf(snr_db / 10.0) / 2.0).sqrt();
    let mut rng = Rng::new(seed);
    for s in samples.iter_mut() {
        *s = Complex::new(
            s.re + (sigma * rng.next_normal()) as f32,
            s.im + (sigma * rng.next_normal()) as f32,
        );
    }
}

fn apply_cfo(samples: &mut [Complex], cfo_hz: f64) {
    let step = std::f64::consts::TAU * cfo_hz / CHANNEL_SAMPLE_RATE_HZ;
    let mut phase = 0.0_f64;
    for s in samples.iter_mut() {
        let (sin, cos) = phase.sin_cos();
        *s = *s * Complex::new(cos as f32, sin as f32);
        phase = wrap_phase(phase + step);
    }
}

#[test]
fn fll_pulls_in_worst_case_doppler() {
    // The demodulator's contract is ±800 Hz; Doppler at 137 MHz reaches
    // ±3.5 kHz. Both signs, and the residual is measured on the corrected
    // stream, not inferred from the loop state.
    for cfo_hz in [-3500.0_f64, -3000.0, 3000.0, 3500.0] {
        let mut rng = Rng::new(0x5EED_0100);
        let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
        let mut iq = modulate_sdpsk_at_sps(&bits, 4);
        apply_cfo(&mut iq, cfo_hz);

        let mut fll = Fll::new();
        fll.process(&mut iq);
        let residual = cfo_hz - fll.freq_hz;
        assert!(
            residual.abs() < 800.0,
            "cfo {cfo_hz}: residual {residual} Hz breaks the demod contract"
        );
    }
}

#[test]
fn fll_pull_in_time_is_bounded() {
    // The pull-in transient costs bits, so bound it: from a 3.5 kHz cold
    // start the loop must be inside the ±800 Hz contract within 2048
    // channel samples (512 symbols, ~107 ms).
    const BUDGET: usize = 2048;
    let mut rng = Rng::new(0x5EED_0101);
    let bits: Vec<bool> = (0..1024).map(|_| rng.next_u64() & 1 == 1).collect();
    let mut iq = modulate_sdpsk_at_sps(&bits, 4);
    apply_cfo(&mut iq, 3500.0);
    assert!(iq.len() > BUDGET);

    let mut fll = Fll::new();
    fll.process(&mut iq[..BUDGET]);
    assert!(
        (3500.0 - fll.freq_hz).abs() < 800.0,
        "after {BUDGET} samples the estimate is still {} Hz",
        fll.freq_hz
    );
}

#[test]
fn fll_stays_put_with_no_offset() {
    // Data-dependent jitter (±300 Hz block to block) must not accumulate
    // into a walk-off on a signal that has no offset to correct.
    let mut rng = Rng::new(0x5EED_0102);
    let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
    let mut iq = modulate_sdpsk_at_sps(&bits, 4);
    let mut fll = Fll::new();
    fll.process(&mut iq);
    assert!(
        fll.freq_hz.abs() < 400.0,
        "loop drifted to {} Hz on a clean signal",
        fll.freq_hz
    );
}

#[test]
fn fll_pulls_in_at_10_db_snr() {
    // The discriminator runs on the decimated stream *before* the
    // demodulator's matched filter, so it sees in-band noise out to
    // ±9.6 kHz rather than the RRC's ±3.36 kHz. Bound that cost at the
    // same 10 dB per-sample SNR `demod.rs` uses for its noise-margin test:
    // pull-in from ±3 kHz must still land inside the ±800 Hz contract.
    //
    // This is a regression guard, not the cliff. Sweeping the SNR down,
    // the loop still pulls in at −10 dB and only breaks near −14 dB — the
    // 256-sample coherent average buys ~24 dB, so the demodulator (already
    // at 5 % BER by 10 dB) fails long before the FLL does. The wider
    // measurement bandwidth costs far less than it looked like it might.
    for (seed, cfo_hz) in [(0x5EED_0110_u64, -3000.0_f64), (0x5EED_0111, 3000.0)] {
        let mut rng = Rng::new(seed);
        let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
        let mut iq = modulate_sdpsk_at_sps(&bits, 4);
        apply_cfo(&mut iq, cfo_hz);
        add_awgn(&mut iq, 10.0, seed);

        let mut fll = Fll::new();
        fll.process(&mut iq);
        let residual = cfo_hz - fll.freq_hz;
        assert!(
            residual.abs() < 800.0,
            "cfo {cfo_hz} at 10 dB SNR: residual {residual} Hz"
        );
    }
}

#[test]
fn fll_survives_non_finite_samples() {
    // A poisoned prefix must neither park the loop on a NaN nor stop it
    // tracking: the poisoned blocks are discarded whole, then the clean
    // tail behind them has to pull in normally.
    const POISON: usize = 1024;
    let mut poisoned = vec![Complex::new(f32::NAN, f32::NAN); POISON];
    poisoned[500] = Complex::new(f32::INFINITY, 1.0);
    poisoned[700] = Complex::new(1.0, f32::NEG_INFINITY);

    let mut fll = Fll::new();
    fll.process(&mut poisoned);
    assert!(fll.freq_hz.is_finite(), "freq went to {}", fll.freq_hz);
    assert!(fll.phase.is_finite());

    let mut rng = Rng::new(0x5EED_0104);
    let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
    let mut clean = modulate_sdpsk_at_sps(&bits, 4);
    apply_cfo(&mut clean, 3000.0);
    fll.process(&mut clean);
    let residual = 3000.0 - fll.freq_hz;
    assert!(
        residual.abs() < 800.0,
        "loop did not resume tracking after the poison: residual {residual} Hz"
    );
    assert!(clean.iter().all(|s| s.re.is_finite() && s.im.is_finite()));
}

#[test]
fn fll_block_boundaries_are_invisible() {
    let mut rng = Rng::new(0x5EED_0103);
    let bits: Vec<bool> = (0..2048).map(|_| rng.next_u64() & 1 == 1).collect();
    let mut iq = modulate_sdpsk_at_sps(&bits, 4);
    apply_cfo(&mut iq, 2000.0);

    let mut whole = iq.clone();
    let mut a = Fll::new();
    a.process(&mut whole);

    let mut ragged = iq;
    let mut b = Fll::new();
    let mut start = 0;
    let mut size = 1;
    while start < ragged.len() {
        let end = (start + size).min(ragged.len());
        b.process(&mut ragged[start..end]);
        start = end;
        size = size % 97 + 1;
    }
    assert_eq!(whole, ragged);
    assert!((a.freq_hz - b.freq_hz).abs() < f64::EPSILON);
}
