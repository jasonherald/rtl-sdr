# SSTV (Slow-Scan Television) Reception: Complete Implementation Reference

Decoding Slow-Scan Television images transmitted by ham radio operators and, occasionally, by cosmonauts aboard the International Space Station. A 1950s-era analog image format that still sees active use — each received image comes out as a photograph transmitted over voice-bandwidth radio.

---

## 1. Background

### 1.1 What SSTV Is

SSTV is an analog image transmission technique designed to fit within a standard voice audio channel (~3 kHz bandwidth). Instead of fax-style direct scanning, SSTV encodes image brightness (and sometimes color) as **frequency modulation of audio tones** between roughly 1500 Hz and 2300 Hz.

The format evolved from 1950s monochrome slow-scan experiments (Copthorne Macdonald's work) into a zoo of color modes in the 1980s. Modern SSTV has ~20 commonly-used modes varying in resolution, color encoding, and transmission time.

### 1.2 Why It's Interesting on RTL-SDR

SSTV signals appear in:

- **Ham radio bands**: 14.230 MHz (20m, primary worldwide SSTV calling frequency), 21.340 MHz (15m), 28.680 MHz (10m), 144.5 MHz (2m)
- **ISS downlinks**: 145.800 MHz FM, during SSTV events (typically 2–4 per year, running for a few days each)

The RTL-SDR R820T2 tunes 24 MHz–1.766 GHz. This means:
- **ISS SSTV on 145.800 MHz**: works directly with any RTL-SDR. Easy target.
- **HF ham SSTV** (14.230 MHz etc.): **below** RTL-SDR's range. Requires an upconverter (e.g. Ham-It-Up, converts HF to 100 MHz + HF) or a different SDR.

For this guide, the focus is **2m (145.800 MHz) ISS SSTV** since that's what you can do with the hardware you have.

### 1.3 ISS SSTV Events

The ARISS (Amateur Radio on the International Space Station) program periodically transmits SSTV images from ISS to celebrate anniversaries, educational outreach, or cosmonaut personal projects. Schedule:

- Typical frequency: 145.800 MHz narrow FM
- Mode: typically **PD120** or **PD180** (Russian cosmonaut choices)
- Duration: usually 2–5 days per event
- Images: 12 different pictures cycled, transmitted ~every 3 minutes
- Events announced at **ariss.org** and **amsat.org** a few weeks in advance

Participants who successfully receive and decode images can submit them for a certificate. It's a cool piece of mail from space.

### 1.4 Ground-Based Ham SSTV

Active operators on HF (with an upconverter, or a different receiver) send SSTV for fun — portrait photos, landscapes, test patterns. There are SSTV contests occasionally. On 20m, activity peaks on weekends in the evenings local time.

---

## 2. RF Setup

### 2.1 For ISS SSTV (145.800 MHz)

| Parameter | Value |
|-----------|-------|
| Center frequency | 145.800 MHz |
| Sample rate | 48 kHz audio (from FM demod) |
| FM bandwidth | 25 kHz (standard narrow FM) |
| Gain | 30–40 dB |

### 2.2 For Ground Ham SSTV (HF)

Need an upconverter. Ham-It-Up Plus or SpyVerter adds a 125 MHz (or 100 MHz) local oscillator, so HF signals appear at `LO + HF`. A 14.230 MHz signal becomes 139.230 MHz (with 125 MHz LO), within RTL-SDR range.

### 2.3 Antenna for ISS

ISS at overhead passes has strong signal — **any 2m antenna works for ISS SSTV**:

- Quarter-wave vertical (48 cm element)
- 2m/70cm dual-band whip (common ham antennas)
- Magnetic-mount mobile antenna on a metal surface
- An indoor telescoping whip, even

Circular polarization helps (ISS tumbles, antenna orientation varies) but isn't critical. A quarter-wave vertical placed outdoors with a clear view of the sky decodes most passes just fine.

For **HF ham SSTV**: need an HF antenna (random wire, dipole, end-fed, etc.) — much bigger antennas, out of scope for this RTL-SDR-focused doc.

### 2.4 Doppler on ISS

ISS at 145.800 MHz has Doppler shift of ~±3.5 kHz peak-to-peak, same magnitude as NOAA weather satellites. Since FM is being used, the demodulator is insensitive to carrier frequency offset (within the receive bandwidth), so no correction needed. The decoded audio tones stay correct.

---

## 3. SSTV Mode Zoo

The single most important fact about SSTV decoding: **there are many modes**, all different, and the decoder must identify which mode is in use. Identification happens via a **VIS code** (Vertical Interval Signalling) at the start of each transmission.

Common modes you'll encounter:

| Mode | Resolution | Duration | Color | Typical use |
|------|-----------|----------|-------|-------------|
| **Robot 36** | 320×240 | 36 sec | Color YCrCb | Common on ISS (older events) |
| **Robot 72** | 320×240 | 72 sec | Color YCrCb | Higher quality version |
| **PD50** | 320×256 | 50 sec | Color YCrCb | ISS (common) |
| **PD90** | 320×256 | 90 sec | Color YCrCb | ISS |
| **PD120** | 640×496 | 2 min | Color YCrCb | ISS (most common recent) |
| **PD180** | 640×496 | 3 min | Color YCrCb | ISS (high quality option) |
| **Martin M1** | 320×256 | 114 sec | Color RGB | Ham HF |
| **Martin M2** | 320×256 | 58 sec | Color RGB | Ham HF |
| **Scottie S1** | 320×256 | 110 sec | Color RGB | Ham HF |
| **Scottie S2** | 320×256 | 71 sec | Color RGB | Ham HF |
| **Scottie DX** | 320×256 | 4.5 min | Color RGB | Ham HF (high quality) |
| **Robot BW** | Various | Various | Monochrome | Legacy |

ISS uses **PD120** most often now; historically it's varied.

---

## 4. Signal Structure

### 4.1 Frequency Mapping

Image data: **frequency modulates** an audio subcarrier:

| Audio frequency | Meaning |
|-----------------|---------|
| 1500 Hz | Black (darkest pixel) |
| 2300 Hz | White (brightest pixel) |
| 1200 Hz | Sync pulse |

So pixel brightness is the instantaneous audio frequency within 1500–2300 Hz. Grayscale is trivial; color modes transmit separate R/G/B (or Y/Cr/Cb) scan lines in sequence.

### 4.2 VIS Code (Mode Identification)

Every SSTV transmission starts with a **VIS code** — a tone sequence identifying the mode:

```
| Leader | Break | Leader | VIS start | VIS bits (7+parity+stop) |
| 1900 Hz, 300 ms | 1200 Hz, 10 ms | 1900 Hz, 300 ms | 1200 Hz, 30 ms | alternating 1100/1300 Hz, 30 ms each |
```

Each VIS bit is:
- **"0" bit**: 1300 Hz for 30 ms
- **"1" bit**: 1100 Hz for 30 ms

7 data bits + 1 parity bit + 1 stop bit = 9 total bits × 30 ms = 270 ms of VIS code.

The 7-bit VIS code identifies the mode:

| VIS | Mode |
|-----|------|
| 0x08 | Robot 8 color |
| 0x0C | Robot 24 color |
| 0x04 | Robot 36 color |
| 0x0C (different parity) | Robot 72 color |
| 0x2C | Martin M1 |
| 0x28 | Martin M2 |
| 0x3C | Scottie S1 |
| 0x38 | Scottie S2 |
| 0x4C | Scottie DX |
| 0x5D | PD50 |
| 0x63 | PD90 |
| 0x5F | PD120 |
| 0x60 | PD180 |

(Full VIS table in any SSTV decoder source. The `black.qsl.net/sstv-handbook/` reference has all of them.)

### 4.3 Scan Line Structure

After VIS, image data begins. Each mode has its own scan line format. Example: **PD120**:

- 640 pixels wide, 496 lines tall
- Each "line group" encodes **two image lines** at once (Y1, R-Y, B-Y, Y2)
  - Y1: luminance of first line
  - R-Y: color difference (red-minus-luminance), shared between both lines
  - B-Y: color difference (blue-minus-luminance), shared between both lines
  - Y2: luminance of second line

Timing within one PD120 line group:

```
| Sync | Porch | Y1        | R-Y       | B-Y       | Y2        |
| 20 ms (1200 Hz) | 2.08 ms | 218.43 ms | 218.43 ms | 218.43 ms | 218.43 ms |
```

Each "line" of image data is the audio frequency sampled across 218.43 ms. At 48 kHz sample rate: 10,485 samples per line. For 640 pixels per line, that's ~16 samples per pixel.

Convert between audio frequency and pixel value:

```python
pixel = (audio_freq_hz - 1500) / 800 * 255   # 1500 Hz = 0, 2300 Hz = 255
pixel = np.clip(pixel, 0, 255)
```

Y/Cr/Cb to RGB:

```python
R = Y + 1.40200 * (Cr - 128)
G = Y - 0.34414 * (Cb - 128) - 0.71414 * (Cr - 128)
B = Y + 1.77200 * (Cb - 128)
```

(Standard YCbCr conversion.)

### 4.4 Mode Differences

**Martin/Scottie modes** use direct RGB (not YCrCb). Each line contains three sequential color components instead of the Y1/R-Y/B-Y/Y2 quad.

**Robot modes** use various schemes — Robot 36 is YCrCb with 2:1 color subsampling (like PD), Robot BW is straight grayscale.

Covering every mode's byte-level structure is ~50 pages of dense detail. The SSTV Handbook linked below has all of them.

---

## 5. Decoder Pipeline

```
IQ samples → FM demod → 48 kHz audio → VIS detection → mode identification →
line sync detection → FM-within-audio demod (1500–2300 Hz → brightness) →
scan line extraction → color reconstruction → PNG
```

### 5.1 FM Demodulation (RF)

Standard FM discriminator on the 145.800 MHz narrow FM signal, producing 48 kHz audio. This is what you'd hear in a conventional ham receiver in NFM mode.

### 5.2 Detecting VIS

Continuously monitor the audio for the VIS pattern:

1. Look for a **1900 Hz tone** lasting >200 ms (leader)
2. Followed by a **brief 1200 Hz pulse** (~10 ms, the break)
3. Followed by another **1900 Hz leader** (~300 ms)
4. Followed by **1200 Hz for 30 ms** (VIS start)
5. Then 8 VIS bits at 30 ms each, each at 1100 or 1300 Hz

Use short FFTs (sliding window, ~30 ms wide) to estimate instantaneous frequency. Detect VIS pattern, extract mode code, compute parity to validate.

Alternative: **Goertzel algorithm** computes the power at specific frequencies (1100, 1300, 1200, 1900 Hz) cheaply without a full FFT — well-suited for VIS detection.

### 5.3 Extracting Instantaneous Audio Frequency

For the image data itself, you need frequency at every moment. Several approaches:

**Zero-crossing counting**: count zero crossings in a sliding window, infer frequency. Simple but noisy.

**FM demodulation of the audio**: treat the audio as FM (with center 1900 Hz, deviation ±400 Hz) and demodulate. The I/Q version:

```python
# Shift audio to baseband (1900 Hz → 0)
t = np.arange(len(audio)) / sample_rate
baseband = audio * np.exp(-2j * np.pi * 1900 * t)

# Lowpass to remove image component
baseband = lowpass(baseband, cutoff=600)

# FM demod
instant_freq = np.diff(np.unwrap(np.angle(baseband))) * sample_rate / (2 * np.pi)
instant_freq += 1900  # restore offset
```

Now `instant_freq[n]` is the audio frequency at sample `n`. Convert to pixel value via the mapping above.

**Direct Hilbert transform**:

```python
analytic = scipy.signal.hilbert(audio)
instant_phase = np.unwrap(np.angle(analytic))
instant_freq = np.diff(instant_phase) * sample_rate / (2 * np.pi)
```

Same result, different formulation.

### 5.4 Line Synchronization

The 1200 Hz sync pulse at the start of each scan line provides alignment. Find each sync pulse (search for a dip to 1200 Hz lasting ~20 ms in the instantaneous frequency). Each sync marks a line start.

Line spacing is fixed by mode — e.g. PD120 has line groups 877.4 ms apart. Sync pulses should appear at that spacing; use it to validate detection and to interpolate between sync-ambiguous regions.

### 5.5 Pixel Extraction

Between sync pulses, sample the instantaneous frequency at the correct rate for the mode (e.g. for PD120's Y1 segment, 218.43 ms / 640 pixels = 0.341 ms per pixel = ~16 samples at 48 kHz).

Average the instantaneous frequency within each pixel window; convert to brightness.

### 5.6 Color Reconstruction

For YCrCb modes: three Y/Cr/Cb components per pixel. Apply the matrix above to convert to RGB.

For RGB modes (Martin, Scottie): three R/G/B lines per image line are already in display space; assemble directly.

### 5.7 Image Output

Save as PNG. Typical resolutions: 320×240 to 640×496.

---

## 6. Practical Path: QSSTV

As with the other projects, there's a mature decoder. **QSSTV** (Qt SSTV) is the standard open-source tool:

- Auto-detects mode from VIS
- Decodes all common modes
- Supports live reception from sound card
- Command-line version exists for automation

On Arch: `pacman -S qsstv` or from the AUR.

For ISS SSTV events, the workflow:

1. Point 2m antenna skyward
2. Tune SDR to 145.800 MHz NFM
3. Pipe audio from `rtl_fm` into QSSTV via pulse/pipewire/alsa loopback
4. Watch images appear when ISS is overhead

**RX-SSTV** and **MMSSTV** are Windows alternatives.

---

## 7. Transmitting SSTV

Beyond the scope of RTL-SDR (receive-only), but worth mentioning:

Ham operators transmit SSTV for fun. Any SSB or FM transceiver can do it with audio from a computer running SSTV software. The simplest setup: a 2m handheld, a cable to the computer's sound card, and QSSTV or MMSSTV in transmit mode.

You'd need a **ham license** to transmit. General class license unlocks HF SSTV; Technician class gives you 2m. Covered by US ARRL license manuals and a weekend of studying.

---

## 8. ISS SSTV Pass Planning

1. **Check for an active event** at ariss.org or amsat.org. Events usually last 2–5 days; outside of them the downlink is silent.
2. **Get ISS pass predictions** from heavens-above.com or n2yo.com for your location. You want passes with maximum elevation >20° for clean reception.
3. **Set up receiver** before AOS (Acquisition of Signal). You have ~10 minutes per pass.
4. **Record the audio** — that way if your decoder fails live, you can retry offline.
5. **Expect partial images**. An image takes 2 minutes; a pass is 10 minutes. That's 3-5 images per pass if the downlink is continuous, but reception gaps are common at low elevation. Partial images are still collectible.

---

## 9. References

**Specifications and technical reference:**

- **The SSTV Handbook** by Dave Jones (KB4YZ) — comprehensive mode reference. https://www.sstv-handbook.com/
- **"Hamilton's Comprehensive Guide to SSTV"** — online tutorial with mode tables

**Software:**

- **QSSTV** — https://www.qsl.net/on4qz/qsstv/
- **MMSSTV** — http://hamsoft.ca/pages/mmsstv.php (Windows)
- **slowrx** — https://github.com/windytan/slowrx — lightweight Linux SSTV decoder, elegant code
- **sstv-tools** (Python) — https://github.com/colaclanth/sstv — simple Python implementation worth reading

**ISS SSTV:**

- **ARISS-SSTV**: https://ariss-sstv.blogspot.com/ — event announcements and certificate submissions
- **AMSAT**: https://www.amsat.org/ — broader amateur satellite news

**Frequency / operating practice:**

- **14.230 MHz** — primary worldwide HF SSTV calling frequency
- **7.033–7.043, 14.225–14.235, 21.335–21.345, 28.675–28.685 MHz** — typical SSTV ranges on HF bands
- **144.500 and 145.800 MHz** — VHF SSTV (both amateur and ISS)

---

## 10. Suggested Build Order

**For first success (ISS SSTV event):**

1. Wait for an announced ARISS SSTV event
2. Install QSSTV
3. Connect RTL-SDR → SDR software (e.g. SDR++) → audio pipe to QSSTV
4. Tune 145.800 MHz NFM during an ISS pass
5. Collect whatever images you get
6. Submit for certificate (free bragging rights from space)

**For decoder implementation (any SSTV audio source):**

1. Get a sample SSTV WAV file (QSSTV ships with examples; web archives have many)
2. Implement audio-frequency extraction (Hilbert transform → instantaneous frequency)
3. VIS detection — decode the mode from the opening tones
4. Implement a **single mode** fully — recommend Martin M1 (RGB, relatively simple)
5. Add line sync detection via 1200 Hz pulses
6. Extract scan lines, assemble image, save PNG
7. Validate against QSSTV on the same WAV
8. Extend to PD modes (handle YCrCb and the line-group structure)
9. Extend to all modes you care about
10. Real-time pipeline: `rtl_fm` audio stream → your decoder → continuous image output

---

*End of reference.*
