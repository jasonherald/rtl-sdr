# NOAA APT Weather Satellite Reception: Complete Implementation Reference

Decoding Automatic Picture Transmission (APT) from NOAA polar-orbiting weather satellites on 137 MHz using an RTL-SDR. Each successful pass yields a real satellite image of Earth — visible and infrared strips of cloud cover, weather systems, and terrain — captured from signal received off your own antenna.

---

## 1. Background

### 1.1 What APT Is

APT is an analog image transmission format used by NOAA weather satellites since the 1960s. The satellite carries a scanning radiometer that sweeps across the ground track as the satellite orbits, producing one scan line per sweep. Each line is transmitted in near-real-time on a VHF FM downlink at 137 MHz.

The encoding is charmingly primitive: the image brightness modulates the amplitude of a **2400 Hz audio subcarrier**, and that audio is FM-modulated onto the 137 MHz carrier. So the signal chain is:

```text
Scan line → AM(2400 Hz subcarrier) → FM(137 MHz) → your dongle
```

Your decoder reverses this: FM-demodulate to recover the 2400 Hz AM audio, then AM-demodulate the audio envelope to recover image brightness, then assemble scan lines into an image.

### 1.2 Active Satellites

As of April 2026, the operational fleet is:

| Satellite | Frequency | Status |
|-----------|-----------|--------|
| NOAA 15 | 137.620 MHz | Operational but aging (launched 1998); APT may cease anytime |
| NOAA 18 | 137.9125 MHz | Operational |
| NOAA 19 | 137.100 MHz | Operational |

**NOAA 15's SBUV instrument and some others have failed.** APT is still working as of April 2026 but the satellite is well past its design life. If you want to catch it, now is the time.

The European **Meteor-M series** broadcasts on nearby frequencies (137.1 / 137.9 MHz) but uses **LRPT** (digital QPSK), not APT. That's a separate project — see the LRPT guide.

### 1.3 Orbit and Passes

NOAA birds are **sun-synchronous polar orbiters** at ~850 km altitude, ~102-minute orbital period. Each satellite makes ~14 passes per day over any given point on Earth, but only 4–6 of those will be high enough (>20° elevation) to get clean reception. A usable pass lasts **10–15 minutes**.

Pass prediction tools:
- **gpredict** (desktop, Linux-native, great on Arch)
- **n2yo.com** (web)
- **Heavens-Above.com** (web)

You need current **TLEs (Two-Line Element sets)** from Celestrak or Space-Track. Most tools auto-update these.

---

## 2. RF Setup

### 2.1 Tuning Parameters

| Parameter | Value |
|-----------|-------|
| Center frequency | 137.100 / 137.620 / 137.9125 MHz (per satellite) |
| Sample rate | 48,000–240,000 Hz after decimation (raw dongle at 2 MSPS, decimate hard) |
| FM deviation | ±17 kHz (narrow FM, but use ~40 kHz bandwidth filter to allow for Doppler + overdeviation) |
| Gain | ~30–40 dB. Satellite signal is strong when overhead; reduce if you see distortion |
| PPM correction | Matters more here than for ADS-B; set based on your measured dongle offset |

### 2.2 Doppler Shift

Low Earth orbit satellites exhibit significant Doppler shift. At 137 MHz, peak-to-peak Doppler across a pass is roughly **±3.5 kHz**. This is well within your tuning bandwidth — you don't *have* to correct for it — but ideally you want the audio subcarrier to stay locked at exactly 2400 Hz.

Two approaches:

1. **Wide bandwidth + no correction.** Use a 40 kHz wide FM demod and accept a small frequency shift in the output audio. Decoders tolerate this.
2. **Software Doppler correction.** The decoder (or an upstream tool like `gpredict` with its rotator/radio interface) continuously retunes the center frequency based on predicted satellite velocity. Produces cleaner audio and better images.

For a first attempt, skip Doppler correction. Fix it later if your images have visible bending or brightness drift across the pass.

### 2.3 Antenna

This is where NOAA APT lives or dies. Options, in increasing order of quality:

**V-Dipole (recommended starting point)**
- Two 53 cm elements forming a 120° V, pointing horizontally
- Oriented so the V "opens" north-south (for most passes, satellites travel N↔S)
- Cheap to build: two pieces of wire or brazing rod, a coax connector, a plastic mount
- Horizontal polarization works surprisingly well despite the satellite transmitting RHCP

