# SDR concepts, against this app's UI

A guide to the underlying ideas of software-defined radio,
explained as you'd see them in SDR-RS. Not a textbook — a
working set of mental models that lets you reason about what
each control actually does.

If you just want to receive your first signal, see
[`getting-started.md`](getting-started.md). This document is
for when you've done that and want to understand why it
worked.

---

## The chain, in one paragraph

The antenna picks up a wide swath of radio spectrum. The
RTL-SDR's tuner translates a chunk of that swath down to
near-zero-frequency baseband and digitises it into a stream of
**IQ samples** at a chosen **sample rate**. The Radio panel's
**channel filter** narrows that stream to one specific channel.
The **demodulator** turns the channel's IQ into audio (or
something else — bits, scan-lines, packets). The audio output
plays it. That's the whole signal path. Every panel in this app
configures one of those stages.

```text
antenna → tuner → IQ samples → channel filter → demodulator → audio
          [RTL-SDR]            [Radio panel]    [demod dropdown]
```

---

## IQ samples

The fundamental data type of SDR. Every sample the RTL-SDR sends
your computer is a *complex number*: a real part (called **I** for
"in-phase") and an imaginary part (**Q** for "quadrature").

### Why complex?

A real-only sample at one moment in time can't tell you which
direction a signal is rotating in the spectrum — it's just an
amplitude. A complex sample (I + jQ) encodes both amplitude *and*
phase. With phase you can distinguish a signal at +5 kHz offset
from one at −5 kHz offset; with only amplitude you couldn't.

### What you see in the UI

The **spectrum plot** at the top of the centre area is the FFT
(Fourier transform) of recent IQ samples. It shows you which
frequencies are present in the captured spectrum window: vertical
height = how strong each frequency is. The **waterfall** below
the spectrum is the same data over time, scrolling downward, with
brightness mapped to strength. Strong narrow signals look like
bright vertical bars. Wideband signals look like fat columns.
Noise is the diffuse glow.

### Sample size

The RTL-SDR sends 8-bit IQ pairs (one byte each for I and Q).
Cheap hardware, ~48 dB dynamic range. Higher-end SDRs (AirSpy,
SDRplay) send 12 or 16 bits per axis, which gets you 70-90 dB —
they hear weaker signals next to stronger ones. SDR-RS handles
the RTL-SDR's 8-bit stream natively; the spectrum view's dB
scale reflects what 8 bits can show.

---

## Sample rate

The number of IQ samples per second the dongle delivers to your
computer. The RTL-SDR can sample at rates from 250 ksps up to
~3.2 Msps. SDR-RS defaults around 2.0 Msps.

### What this controls

**The width of the spectrum view.** Sample rate = bandwidth you
can see at once. At 2.0 Msps the spectrum plot covers ±1 MHz
around the centre frequency (the Nyquist limit — half the sample
rate). At 250 ksps it covers ±125 kHz: same vertical resolution
of the FFT bins but a much narrower window.

### When to change it

Most users never need to. The defaults are tuned to give a useful
spectrum view (~2 MHz wide) and enough headroom for any single
demodulator. Push it higher if you need a wider view (e.g.
hunting through a band), or lower if your USB connection is
flaky and dropping samples (which shows up as gaps in the
waterfall).

The **General panel** (`Ctrl+1`) is where you'd change the
sample rate.

---

## Decimation

The SDR captures at 2 Msps. The demodulator probably wants the
data at a much lower rate — FM broadcast is happy at 250 ksps,
voice is fine at 48 kHz, APT subcarrier work at 11 ksps. Going
straight from 2 Msps to 11 ksps in one step would waste CPU on
filter taps that are mostly throwing samples away. **Decimation**
is the staged downsampling that bridges them: filter the wider
stream to remove energy outside the new band, then keep one
sample per N (the decimation factor).

### What you see in the UI

The General panel's **decimation** slider controls how
aggressively the source rate is decimated before reaching the
demodulator. Higher decimation = narrower IF (intermediate
frequency) view = lower CPU cost but less spectrum visible to
the demodulator. SDR-RS auto-picks a sensible decimation when
you change demod modes; you mostly leave it alone.

### When it matters

If you crank the spectrum-window-width FFT setting in the Display
panel and notice the waterfall becomes coarse-pixelated, you're
seeing the limit of the post-decimation IF stream rather than the
2 Msps source. The fix is usually to *lower* decimation (more IF
samples = more data for FFT) at the cost of CPU.

---

## Bandwidth (channel filter)

After decimation, the chain knows which frequency it's tuned to,
but the demodulator only wants ONE channel — not the whole
band. The **channel filter** is a narrow band-pass around the
tuned frequency that removes everything outside it before demod.

