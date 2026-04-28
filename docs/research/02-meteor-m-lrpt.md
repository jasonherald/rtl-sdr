# Meteor-M LRPT Reception: Complete Implementation Reference

Decoding Low-Rate Picture Transmission (LRPT) from Russian Meteor-M series weather satellites on 137 MHz using an RTL-SDR. Digital successor to NOAA's analog APT with dramatically better resolution — full-color composite imagery at ~1 km/pixel spatial resolution.

---

## 1. Background

### 1.1 What LRPT Is

LRPT is the digital downlink format used by the Russian Meteor-M weather satellites. Unlike NOAA's analog APT (one audio subcarrier, AM-modulated brightness), LRPT is:

- **QPSK modulated** at 72 ksym/s
- **Rate 1/2 convolutionally coded** (Viterbi-decodable)
- **Reed-Solomon (255, 223) outer coded** for error correction
- **CCSDS frame formatted** (same packet standard used throughout spacecraft data systems)
- **JPEG-compressed image data** inside the CCSDS packets

Resolution: roughly **1 km/pixel**, six spectral channels, three of which are typically transmitted simultaneously — giving you the raw material for genuine RGB color composites.

### 1.2 Active Satellites (as of 2026)

| Satellite | Frequency | Status |
|-----------|-----------|--------|
| Meteor-M 2 | 137.100 MHz | Launched 2014, degraded but still transmitting intermittently |
| Meteor-M 2-2 | 137.900 MHz | Launched 2019, lost 2022 after micrometeoroid impact |
| Meteor-M 2-3 | 137.900 MHz | Launched 2023, operational |
| Meteor-M 2-4 | 137.900 MHz | Launched 2024, operational |

Check current status before planning — the Meteor program has had hardware issues. Satellite operational pages on `space-track.org` and the `r/amateursatellites` subreddit track status in near real-time.

### 1.3 Comparison with APT

| | NOAA APT | Meteor-M LRPT |
|---|---|---|
| Modulation | AM on FM subcarrier | QPSK |
| Data rate | ~4 kbit/s effective | 72 ksym/s → ~30-80 kbit/s data |
| Resolution | ~4 km/pixel | ~1 km/pixel |
| Channels | 2 (side-by-side) | Up to 6, commonly 3 used for RGB |
| Decoder complexity | Low (AM envelope) | High (PLL, Viterbi, RS, CCSDS) |
| Image quality | Grainy, usable | Sharp, broadcast-quality |

LRPT is unambiguously more work but produces images that don't look like they came out of a 1970s fax machine.

---

## 2. RF Setup

### 2.1 Tuning Parameters

| Parameter | Value |
|-----------|-------|
| Center frequency | 137.100 or 137.900 MHz |
| Sample rate | 2.048 MSPS (minimum); higher doesn't hurt |
| Signal bandwidth | ~140 kHz (QPSK at 72 ksym/s with roll-off) |
| Gain | 35–45 dB, experiment by pass |

### 2.2 Doppler

Same as NOAA APT — ±3.5 kHz peak-to-peak at 137 MHz. LRPT is more Doppler-sensitive than APT because the QPSK symbol timing and carrier PLL must track. Two options:

1. **Let the decoder's PLL handle it.** Most LRPT decoders have a costas loop that tracks the carrier; they handle typical LEO Doppler without explicit correction.
2. **External Doppler correction** via `gpredict` + `rigctld` driving `rtl_fm` tuning. Produces cleaner locks at low elevations.

### 2.3 Antenna

**Same recommendations as NOAA APT.** Quadrifilar helix (QFH) is the gold standard. V-dipole works for strong passes but LRPT is less forgiving of weak signal than APT — you need more margin.

Because LRPT is digital, the signal either decodes or it doesn't. There's no "noisy but viewable" middle ground like APT. This makes antenna quality matter more.

An LNA with bandpass filtering is close to mandatory for Meteor-M. Pass rates go from "occasional success" to "nearly every pass decodes" with a proper RF frontend.

---

## 3. The Signal

### 3.1 Frame Structure (top-down)