**Quadrifilar Helix (QFH)**
- Four helical elements forming two orthogonal bifilar helices
- Right-hand circular polarization — matches the satellite exactly
- Omnidirectional hemisphere pattern — no steering needed
- Best passive antenna for this job
- Build it from ½" PVC + copper wire; plans widely available

**Turnstile (crossed dipoles)**
- Two dipoles at 90° with 90° phase offset between feeds
- Also RHCP, simpler than QFH but lower gain at the horizon
- Decent performance, easier build than QFH

**Critical consideration**: NOAA transmits on VHF, but FM broadcast at 88–108 MHz is *very* close by and *much* stronger. An **FM broadcast notch filter** or a 137 MHz bandpass filter is close to mandatory. Without one, FM broadcast intermod will destroy your images.

### 2.4 Pre-Amp

An LNA (low-noise amplifier) at the antenna base with ~20 dB gain noticeably improves image quality, especially at low elevation angles. Nooelec and SAWbird both sell 137 MHz purpose-built LNAs with integrated bandpass filtering — one product solves two problems.

---

## 3. The Signal in Detail

### 3.1 Subcarrier and Line Rate

- Subcarrier frequency: **2400 Hz** (exactly)
- Line rate: **2 lines/second**
- Samples per line (when audio is sampled at 11,025 Hz): **5,512.5** — note the half-sample; interpolation matters
- Pixels per line: **2080** (including sync, telemetry, and both channels)

### 3.2 Line Structure

Every scan line contains two image channels side-by-side, plus sync pulses and telemetry frames. Layout of one 2080-pixel line:

```text
| Sync A | Space A | Image A | Telemetry A | Sync B | Space B | Image B | Telemetry B |
| 39 px  | 47 px   | 909 px  | 45 px       | 39 px  | 47 px   | 909 px  | 45 px       |
```

Total: 39 + 47 + 909 + 45 + 39 + 47 + 909 + 45 = 2080 pixels.

**Sync A**: 7-cycle 1040 Hz square wave burst. Used to lock the decoder to line starts.

**Sync B**: 7-cycle 832 Hz square wave burst. Different frequency so you can tell Channel A start from Channel B start.

**Image A/B**: the actual picture data. 909 pixels wide each.

**Telemetry A/B**: wedge patterns used for calibration — 16 wedges of 8 lines each, forming a 128-line repeating pattern with known grayscale values. You can use these to calibrate brightness and to identify which sensor is on which channel.

### 3.3 Channels

The AVHRR radiometer onboard has 5 channels (visible, near-IR, and three thermal IR bands). Only **two** at a time are sent via APT — the choice depends on whether the satellite is in daylight or darkness over the current part of its pass:

| Mode | Channel A | Channel B |
|------|-----------|-----------|
| Day  | Channel 2 (near-IR, 0.725–1.0 μm) | Channel 4 (thermal IR, 10.3–11.3 μm) |
| Night| Channel 3B (3.55–3.93 μm) | Channel 4 or 5 (thermal IR) |

Daytime images are the visually striking ones — you can see clouds, land, ocean. Nighttime images are thermal: hot = dark, cold = bright (so cold cloud tops look like snow).

### 3.4 Telemetry Wedges

The right edge of each channel carries a **calibration strip**: 16 grayscale wedges, each 8 scan lines tall, cycling through known brightness values (0, 31, 63, 95, 127, 159, 191, 223, 255, plus a "zero modulation" reference, then channel ID wedges identifying which AVHRR channel is being transmitted).

You can ignore these for a first-cut decoder. For calibrated thermal imagery, they're essential.

---

## 4. Decoder Pipeline

```text
IQ samples → FM demod → resample to 11,025 Hz → AM demod (envelope) →
sync detection → line slicing → 2D image buffer → histogram/color map → PNG
```

### 4.1 FM Demodulation

Standard quadrature FM discriminator. For complex samples `x[n] = I[n] + jQ[n]`:

```text
audio[n] = atan2(imag(x[n] * conj(x[n-1])), real(x[n] * conj(x[n-1])))
```

Equivalently:

```text
audio[n] = atan2(I[n]*Q[n-1] - Q[n]*I[n-1],
                 I[n]*I[n-1] + Q[n]*Q[n-1])
```

The result is the instantaneous frequency deviation, proportional to the recovered audio signal. Scale to taste.

### 4.2 Resampling

