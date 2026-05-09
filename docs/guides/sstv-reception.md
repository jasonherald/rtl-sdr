# Receive your first ISS SSTV image

A walkthrough from "I have an RTL-SDR, the app installed, and I've
already received a NOAA APT pass" to "I just decoded an
SSTV-encoded photograph that an ISS cosmonaut transmitted from
orbit." If you haven't done APT first, start with
[apt-reception.md](apt-reception.md) — most of the antenna and
ground-station setup is shared across all the satellite-reception
flows in this app.

The big difference from APT and LRPT: **SSTV from the ISS is not
a regular daily occurrence.** The International Space Station only
transmits SSTV during scheduled ARISS (Amateur Radio on the
International Space Station) commemoration events, which (as of May
2026) happen 2-4 times per year and run for 2-5 days at a stretch.
You can't just arm the auto-record toggle on a quiet weekday and
expect imagery — you need to time it to an active event.

> **Frequency migration note (as of May 2026):** ARISS moved SSTV
> from the legacy 2 m amateur slot at 145.800 MHz to UHF 70 cm at
> **437.550 MHz** starting with Series 31 (April 2026). The catalog
> entry tunes there automatically. Older write-ups and YouTube
> videos still mention 145.800 — that's no longer current. Voice
> contacts and APRS still use the 2 m slot, but SSTV events are
> UHF until further notice.

---

## What you'll see

During an active ARISS event (as of May 2026), the ISS transmits
a rotation of ~12 photographs over a 2-5 day window. Each pass over
your location typically captures 1-3 of them, depending on the pass
duration and where the cycle was when the satellite came into view.

Modes vary per event. Recent series have used **PD120** /
**PD180** / **PD240** (640×496 colour) for longer slots, and
**Robot 36** (320×240 colour) for tighter event schedules — the
May 2026 Series 32 ("Cooperation in Space") is Robot 36. The
viewer's window-title subtitle shows which mode the current image
is being decoded as.

> **TODO:** hero screenshot — a real ISS SSTV image we received
> during an ARISS event, ideally showing the mode subtitle in the
> viewer header. `docs/guides/images/sstv-hero.png`

A typical ARISS event publishes a "see what others received" gallery
via the ARISS web blog. Receiving a clean image and submitting it
gets you a paper certificate, which is a fun keepsake.

---

## Knowing when to listen

The single most important step is timing — there's no point arming
auto-record if there's no active event. Check both:

- **[ariss.org/upcoming-sstv-events.html](https://www.ariss.org/upcoming-sstv-events.html)** —
  ARISS's primary event calendar. Lists upcoming series with start /
  end UTC times, frequency, and mode. Updated 1-4 weeks before each
  event.
- **[@ARISS_Intl on X](https://x.com/ARISS_Intl)** — short-notice
  announcements and last-minute schedule changes (e.g. event
  windows extended or postponed).
- **[amsat.org news](https://www.amsat.org/category/announcements/)** —
  cross-posts ARISS event announcements and sometimes adds
  technical notes.

Past events (as of May 2026) have run from a few hours to five
full days. Event windows are announced as continuous, but in
practice the ISS crew power-cycles the SSTV transmitter during
station activities so some passes during the window will be silent.
Plan to attempt several passes per day during an event rather than
relying on any one.

### Duty cycles change between events

ARISS Series 32 (May 8-12, 2026) ran on a **36-second ON / 2-minute
OFF** duty cycle — the SSTV transmitter keys for 36 seconds
(approximately one full Robot 36 frame), then sleeps for 2 minutes,
then keys again. A typical 7-10 minute pass over your station catches
**2-3 burst windows**, not the continuous transmission older guides
describe. That puts a hard ceiling on per-pass yield: at one image
per burst, a single pass produces **at most 2-3 images**, not the
12+ you might see referenced in older ARISS event documentation.

Other recent series have run different duty cycles, including
continuous transmission (Series 27) and 1-minute / 2-minute
splits (Series 30). The current series's duty cycle is documented
in the ARISS event flyer — always check the flyer for the active
series before assuming a continuous transmit.

**Practical implication for log analysis.** The auto-record path
emits a per-pass `SSTV pass summary` log line at LOS with three
counters: `vis_count`, `image_complete_count`, `lines_decoded`.
A successful Series 32 pass might log `vis_count = 3,
image_complete_count = 2, lines_decoded = 560` — three burst
detections, two complete images, the third burst truncated by LOS
or the duty-cycle OFF period before image completion. Use these
counters to distinguish "satellite was off / out of range"
(`vis_count = 0`) from "satellite was on but partial decode"
(`vis_count > image_complete_count`) — the diagnostic surface that
shipped in #648.

---

## Antenna

ARISS SSTV (as of May 2026) is on **437.550 MHz** narrow FM — that's
the 70 cm amateur band, **not** the 137 MHz weather-satellite band APT
and LRPT use, and **not** the legacy 2 m slot at 145.800 MHz that
older guides reference (see the migration note above). You need an
antenna that works at 70 cm; a horizontal V-dipole tuned for 137 MHz
APT is too far off-resonance to be useful at 437 MHz, and a
SAW-filter-front-end designed for the 137 MHz weather band will
heavily attenuate the UHF signal.

### Best: 70 cm vertical or 2 m / 70 cm dual-band

The ISS's onboard antennas are roughly omnidirectional and
**vertically polarised**, unlike weather satellites' RHCP. A
70 cm vertical (quarter-wave for 70 cm is ~17 cm, easy to build),
a 2 m / 70 cm dual-band whip, or a J-pole tuned for 70 cm is the
right match. Many ham handhelds ship with a stock dual-band
"rubber ducky" that works adequately for high-elevation passes —
take the antenna off the radio and connect it to the dongle.

### Acceptable: 2 m vertical or random wire

A 2 m vertical (designed for 145 MHz) operated at 437 MHz is
~3× higher than its design frequency — the antenna will exhibit
multiple lobes and uneven gain, so reception works on some passes
and not others. A simple wire at the right physical length
(quarter-wave ~17 cm, half-wave ~34 cm) outperforms a mismatched
commercial antenna for this specific frequency.

### Bypass the 137 MHz SAW + LNA chain

If your station has a SAW filter + LNA tuned for the 137 MHz
weather-sat band, **disconnect both for ARISS reception**. SAW
filters are passive bandpass; a 137 MHz SAW will attenuate the
437 MHz signal by 30+ dB. Plug the 70 cm antenna directly into the
RTL-SDR. Bias-T should be **off** — the LNA isn't useful at 70 cm
and would just draw current.

### Improvised: SDR's stock whip

The rubber-duck antenna that ships with most RTL-SDRs is roughly
quarter-wave at 145 MHz (it's tuned for the ~150 MHz commercial /
amateur range). Stand it vertical on a windowsill with sky view
and you'll get something on a 60°+ pass. Don't expect images on
low passes.

### What helps more than antenna upgrades

- **Clear sky to the south.** ISS orbits at 51° inclination, so
  passes track diagonally. From mid-latitude northern locations
  the highest-elevation pass arc happens with the satellite
  south of overhead. A south-facing window or balcony beats a
  better antenna in a basement.
- **Pass elevation ≥ 30°.** ISS SSTV needs more SNR than APT to
  decode cleanly because the SSTV demodulator is line-rate-locked
  rather than re-syncable per scan line. A horizon-grazing pass
  produces noise where APT would have given you a thin strip.

---

## FM broadcast notch filter

**Unnecessary** for current ARISS SSTV at 437.550 MHz UHF (as of
May 2026). Broadcast FM (88-108 MHz) is far enough below 437 MHz
that intermodulation products don't land in-channel.

A common pager / commercial-radio band sits around 460 MHz, which
is closer in frequency. Most stations don't see interference from
it but if your 70 cm passes are noisier than expected, scan with a
spectrum tool to confirm a clean band before suspecting the
decoder.

If you ever revert to monitoring legacy 145.800 MHz transmissions
(rare, but ARISS has flipped frequencies before), an FM-broadcast
notch helps marginally on the 2 m band.

---

## Active mission

The ISS catalog entry is already in the app (NORAD 25544, 437.550
MHz NFM), wired with the SSTV imaging protocol. No setup needed
beyond the standard ground-station entry.

The catalog entry doesn't gate on whether ARISS is currently
active — if you arm auto-record outside an event window the radio
will tune up, the viewer will open, and the recorder will produce
an empty per-pass directory (or skip the directory creation, see
[Troubleshooting](#troubleshooting)). That's harmless but
pointless; check the schedule first.

---

## Your first pass

The auto-record flow is the same as APT and LRPT — set ground
station, toggle on, walk away — but the timing differs because
you're triggering on a specific scheduled event rather than the
satellite's natural orbit.

### 1. Confirm the ARISS event is active

Check the schedule (see [Knowing when to
listen](#knowing-when-to-listen)). If we're inside the published
window, proceed. If not, come back when one starts.

### 2. Open the Satellites panel

Click the satellite icon in the left activity bar (or `Ctrl+7`).
The panel layout is the same as for APT and LRPT.

### 3. Set your ground station and refresh TLEs

Same as APT: type your US ZIP, or enter lat/lon directly, then
click ↻ next to **Last refreshed** if the TLE data is stale. The
upcoming-passes list will repopulate within a second or two.

ISS passes happen **far more often** than weather-satellite passes
— typically 4-6 visible passes per day from a typical mid-latitude
location, more from higher latitudes. ISS orbits at 51°
inclination so passes track diagonally rather than pole-to-pole;
your pass list will show ISS interleaved with NOAA / Meteor-M
passes.

### 4. Toggle auto-record

Flip **Auto-record satellite passes** on. The same toggle that
arms NOAA APT and METEOR-M LRPT also arms ISS SSTV — the recorder
branches on the catalog entry's protocol field internally. The
"Also save audio (.wav)" sub-toggle **is** honoured for SSTV
because the demod is audible NFM and the audio recording captures
the raw signal (unlike LRPT, where the demod is a silent
passthrough and audio recording is suppressed).

### 5. Watch the live viewer build

5 seconds before AOS, the recorder takes over your radio:

- Tunes to 437.550 MHz
- Switches the demod to NFM (you'll hear the SSTV warble through
  your speakers)
- Sets the channel bandwidth to 38 kHz (covers Doppler swing at UHF
  with headroom)
- Zeros the VFO offset
- Opens a non-modal **ISS SSTV** viewer window alongside the main
  radio window
- Clears the canvas so back-to-back passes start fresh

The viewer's window-title subtitle starts as **"Waiting for VIS"**.
Within the first ~30 seconds of receiving signal the SSTV decoder
locks onto a VIS header; the subtitle updates to **"PD120"** /
**"PD180"** / **"PD240"** depending on which mode is being
transmitted. The image then fills top-to-bottom at the mode's
native cadence:

- PD120: ~2 lines/sec for ~120 seconds
- PD180: ~1.4 lines/sec for ~180 seconds
- PD240: ~1 line/sec for ~240 seconds

Each ARISS image takes a substantial fraction of an ISS pass to
transmit, so a typical pass produces 1-3 complete images depending
on the mode and pass duration. The **Pause** and **Clear** buttons
in the header bar work the same as in the APT and LRPT viewers.

### 6. After LOS

When the satellite drops below the horizon:

- **Per-image PNGs are written** into a directory at
  `~/sdr-recordings/sstv-iss-2026-04-25-143022/` (catalog slug +
  local AOS timestamp). One file per completed image —
  `img0.png`, `img1.png`, etc. The save toast reports how many
  images landed.
- **The radio restores** to whatever frequency / mode / bandwidth
  / scanner state you had before AOS. If playback was off pre-AOS,
  it stops; if you were tuned to a different mode, you go back to
  it.
- **The viewer window stays open** so you can review the imagery
  before the next pass clears it.

If a pass came in mid-image (e.g. you started auto-record halfway
through the satellite's transmission of image #4), you'll get a
partial image — top half is the relevant scene, bottom half stays
black. That's normal; the next pass starts a new directory.

The save is decoupled from the live viewer — even if you dismissed
the viewer mid-pass to free screen space, the LOS save still
produces the per-pass directory. The DSP keeps decoding into a
shared image buffer regardless of viewer presence.

---

## Manual SSTV mode

Unlike APT and LRPT, **SSTV doesn't have a dedicated mode in the
demod dropdown** (as of May 2026). The SSTV decoder runs only
when the satellite recorder armed it for an ISS pass. To use the
viewer outside the auto-record path:

- `Ctrl+Shift+V` opens the SSTV viewer window even when no pass is
  in progress. The decoder is wired to NFM audio, so as long as
  you're tuned to a live ARISS SSTV signal (437.550 MHz NFM during
  a current event window) with a clean carrier, the decoder will
  produce frames. This is useful if the auto-record schedule
  missed a pass and you want to watch the next one
  manually.
- Switching the demod mode away from NFM stops decode entirely.
- Closing the viewer window doesn't stop the decoder — the shared
  image buffer keeps accumulating in the background until the
  recorder transitions out of the SSTV-aware state.

If you want to receive non-ISS SSTV transmissions (e.g. an HF SSTV
QSO on 14.230 MHz), the same `Ctrl+Shift+V` flow works once you've
tuned to USB and set the audio sample rate appropriately. HF SSTV
is out of scope for the V1 ISS SSTV epic but the underlying
slowrx decoder doesn't care about the source.

---

## Troubleshooting

### Pass directory was created but is empty

The recorder armed and the radio tuned, but no SSTV image
completed during the pass. Most common causes, in order:

1. **No active ARISS event.** Check
   [ariss.org/upcoming-sstv-events.html](https://www.ariss.org/upcoming-sstv-events.html).
2. **Pass too low.** SSTV's PD-family decoders are line-rate-
   locked rather than per-line resyncable; a horizon-grazing pass
   may carry signal but never sustain enough SNR for VIS detection.
3. **Antenna polarisation mismatch.** A horizontal antenna (e.g.
   the 137 MHz V-dipole used flat for APT) nulls vertically-
   polarised UHF signals. Lay the antenna vertical, or use a 70 cm
   vertical purpose-built for the band.
4. **Wrong frequency due to Doppler.** The recorder doesn't apply
   Doppler correction for SSTV (as of May 2026); on UHF 437.550 MHz
   the shift is roughly **±10 kHz** (3× larger than on the legacy
   2 m frequency). At the catalog's 38 kHz bandwidth that fits, but
   if you've narrowed the channel bandwidth, the Doppler can walk
   the signal out of the passband near AOS / LOS. Restore bandwidth
   to 38 kHz.

If the directory is created but empty, you'll see a toast
"Pass complete, but no SSTV images decoded — nothing saved to {dir}".

### Images are mostly noise / hue-shifted

The decoder locked onto a VIS header but the signal was too weak
to produce clean image data. Same root causes as the empty
directory above. Hue shifts (overall image tinted blue or yellow)
indicate the HEDR (header) frequency-shift detection picked up a
slight carrier offset — slowrx automatically compensates for this
during decode, so persistent hue shifts mean the carrier drifted
mid-image faster than HEDR detection could track.

### Image is striped with horizontal jumps

Sync lost mid-image. The decoder doesn't recover from sync loss
within an SSTV image (unlike APT, which re-syncs per scan line),
so the rest of that frame is lost. The next VIS header starts a
fresh image — wait for it.

### Subtitle says "Waiting for VIS" for the entire pass

The decoder never detected a VIS header. Either there's no signal
(see "empty pass directory" above), or the signal is present but
the VIS detector's tone-classification thresholds didn't fire.
The detector is bandlimited to the standard 1100 / 1300 / 1900 /
2300 Hz tones — if the satellite's audio chain has shifted those
tones (rare but possible during onboard equipment swaps), VIS
won't detect.

### Pass scheduled but nothing happens

Check the **Last refreshed** row in the TLE Data section. ISS
TLEs go stale faster than NOAA / Meteor-M because the ISS does
periodic reboost burns that change its orbit by tens of km.
Refresh weekly during an active ARISS event. Celestrak rate-
limits aggressive clients, so don't refresh more than once per
day.

### Auto-record toast says "couldn't create {dir}"

Check disk space and permissions on `~/sdr-recordings/`. The
shared image is still in memory — open the viewer with
`Ctrl+Shift+V`, optionally pause to inspect, and use the manual
**Export PNG** button to write what's been decoded to a different
location.

### Viewer window gets buried under the main window

Hit `Ctrl+Shift+V` again. The action raises the existing viewer
to the front rather than opening a duplicate.

---

## Next steps

- **NOAA APT** (epic [#468](https://github.com/jasonherald/rtl-sdr/issues/468))
  — analog 137 MHz weather satellite. Different band, different
  antenna ideally (horizontal V-dipole rather than vertical
  whip), much more frequent passes. See
  [`apt-reception.md`](apt-reception.md).
- **Meteor-M LRPT** (epic [#469](https://github.com/jasonherald/rtl-sdr/issues/469))
  — Russian polar weather satellite on 137 MHz with multi-
  channel digital imagery. The same horizontal V-dipole that
  receives APT receives LRPT. See
  [`lrpt-reception.md`](lrpt-reception.md).
- **HF SSTV.** The slowrx decoder works on any audio source, not
  just the ISS. Tune to 14.230 MHz USB during a busy weekend and
  you'll catch SSTV QSOs from amateurs around the world. The
  manual-mode `Ctrl+Shift+V` flow handles this; no auto-record
  scheduling because HF SSTV isn't tied to a satellite pass.
- **Send your image to ARISS.** After a successful event,
  [ARISS publishes a submission portal](https://www.spaceflightsoftware.com/ARISS_SSTV/index.php)
  for received images. Submit your best capture and you'll get a
  paper certificate by mail — a satisfying piece of physical
  proof that you decoded a digital image off a spacecraft.

If your first ISS SSTV pass came through clean — VIS subtitle
populated, image fills the canvas top-to-bottom in colour, no
horizontal jumps — congratulations. You just demodulated a
narrowband FM signal from low Earth orbit, ran it through a VIS
detector and a PD-family colour decoder, and produced a
photograph that a cosmonaut transmitted from inside the
International Space Station. That is, on balance, a deeply silly
thing to be able to do from a balcony with a $30 dongle. Enjoy it.