```
Physical layer:     QPSK symbols at 72 ksym/s
  ↓
Convolutional code: Rate 1/2, constraint length 7, Viterbi decodable
  ↓
CCSDS Transfer Frame: 1024 bytes (includes 4-byte sync marker)
  ↓
Reed-Solomon (255, 223): corrects up to 16 byte errors per 255-byte block
  ↓
CCSDS Source Packets: variable length, packed into frames
  ↓
JPEG-compressed image strips: reassembled into scan lines
  ↓
Final image: multichannel raster
```

### 3.2 QPSK Modulation

Four phase states, 2 bits per symbol:

```
Bits  Phase
00    45°
01    135°
11    225°
10    315°
```

Gray-coded (adjacent bit patterns differ by one bit), so a symbol error from noise typically causes only one bit error.

Meteor-M uses **differentially-encoded QPSK** in some variants (to resolve phase ambiguity) and **Offset QPSK (OQPSK)** in others. Newer Meteor-M 2-3/2-4 transmissions use standard QPSK. You may need to try both in your decoder if you're building from scratch.

### 3.3 CCSDS Transfer Frame

After Viterbi decoding, you get a stream of bytes containing CCSDS Transfer Frames. Each frame is 1024 bytes:

```
| ASM    | Frame header | VCDU data field              | RS parity |
| 4 B    | 6 B          | 882 B                        | 132 B     |
```

- **ASM (Attached Sync Marker)**: fixed 4-byte pattern `0x1ACFFC1D` that marks frame boundaries
- **Frame header**: spacecraft ID, virtual channel ID, frame counter
- **VCDU data field**: actual payload (image packets, telemetry, etc.)
- **RS parity**: 4 interleaved Reed-Solomon (255, 223) codewords

### 3.4 Sync Marker

`0x1ACFFC1D` (`0001 1010 1100 1111 1111 1100 0001 1101`) is the universal CCSDS ASM. Your framer searches for this pattern in the bitstream. Due to the convolutional encoder's rate 1/2, the ASM appears every 2048 channel bits (= 1024 bytes).

Because of the QPSK phase ambiguity (you might lock 90°, 180°, or 270° off from correct), the ASM might appear inverted or rotated. Your framer should look for `0x1ACFFC1D` and its three rotated/inverted variants.

---

## 4. Decoder Pipeline

Big picture:

```
IQ samples → matched filter → timing recovery → carrier recovery →
QPSK demod → Viterbi decode → sync marker search → descramble →
Reed-Solomon decode → CCSDS packet extraction → JPEG decode →
channel assembly → RGB composite → PNG
```

Each of these is a real piece of work. This is not a weekend project.

### 4.1 Matched Filter

QPSK uses a root-raised-cosine pulse shape. Your matched filter is the same RRC applied at the receiver:

```python
from commpy.filters import rrcosfilter

# beta = 0.6 for Meteor-M (verify against current spec)
# symbol rate = 72000, sample rate = whatever you decimated to
num_taps = 51
t, rrc = rrcosfilter(num_taps, alpha=0.6, Ts=1/72000, Fs=sample_rate)

filtered = np.convolve(iq_samples, rrc, mode='same')
```

### 4.2 Timing Recovery

You need to sample exactly once per symbol, at the symbol center. **Gardner timing error detector** is the standard choice for QPSK:

```
error = (y[n] - y[n-2]) * conj(y[n-1])
```

where `y` is the matched-filter output sampled at 2×symbol rate. The real part of `error` drives a loop filter → NCO → timing adjustment. Converges in a few hundred symbols.

### 4.3 Carrier Recovery (Costas Loop)

Even after matched filtering, your signal has residual frequency and phase offset. A Costas loop for QPSK:

```python
def costas_qpsk(sample, phase_estimate):
    rotated = sample * np.exp(-1j * phase_estimate)

    # Phase error for QPSK: product of sign of real and imaginary parts
    error = np.sign(rotated.real) * rotated.imag - np.sign(rotated.imag) * rotated.real

    # Loop filter (PI controller)
    frequency_estimate += Ki * error
    phase_estimate += frequency_estimate + Kp * error

    return rotated, phase_estimate, frequency_estimate
```

