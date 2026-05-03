# Getting started with SDR-RS

A first-time-user walkthrough. Aimed at someone who picked up an
RTL-SDR dongle, installed this app, and isn't sure what they're
looking at when they launch it.

You can be listening to your first FM-broadcast station in under
five minutes. Real signal-finding takes longer, but that's because
of the radio spectrum, not the software — once you know which
sidebar panel does what, the workflow is short.

---

## What you bought, and what it does

An **RTL-SDR** is a $30 USB device that started life as a European
TV tuner. Someone discovered the chip exposes raw radio samples
over USB, and it became the cheapest software-defined radio
receiver on the planet. It's not a transmitter; it can only listen.
It covers roughly 24-1700 MHz with one common gap, which is enough
to pick up:

- FM broadcast (88-108 MHz)
- Air-band aviation voice (118-137 MHz)
- Weather satellites (137 MHz)
- Amateur radio (multiple bands; 144 / 440 MHz are easiest)
- Pagers and trunked-radio systems (~150-470 MHz)
- The International Space Station's ham downlink (145.800 MHz)
- Plus the noise floor between all of those, which is its own
  kind of fun.

A **software-defined radio** moves the radio's "dial" and
"demodulator" out of dedicated hardware and into software. The
RTL-SDR sends raw I/Q samples (the digital representation of
whatever's on the antenna) to your computer, and SDR-RS does
everything past that — channel filter, FM/AM/SSB demodulation,
audio output, recording, decoding. That's why one $30 dongle can
listen to a dozen different protocols: the protocol logic is just
code.

If you care about the underlying concepts (sample rate, IQ,
decimation, why bandwidth matters), see
[`sdr-concepts.md`](sdr-concepts.md). You don't need any of it to
get a first signal working.

---

## What you'll need

- The RTL-SDR dongle, plugged in.
- An antenna. The little rubber-duck antenna that ships with the
  dongle works for FM broadcast and not much else. For weather
  satellites you'll want a horizontal V-dipole; for air-band a
  ¼-wave whip; for ham bands a tuned dipole. Antenna choice is the
  single biggest quality lever in this whole hobby.
- SDR-RS launched. (`make install` from the repo root, or pull a
  pre-built binary if one exists for your distro.)
- About 5 minutes if you're aiming at FM broadcast as a first win.

---

## The two-minute orientation

When the app launches you'll see four regions:

```
+=========================================================+
|  ▶ [ 100.000.000 Hz ]  WFM ▾   ━━━━ vol             ☰  |   ← header bar
+--+---------------------------+--+--+----------------+--+
|  |                           |  |  |                |  |
|🎚 |    spectrum + waterfall   |  |  |  transcript    |  |
|🛰 |    (the wiggly graph and  |  |  |  (when active) |  |
|🎵 |     scrolling thing)      |  |  |                |  |
|… |                           |  |  |                |  |
|  |                           |  |  |                |  |
+--+---------------------------+--+--+----------------+--+
|  status bar: SNR ▮▯▯▯  •  Sample 2.4 Msps  •  WFM 200kHz |
+----------------------------------------------------------+
```

- **Header bar** (top): play/stop, the digit-by-digit frequency
  display, the demodulation-mode dropdown, volume, and an app-menu
  hamburger. The most-used controls live here so you never have to
  scroll for them.
- **Left sidebar (icons)**: the **activity bar**. Clicking each
  icon switches the panel beside it between General / Radio /
  Audio / Display / Scanner / Share / Satellites. Think of it like
  VS Code's left rail. Each activity has different controls.
  **`F9` toggles the sidebar open/closed.**
- **Centre**: the spectrum plot (top half) and waterfall (bottom
  half). The spectrum shows what's being received *right now*; the
  waterfall shows the last few seconds, scrolling downward. Strong
  signals are bright; noise is dark.
- **Right sidebar (icons)**: transcript and bookmarks panels. Same
  pattern as the left, just on the other side.
- **Status bar** (bottom): live signal-to-noise readout, sample
  rate, current demod mode + bandwidth.

The left activity icons in order: General, Radio, Audio, Display,
Scanner, Share over network, Satellites. `Ctrl+1` through
`Ctrl+7` jumps to each.

---

## First signal: FM broadcast

The easiest possible win. FM broadcast is loud, ubiquitous, and
your stock antenna already kind of works at 100 MHz.

1. **Plug the dongle in.** SDR-RS should auto-detect it (Source
   should say "RTL-SDR" in the General panel — `Ctrl+1`).
2. **Type a station's frequency** into the header-bar frequency
   display. Click the digit you want to change and scroll, or just
   click and type. NPR / BBC / your favourite station — whatever
   you'd find on a car radio. Try **101.5 MHz** if nothing comes
   to mind.
3. **Pick the right demod**: WFM (Wide FM). It's probably already
   selected; if not, header-bar dropdown → WFM.
4. **Hit play** (▶ button, header bar) or press **Space**.
5. Adjust volume.

If you hear music, congratulations — you've used a software-defined
radio. If you hear static, the most likely culprits are antenna
(move it near a window, point it up) or you've tuned to a frequency
nothing's broadcasting on.

---

## What each panel does

The **activity bar** on the left switches between conceptual layers
of the radio:

- **General** (`Ctrl+1`) — choose a *source*. RTL-SDR (your
  dongle), TCP/UDP network IQ (someone else's dongle on the
  network), or WAV file playback (a recorded session). For first
  use, leave it on RTL-SDR.
- **Radio** (`Ctrl+2`) — *demodulation* and audio post-processing.
  Bandwidth, squelch, CTCSS, deemphasis, notch, noise blanker.
  This is where you'd dial in a narrow voice channel or knock down
  a hum.
- **Audio** (`Ctrl+3`) — output device selection (PipeWire on
  Linux, CoreAudio on macOS) and audio recording.
- **Display** (`Ctrl+4`) — spectrum/waterfall appearance: FFT
  size, framerate, dB range, colormap.
- **Scanner** (`Ctrl+5`) — sequential scan across bookmarked
  channels. Once you have a few favourite frequencies saved as
  bookmarks, the scanner cycles through them looking for activity.
- **Share over network** (`Ctrl+6`) — turn your machine into a
  network radio source for other clients (GQRX, SDR++, another
  copy of this app on your phone).
- **Satellites** (`Ctrl+7`) — pass prediction for NOAA APT,
  Meteor-M LRPT, and ISS SSTV. Auto-record on overhead pass.

**Right sidebar**:

- **Transcript** — when transcription is enabled, decoded speech
  shows up here. Useful for ham conversations, air traffic, etc.
- **Bookmarks** — your saved frequencies. Each bookmark stores
  not just the frequency but the demod mode, bandwidth, squelch,
  and audio settings, so recalling a bookmark restores the full
  listening setup.

---

## Concrete next things to try

Pick whichever sounds fun. They escalate roughly in difficulty.

### Easy, indoor-antenna-OK

- **Air-band voice** — tune to your nearest airport's tower
  frequency (lookup: `liveatc.net` or your country's aviation
  authority). Demod = AM. Bandwidth ~10-12 kHz. You'll hear clipped
  voice in classic ATC cadence: "Cessna 1234 cleared for takeoff
  runway 27 left." Air-band is one of the more reliably-active
  bands.
- **Public weather radio** (NOAA NWR in the US, similar elsewhere)
  — tune one of the seven NOAA frequencies (162.400-162.550 MHz).
  Demod = NFM. Bandwidth 12.5 kHz. Synthesised voice reading the
  forecast 24/7. Cheerfully boring, but a guaranteed signal to
  confirm your setup is working.
- **2-metre amateur repeaters** (144-148 MHz in the US). Quiet
  most of the time but bursty. Listen during commute hours for
  the best chance of activity. Demod = NFM. Use the **squelch**
  control in the Radio panel so you only hear audio when someone
  transmits.

### Medium, outdoor antenna helps

- **Trunked or pager systems** — POCSAG / FLEX paging on
  150-160 MHz, P25 trunked on 851 MHz. SDR-RS doesn't decode
  these yet (it's tracker work), but you can hear the digital
  warble.
- **NOAA APT weather satellites** — see
  [`apt-reception.md`](apt-reception.md). Image of cloud cover
  from a polar-orbiting satellite, end-to-end including the
  antenna and the satellite-pass-prediction setup.

### Harder, real antenna and patience

- **Meteor-M LRPT** — Russian polar weather sat, multi-channel
  digital imagery. Same antenna as APT, harder demodulator. See
  [`lrpt-reception.md`](lrpt-reception.md).
- **ISS SSTV** — the International Space Station occasionally
  beams photographs from orbit on 145.800 MHz during ARISS
  events. See [`sstv-reception.md`](sstv-reception.md).

---

## Things every newcomer wonders

### "Why is everything so noisy?"

The radio spectrum is mostly noise. Real signals are narrower than
they look in the spectrum view, and most of the time there's
nothing on most of them. The waterfall is doing its job by showing
you the noise floor honestly. Look for vertical bars (continuous
signals) or short bright dashes (bursts).

### "Why does my signal sound terrible?"

Most often: bandwidth wrong (in the Radio panel — air-band ATC
needs ~10 kHz, FM broadcast needs 200 kHz, an FM repeater needs
12.5 kHz), or demod wrong (FM broadcast on AM = silence,
NFM on WFM = quiet).

The Radio panel's **bandwidth** slider is the single most
common knob a beginner gets wrong.

### "Why doesn't my dongle pick up X?"

If X is below ~24 MHz: most RTL-SDRs can't go below that without
a special "direct sampling" mod or a separate upconverter.
Shortwave is unfortunately not in scope for stock hardware.

If X is above ~1.7 GHz: same story, hardware limit. Anything
satellite-related higher than that (Inmarsat / Iridium at
1.5-1.6 GHz is right at the edge and *technically* possible but
needs a sensitive antenna).

### "Why are the signals drifting / shifting frequency?"

RTL-SDRs have a small frequency error — typically tens of ppm —
because they use cheap crystals. For an unmodified dongle that's
within tolerance for casual listening; for satellite work you
sometimes need to apply a "PPM correction" in the General panel.
Most satellite passes work fine without it.

### "Can I record what I'm hearing?"

Yes. Audio panel → start a recording. Files land in
`~/sdr-recordings/`.

### "Can I share my dongle with another machine?"

Yes — Share over network panel (`Ctrl+6`). Turn it on, the other
machine can connect via TCP using SDR-RS, GQRX, or any
`rtl_tcp`-compatible client. Useful if you have one good antenna
on the roof but want to listen from your laptop.

---

## Where to go next

- [`sdr-concepts.md`](sdr-concepts.md) — the underlying ideas
  (IQ, sample rate, decimation, FFT, why bandwidth matters)
  explained against this app's own UI. Reading this lets you
  reason about *why* something isn't working, not just what to
  try.
- [`apt-reception.md`](apt-reception.md) — your first NOAA
  weather satellite image. The most achievable "wow" moment in
  the hobby.
- [`lrpt-reception.md`](lrpt-reception.md) — Meteor-M LRPT
  multi-channel imagery, once APT works.
- [`sstv-reception.md`](sstv-reception.md) — receive a
  photograph from the International Space Station during an
  ARISS event.

The radio hobby is deep and old. SDR-RS is one tool in it; the
broader community has decades of accumulated knowledge on
antennas, propagation, and protocols. Search terms like "amateur
radio band plan", "rtl-sdr cheat sheet", and your country's
regulator's website will keep you busy for as long as you want
them to.