Audio out of the FM demod comes at your baseband rate (e.g. 48,000 Hz if you decimated to there). You want **11,025 Hz** or a multiple. This gives exactly 5512.5 samples per line, and with a resampler you can target integer samples per pixel (e.g. resample to `11025 * 2 = 22050` Hz → 11025 samples per line, 5.3 samples per pixel).

Use a polyphase resampler (in Python: `scipy.signal.resample_poly`).

### 4.3 AM Envelope Demodulation

You now have an audio signal with a 2400 Hz carrier whose amplitude is the image brightness. To recover brightness, compute the envelope.

**Hilbert transform method (accurate):**

```python
from scipy.signal import hilbert
analytic = hilbert(audio)
envelope = np.abs(analytic)
```

**Rectify + lowpass (fast):**

```python
rectified = np.abs(audio)
envelope = lowpass(rectified, cutoff=1200)  # well below 2400 Hz
```

Either works. Hilbert is cleaner; rectify+LPF is cheaper.

### 4.4 Downsample to Pixel Rate

After envelope extraction, downsample to exactly **4160 samples/second** (that's 2 lines × 2080 pixels per second). Now each sample is one image pixel.

```python
pixels = resample_poly(envelope, 4160, input_rate)
```

### 4.5 Sync Detection and Line Alignment

This is the one step where clever matters.

Build a reference **Sync A** pattern — 39 samples representing the 1040 Hz square wave burst at your pixel rate. Cross-correlate this with the pixel stream. Peaks in the correlation mark the start of each scan line.

```python
sync_a = build_sync_a_template(length=39)  # +1/-1 pattern, 7 cycles of 1040 Hz

correlation = np.correlate(pixels, sync_a, mode='valid')

# Find peaks spaced ~2080 pixels apart
line_starts = find_peaks_with_spacing(correlation, spacing=2080, tolerance=50)
```

Real-world refinements:

- Use **sub-pixel alignment**: quadratic interpolation of correlation peaks gives sub-pixel timing and prevents image slant
- Track spacing adaptively: if your clock drifts (or Doppler compresses/expands the signal), the spacing between sync pulses drifts accordingly. Use a PLL or a running average to adapt
- Discard lines where sync quality is poor (low correlation peak) — this removes noise bursts and dropped samples

### 4.6 Line Extraction

Once you have line_starts, slice each line:

```python
image = np.zeros((num_lines, 2080), dtype=np.uint8)
for i, start in enumerate(line_starts):
    line = pixels[start:start + 2080]
    image[i, :] = normalize_to_uint8(line)
```

### 4.7 Brightness Normalization

Raw envelope values won't span 0–255. Simple approach: percentile-based normalization.

```python
low, high = np.percentile(pixels, [1, 99])
normalized = np.clip((pixels - low) / (high - low) * 255, 0, 255).astype(np.uint8)
```

Telemetry-based normalization is more accurate — use the calibration wedges to map known-reference grayscale values to 0–255 — but percentile works fine for pretty pictures.

### 4.8 Save

Emit as PNG or raw grayscale. Two side-by-side images come out: Channel A on the left, Channel B on the right, 909 pixels wide each.

---

## 5. Image Enhancements

Raw APT images look washed out. Common post-processing:

### 5.1 False Color

Combine Channel A (visible/near-IR) and Channel B (thermal IR) into a synthetic color image. Classic recipe ("NO" in WXtoImg):

- **Red** = Channel A
- **Green** = Channel A (or a blend)
- **Blue** = inverted Channel B (so cold clouds → white, warm sea → blue)

More sophisticated: use Channel B to classify (land vs. cloud vs. sea based on thermal) and apply different color maps to each class.

### 5.2 Contrast Enhancement

- Linear stretch with percentile clipping (above)
- Histogram equalization (OpenCV: `cv2.equalizeHist`)
- CLAHE (Contrast Limited Adaptive Histogram Equalization) — localized contrast, preserves edges

### 5.3 Geometric Correction

Raw APT images have no map projection. The satellite scans along a curved path, and the edges of each scan line are at different slant ranges than the center, so there's geometric distortion (looks like pincushion). For pretty images, this doesn't matter. For real geolocation, you need to:

1. Know the satellite's precise position at the time of each line (from TLE + SGP4 propagator)
2. Map each pixel from (line, column) → (scan angle, time)
3. Ray-trace from the satellite through the Earth's surface
4. Resample into a map projection (lat/lon, Mercator, etc.)

This is what tools like `noaa-apt` (Rust-based modern decoder) and the classic `wxtoimg` do.

---

## 6. Automation

The natural architecture:

```text
gpredict (or schedule script)
     │
     │  triggers on pass prediction
     ▼
rtl_fm -f <freq> -s 48000 | apt_decoder → /images/YYYY-MM-DD_<sat>.png
     │
     ▼
post-process: color map, stretch, annotate with pass info, upload to web
```

Key tools:

- **`predict`** / **`pypredict`** — CLI pass predictor using TLEs. Can emit start/end times for cron-style automation.
- **`rtl_fm`** — simple FM demodulator that pipes PCM to stdout; good enough for APT.
- **`noaa-apt`** — standalone APT decoder with modern UI and CLI, good defaults.
- **`SatDump`** — general satellite decoder, handles APT + LRPT + many others.

For a cron-style setup, `wxtrack` and `autowx2` are pre-built Python automation layers that do pass prediction, recording, decoding, and upload. Or build your own — it's ~200 lines.

---

## 7. Testing and Validation

### 7.1 Record Raw IQ During a Pass

```text
rtl_sdr -f 137912500 -s 2400000 -g 40 noaa18_pass.iq
```

A 15-minute pass is ~4 GB. Having the raw file lets you replay through different decoder versions as you iterate.

### 7.2 Sample Files

Several people post raw APT IQ captures for algorithm development. Check the SDR subreddit and `rtl-sdr.com` archives. There are also clean audio WAV recordings of passes, which lets you skip the FM demod entirely and start at step 4.3.

### 7.3 Cross-Check

Run `noaa-apt` on the same recording. Your decoder should produce substantially similar imagery — noise differences are fine; gross shape and brightness should agree.

---

## 8. Common Problems

| Symptom | Likely cause |
|---------|--------------|
| Diagonal slanting/skewing across image | Sample rate drift or wrong rate; also check sub-pixel sync alignment |
| Horizontal bands of noise | Weak signal at low elevation, or nearby RF interference |
| Washed out / low contrast | Need better normalization, or telemetry-wedge-based calibration |
| Image repeats/overlaps | Double-triggering on sync pulses — tighten peak detection spacing |
| Can't find sync at all | FM broadcast intermod is swamping your dongle; add FM notch filter |
| Only half the image received | Pass was marginal elevation; satellite went below horizon mid-pass |
| Completely white or black | Gain misconfigured; try AGC off and manual gain 30 dB |

---

## 9. References

**Specifications:**

- **NOAA KLM User's Guide** (free PDF from NOAA/NESDIS) — authoritative source for APT format, telemetry, and AVHRR channel definitions. https://www.star.nesdis.noaa.gov/mirs/documents/0.0_NOAA_KLM_Users_Guide.pdf
- NOAA APT format description: https://noaasis.noaa.gov/NOAASIS/ml/apt.html

**Implementations to study:**

- **noaa-apt** (Rust, modern, clean code) — https://noaa-apt.mbernardi.com.ar/
- **SatDump** (C++, all-in-one satellite decoder) — https://github.com/SatDump/SatDump
- **WXtoImg** (classic Windows tool, closed-source but archived) — reference for enhancement filters

**Community:**

- **r/RTLSDR** and **r/amateursatellites** on Reddit
- **rtl-sdr.com** — tutorials and hardware reviews
- **sdrforum** — occasional APT threads

**TLEs:**

- **Celestrak** — https://celestrak.org/NORAD/elements/weather.txt
- **Space-Track.org** — requires free registration

---

## 10. Suggested Build Order

1. **Set up pass prediction** with gpredict; confirm you know when the next usable pass is
2. **Build a V-dipole** (30 minutes with wire and a coax connector)
3. **Record raw IQ** of a pass using `rtl_sdr` — you now have test data regardless of decoder state
4. **FM demodulate** — verify you hear the characteristic "tick-tick-tick" of the APT signal in a wav file
5. **AM envelope extraction + resample to pixel rate** — at this point you have a 1D stream of pixel values
6. **Sync detection** — find line starts; plot correlation and verify peaks are ~2080 pixels apart
7. **Line slicing + PNG output** — first image. It will be ugly but recognizable.
8. **Normalization and contrast** — now it looks like a satellite photo
9. **False color** — now it looks like something you'd see in a weather report
10. **Geometric correction + map projection** (optional) — research-grade output
11. **Automate** end-to-end for scheduled passes

Each step is independently testable on a recorded IQ file. You can work on this without a satellite overhead once you have one good recording.

---

*End of reference.*