Converges to one of four phase ambiguities. Sync marker search resolves which one.

### 4.4 QPSK Demod to Soft Bits

After carrier recovery, each sample is a complex point near one of four constellation locations:

```python
# Hard bit decisions
bit0 = 1 if sample.real < 0 else 0
bit1 = 1 if sample.imag < 0 else 0
```

For best Viterbi performance, use **soft bits** — map the real and imaginary parts directly to signed integers representing confidence:

```python
soft_bit_0 = -sample.real * scale  # positive = 0 confidence, negative = 1
soft_bit_1 = -sample.imag * scale
```

Soft-decision Viterbi gains ~2 dB vs hard-decision.

### 4.5 Viterbi Decoding

Rate 1/2, constraint length 7, polynomials **G1 = 0o171, G2 = 0o133** (octal, NASA standard). This is the same convolutional code used in many CCSDS applications.

Don't write your own Viterbi from scratch — it's a classic compute-heavy DSP routine that's been optimized to death. Use:

- **libfec** (Phil Karn's library, AVX-optimized) — the canonical fast implementation
- **gr-satellites** in GNU Radio — has all of this in a ready-made block chain
- **SatDump**'s Viterbi — pulled from libfec, ready to use

Input: soft bits at 2× the output rate. Output: decoded bitstream at the original (pre-encoding) rate.

### 4.6 Sync Marker Search & Frame Sync

Search the decoded bitstream for `0x1ACFFC1D`. Once found, frames are every 8192 bits (1024 bytes) thereafter.

Handle the four phase ambiguities: if you don't find the sync marker, try rotating your QPSK constellation by 90°, 180°, 270° (equivalent to swapping/inverting real/imaginary) and re-searching.

### 4.7 Descrambling

CCSDS frames are randomized by XORing with a pseudo-random sequence (PN sequence, polynomial `x^8 + x^7 + x^5 + x^3 + 1`). This ensures sufficient bit transitions for the receiver's timing recovery regardless of payload content.

Your descrambler is the exact same XOR operation on the receiver side — generate the PN sequence and XOR it into each frame (skipping the ASM, which is transmitted unscrambled).

### 4.8 Reed-Solomon Decoding

Each 1024-byte frame contains **four interleaved RS(255, 223) codewords**. De-interleave (take every 4th byte into 4 separate codewords), then RS-decode each.

RS(255, 223):
- 223 data bytes + 32 parity bytes = 255 bytes per codeword
- Corrects up to 16 byte errors per codeword
- Uses the CCSDS dual-basis representation (slightly different field polynomial than "standard" RS)

Again: use **libfec** (`decode_rs_ccsds`) rather than writing your own. Writing a correct RS decoder for CCSDS is a week's work.

### 4.9 CCSDS Packet Extraction

After RS decode, strip the 6-byte frame header and parity, leaving 882 bytes of VCDU data. These bytes contain CCSDS Space Packets, each with its own header:

```
| Packet header  | Packet data      |
| 6 B            | variable         |
```

Packet header includes Application Process ID (APID) which tells you what the packet carries:

| APID range | Content |
|-----------|---------|
| 64–69 | Image data from channels 1–6 |
| Others | Telemetry, housekeeping |

### 4.10 JPEG Reconstruction

Image data within APID 64–69 packets is **JPEG-compressed strips**, each strip being 8 scan lines tall and 1568 pixels wide. Decode each strip with a JPEG decoder, assemble vertically into the full channel image.

**Meteor-M uses a custom JPEG variant** — standard libjpeg won't decode it out of the box. The quantization tables are fixed by the spec rather than transmitted in the stream, and the entropy coding uses fixed Huffman tables. You need a modified JPEG decoder.

SatDump and `medet` (classic Meteor-M decoder by Oleg) both have working implementations you can reference.

### 4.11 Channel Assembly and RGB Composite

You now have up to 6 channel images. Typical composites:

- **Channels 1, 2, 3** (visible, near-IR, red) → pseudo-true-color RGB
- Single channel IR for thermal imagery

The "MSU-MR" sensor channels roughly map to:

| Channel | Wavelength | Typical use |
|---------|-----------|-------------|
| 1 | 0.5–0.7 μm (visible) | Blue channel in RGB |
| 2 | 0.7–1.1 μm (near-IR) | Green/red in RGB |
| 3 | 1.6–1.8 μm (SWIR) | Snow/ice discrimination |
| 4 | 3.5–4.1 μm (MWIR) | Thermal |
| 5 | 10.5–11.5 μm (LWIR) | Thermal (most common IR display) |
| 6 | 11.5–12.5 μm (LWIR) | Thermal, water vapor |

Which three channels are transmitted varies. Telemetry packets indicate which APIDs carry which channels for the current pass.

---

## 5. Practical Reality: Just Use SatDump

Writing an LRPT decoder from IQ to image is a **months-long** project if you're doing all stages yourself. The realistic path for a new hobbyist:

1. **Use SatDump** end-to-end. It handles everything from IQ to PNG.
2. **Read SatDump's source** if you want to understand the stages. It's C++, modular, and well-organized.
3. **Build individual stages yourself** as learning exercises — e.g. write your own QPSK demod and pipe its output to SatDump's Viterbi+onward chain.

Other options:

- **`meteor_demod` + `medet`** — older two-stage pipeline. `meteor_demod` does QPSK → soft bits; `medet` does Viterbi onward to images. Still functional, simpler codebase than SatDump.
- **`gr-satellites`** (GNU Radio) — good for graphical block-diagram experimentation; excellent for understanding signal flow.

---

## 6. Automation

Similar structure to NOAA APT:

```
gpredict (pass prediction)
     │
     │  trigger on AOS (acquisition of signal)
     ▼
rtl_sdr -f 137900000 -s 2400000 → raw IQ file
     │
     ▼
SatDump offline decode → PNG channel images + false color composite
     │
     ▼
Upload / display
```

SatDump can also run in live mode directly from an RTL-SDR, decoding in real time and saving channels as they arrive.

---

## 7. References

**Specifications:**

- **CCSDS 131.0-B-3** (TM Synchronization and Channel Coding) — covers the ASM, Viterbi, RS, and scrambling. https://public.ccsds.org/Pubs/131x0b4.pdf
- **CCSDS 132.0-B-3** (TM Space Data Link Protocol) — frame format
- **Meteor-M LRPT signal description** — informally documented; primary source is reverse-engineered from working decoders

**Implementations:**

- **SatDump** — https://github.com/SatDump/SatDump
- **gr-satellites** — https://github.com/daniestevez/gr-satellites
- **meteor_demod** — https://github.com/dbdexter-dev/meteor_demod
- **medet** — https://github.com/artlav/meteor_decoder (original Oleg "artlav" version)

**Community knowledge:**

- **r/amateursatellites** subreddit — Meteor-M status updates, tips, pass recommendations
- **SatDump Discord** — active community, helpful for decoder troubleshooting
- **rtl-sdr.com Meteor tutorials** — good starting points for beginners

---

## 8. Suggested Build Order

If your goal is **decoded imagery fast**:

1. Build a QFH antenna (skip the V-dipole — LRPT needs better SNR)
2. Install SatDump
3. Use gpredict to schedule pass recordings
4. Run SatDump offline decode
5. Enjoy images
6. *Then* read SatDump's source to learn how it works

If your goal is **understanding every stage**:

1. Record raw IQ of a known-good pass (don't fight the physics yet)
2. Write QPSK matched filter + timing recovery; visualize the constellation
3. Write Costas loop; confirm constellation locks
4. Use libfec for Viterbi (decoding one from scratch is a project in itself)
5. Sync marker search; frame alignment
6. Descrambler
7. Reed-Solomon via libfec
8. CCSDS packet parsing
9. JPEG strip reassembly (use SatDump's Meteor-specific JPEG code as reference)
10. Channel compositing

Either path is valid. The first gets you pictures in an evening; the second makes you a better DSP engineer.

---

*End of reference.*