### Why this matters

It's almost always the most-important knob a beginner gets
wrong. **Different protocols use different bandwidths**:

| Use case | Typical channel BW | Demod |
|----------|-------------------|-------|
| FM broadcast | 200 kHz | WFM |
| FM repeater (voice) | 12.5 kHz | NFM |
| Aviation voice (AM) | ~10 kHz | AM |
| Single sideband (ham HF) | ~2.7 kHz | USB / LSB |
| CW (Morse) | ~500 Hz | CW |
| NOAA APT (weather sat) | 38 kHz | NFM |
| Meteor LRPT (digital weather sat) | 144 kHz | LRPT |

If you tune to FM broadcast at 200 kHz wide and listen on NFM
with 12.5 kHz channel filter, you'll hear distorted fragments —
the filter throws away most of the signal energy. If you tune to
ham voice at 12.5 kHz wide and listen on WFM with 200 kHz
filter, you'll hear faint voice plus a lot of adjacent-channel
hiss.

### Where you set it in the UI

**Radio panel** (`Ctrl+2`) → bandwidth row. The slider's allowed
range adjusts to the current demod mode (NFM caps at 25 kHz, WFM
goes up to 200 kHz, etc).

---

## Demodulation

The actual extraction of information from a modulated carrier.
SDR-RS implements 8 demods:

- **WFM** (Wide FM): broadcast FM. 200 kHz channels, plus
  optional deemphasis (US 75 µs / EU 50 µs to match the
  broadcaster's pre-emphasis).
- **NFM** (Narrow FM): voice FM, 12.5 kHz typical. Used by ham
  radio repeaters, public-service trunked systems (the audio
  layer at least), pagers, weather radio.
- **AM** (Amplitude Modulation): aviation voice, AM broadcast,
  some shortwave. Picks up a carrier's amplitude variations.
- **USB / LSB** (Upper / Lower Sideband): single-sideband voice,
  used on amateur HF bands. Only one half of the AM spectrum
  is transmitted, doubling spectrum efficiency. **Voice on SSB
  sounds like Donald Duck if you're tuned even 100 Hz off** —
  use the frequency-fine knob.
- **DSB** (Double-sideband): both sidebands without a carrier.
  Niche.
- **CW** (Continuous Wave): Morse code. Receiver injects a
  beat-frequency oscillator (BFO) so the on/off keying becomes
  audible tones. Bandwidth ~500 Hz.
- **RAW**: just outputs the IQ as audio. Doesn't sound like
  anything; useful for piping to an external decoder.

There's also a satellite-specialised path that doesn't appear in
the regular demod dropdown:

- **LRPT** (Low-Rate Picture Transmission): the QPSK digital
  channel from Meteor-M weather satellites. Not a general-purpose
  demod — it's a silent passthrough wired by the Satellites
  panel's auto-record flow when a Meteor pass starts (or via
  `Ctrl+Shift+L` for manual operation). The 144 kHz channel
  bandwidth shown later in the table refers to this path; you
  don't pick "LRPT" from the dropdown the way you pick WFM.

### How to know which to use

If you're on FM broadcast: WFM. Voice on aviation: AM. Voice on
amateur HF: USB above 10 MHz, LSB below 10 MHz (the convention).
Voice on amateur 2 m/70 cm: NFM. NOAA weather satellites: NFM
(the APT decoder takes the audio). When in doubt: try one, listen,
try another.

---

## Squelch

A volume gate that closes the audio output when the signal isn't
strong enough to be intelligible. Without squelch, listening to
an idle voice repeater is just continuous hiss — squelch makes the
hiss vanish until someone keys up.

### Variants

The Radio panel offers four:

- **Power squelch**: gates on raw signal-strength threshold (in
  dBFS). Set the threshold a few dB above your local noise floor.
  The classic, works for any modulation.
- **Auto-squelch**: tracks the noise floor automatically and
  gates above it. Set-and-forget for casual listening.
- **CTCSS** (Continuous Tone-Coded Squelch System): the audio gate
  opens only when a specific sub-audible tone (67 Hz - 254 Hz) is
  detected on the carrier. Used by many amateur repeaters to
  reject signals from other repeaters on the same frequency. If
  you know the repeater's CTCSS tone (Repeaterbook lists them),
  set it here and the squelch only opens for that repeater.
- **Voice-squelch**: detects the syllabic envelope of human
  speech and gates the audio when speech is present, irrespective
  of carrier level. Useful in noisy bands where power squelch
  would either let through hiss or cut off weak voice.

You can have only one type of squelch active at a time (the
panel enforces this). All of them only affect what you hear from
the speaker; the spectrum / waterfall / decoders see the
ungated signal.

---

## Auto-gain control (AGC) and the tuner

The RTL-SDR has a single tuner-gain control (in the General
panel). It's important because:

- **Too little gain**: weak signals get lost in the dongle's
  thermal noise floor. Image: dim waterfall, no carriers visible.
- **Too much gain**: strong signals overload the tuner's
  amplifier. Image: ghost images of the strong signal appearing
  at multiples of its frequency, "spurs" at every 1 MHz from a
  big FM station, loud broadband noise that tracks gain.

The dongle has both an "auto" gain mode and a manual gain stepped
in dB increments. **Auto often picks too low a gain in low-signal
environments** — manual at ~30 dB gain is a sane starting point
for general listening.

---

## SNR and the noise floor

The status bar shows a **signal-to-noise ratio** estimate (SNR)
in dB. This is the difference between current signal level and
the local noise floor.

- A very strong commercial FM station at close range: 50+ dB SNR.
- A clean repeater conversation: 20-30 dB SNR.
- Marginal voice still intelligible: 5-10 dB SNR.
- Below 0 dB SNR: the noise is stronger than the signal; you'd
  need to know what you're looking for to extract anything.

Most digital protocols have a hard threshold: above N dB they
decode flawlessly, below N dB they don't decode at all. NOAA APT
needs ~10 dB; LRPT needs ~5 dB more than that; SSTV is forgiving.

---

## Spectrum view: FFT

The spectrum plot is computed by **Fast Fourier Transform** on
windows of recent IQ samples. The Display panel exposes a few
knobs:

- **FFT size**: how many IQ samples per FFT window. Bigger =
  finer frequency resolution (more bins) but slower update and
  more CPU. 4096 / 8192 are typical.
- **Window function**: how samples are weighted at the FFT
  edges. Hamming / Blackman / Hann are different trade-offs
  between resolution and side-lobe rejection. Hann is fine
  for everything.
- **Frame rate**: how often the spectrum + waterfall redraw.
  60 Hz is buttery-smooth; 30 Hz saves some CPU. The decoders
  don't care — this only affects what your eyes see.

### What the FFT plot CAN'T tell you

It can't tell you the modulation type or the demod that'll
recover audio. A peak in the spectrum means "there's energy at
this frequency"; figuring out *what kind* of energy (FM voice?
AM aviation? data burst? jammer?) is what you do by tuning to
it and listening / decoding.

---

## Recording

Three things you can record from SDR-RS:

- **Audio (.wav)**: the demodulated audio output. Audio panel
  → "Start recording".
- **IQ (.wav)**: the raw IQ stream from the source.
  Reproducible offline replay later. Big files: at 2 Msps with
  8-bit IQ, that's 4 MB/sec on disk.
- **Pass artifacts**: NOAA APT (single PNG), Meteor LRPT
  (per-pass directory, one PNG per AVHRR channel), ISS SSTV
  (per-pass directory, one PNG per completed image). All
  triggered by the Satellites panel auto-record toggle.

Files land under `~/sdr-recordings/` with timestamped names.

---

## Why the activity-bar layout matches the SDR pipeline

The left sidebar's icon order isn't aesthetic — it's the
conceptual order of the signal chain:

```text
General      → choose the source (RTL-SDR / network / file)
Radio        → choose the demod and audio post-processing
Audio        → choose the output device
Display      → configure the spectrum view
Scanner      → automate channel-by-channel listening
Share        → expose your dongle to other clients
Satellites   → schedule auto-record on overhead pass
```

Each panel maps to one stage. If you're trying to figure out
where a control lives, ask "which conceptual stage is this?"
— the answer points at the right panel.

---

## Further reading

- [`getting-started.md`](getting-started.md) — first-signal
  walkthrough.
- [`apt-reception.md`](apt-reception.md) — NOAA APT weather
  satellite end-to-end.
- [`lrpt-reception.md`](lrpt-reception.md) — Meteor-M LRPT
  digital weather satellite.
- [`sstv-reception.md`](sstv-reception.md) — ISS SSTV.
- The [SDR++ wiki](https://github.com/AlexandreRouma/SDRPlusPlus/wiki),
  since SDR-RS is a port of SDR++; many concepts and best
  practices carry over verbatim.
- [rtl-sdr.com](https://www.rtl-sdr.com/) — the community blog
  for RTL-SDR users; reception reports, antenna builds, decoder
  tooling.
- Your country's amateur-radio regulator publishes a band plan
  showing which frequencies are used for what (US: ARRL band
  plan, UK: Ofcom, etc).
