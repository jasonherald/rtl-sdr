# Receive your first NOAA APT pass

A walkthrough from "I have an RTL-SDR and the app installed" to "I just
received a satellite image off my own antenna." Aimed at a first-timer
who hasn't decoded weather satellites before. If you're brand new to
the app, [`getting-started.md`](getting-started.md) tours the activity
bar and gets a first signal up in under five minutes — worth doing
once before tackling a satellite pass.

The whole thing takes about as long as the pass itself — 10–15
minutes of attended time once you've got an antenna up.

---

## What you'll see

NOAA 15, 18, and 19 are polar-orbiting weather satellites in a
~100-minute orbit. They pass overhead a handful of times a day,
each pass lasting roughly 8 to 15 minutes from horizon to horizon.
While they're in view they continuously broadcast a 137 MHz analog
FM signal containing two side-by-side images — one visible-light
(daytime) or infrared (night), one infrared (always) — modulated
as audio onto the carrier.

A typical received image looks like this:

> **TODO:** hero screenshot — a real APT image we received during
> overnight testing. Replace this placeholder once we have one.
> `docs/guides/images/apt-hero.png`

The strip you see is what the satellite saw under it during that
pass — clouds, coastline, the curve of the Earth at the edges.
Two channels run side-by-side because each scan line carries one
visible and one IR sample of the same patch of ground; you read
them like two narrow vertical strips.

Around the edges sit telemetry "wedges" — eight grey bars per
channel that carry calibration data. The decoder uses them to
identify which AVHRR channel the satellite is currently
transmitting (visible / near-IR / thermal IR / etc.) and to
brightness-calibrate the image. They're also a quick visual SNR
check: clean horizontal banding = clean signal; warped or torn
wedges = something's off (see [When things go wrong](#when-things-go-wrong)).

---

## Antenna

NOAA APT is at 137 MHz with right-hand circular polarisation,
broadcast from ~850 km up. You don't need anything fancy to hear
it, but you do need *something* — a stock RTL-SDR with the rubber
duck antenna it shipped with will give you noise.

### V-dipole (recommended cheap option)

Two 53 cm wires forming a 120° V, hung horizontally with the
opening pointing roughly north or south (so the satellite's
ground track passes through the antenna's lobe). Total parts:
~1 m of wire, an SO-239 chassis mount or similar, ten minutes
with a tape measure.

> **TODO:** photo of the V-dipole setup. Hang it above any nearby
> metal (gutters, AC units, your laptop) — height matters less
> than clear sky in the direction the bird is moving.
> `docs/guides/images/apt-vdipole.jpg`

Wire length matters more than gauge. Cut the two arms to **53 cm
each**, measured from the centre feedpoint to the tip. The 120°
angle is approximate; the antenna isn't picky to within ±10°.

### Improvised alternatives

Anything that's roughly the right length and roughly horizontal
will produce *something*:

- A clip-lead pair on the RTL-SDR's MCX connector, arms taped to
  a window frame in a V — works, image will be noisy
- A telescoping rabbit-ears TV antenna, arms set to ~53 cm,
  spread to ~120° — works, slightly better than clip leads
- A handheld 2 m / 70 cm whip pointed at the sky — works for
  high-elevation passes only, lots of noise on low ones

You'll know your antenna is the limiting factor when the image is
visible but speckled with snow throughout, no matter how high the
pass.

### What helps more than antenna upgrades

A clear horizon. APT is line-of-sight, so a pass that goes
overhead at 70° from a balcony beats a pass that goes overhead at
70° from inside a basement. If you can put the antenna outside —
even just on a balcony rail — do that before spending money on a
better antenna.

---

## FM broadcast notch filter

This is the single biggest quality-vs-cost upgrade for APT and
the *only* piece of gear most people end up buying after the
SDR itself.

**The problem.** Commercial FM broadcast (88–108 MHz) is loud —
often 80+ dB above the noise floor — and the RTL-SDR's front-end
has limited dynamic range. The strong FM signals create
intermodulation products that fall right on top of the 137 MHz
APT band. You see them as wavy or wobbly horizontal banding in
the image, often correlated with audio content from a strong
local FM station.

**The fix.** Inline notch filter that drops 88–108 MHz by 30+ dB
while passing 137 MHz cleanly. Costs $20–30. Two products that
work:

- **Nooelec FM Bandstop Filter** — passive notch, plugs inline
  between antenna and dongle. Cheap and effective.
- **SAWbird+ NOAA** — adds an LNA tuned for the 137 MHz band on
  top of the notch. More expensive (~$50) but the LNA gain helps
  if your antenna is borderline.

**How to tell if you need one.** First pass with no filter — if
the image has horizontal wave patterns that look like distortion
*and* you live within ~30 km of an FM broadcast tower, that's
broadcast IMD and a notch will fix it. If your image is clean,
you don't need one.

---

## Your first pass

The app does the heavy lifting once you've told it where you
are. The flow:

### 1. Open the Satellites panel

Click the satellite icon in the left activity bar (or `Ctrl+7`).
The panel has four sections, top to bottom:

- **Ground Station** — your latitude / longitude / altitude
- **TLE Data** — the orbit predictions, refreshed from Celestrak
- **Recording** — the auto-record toggle
- **Upcoming Passes** — the next several passes for your location

### 2. Set your ground station

Type your US ZIP code into the **US ZIP code** entry and press
Enter. The app calls Zippopotam.us for lat/lon and
Open-Elevation for altitude, fills the three rows above, and
re-runs pass prediction. If you're outside the US, type lat/lon
directly into the Latitude / Longitude / Altitude rows — same
SGP4 maths either way.

Accuracy matters but not at the centimetre level. ±1 km gets you
pass times accurate to within a few seconds, which is plenty for
APT.

### 3. Verify a pass is coming up

The **Upcoming Passes** group lists the next several passes
above your minimum elevation threshold for the next several
hours. Each row shows the satellite name, AOS countdown,
duration, peak elevation, and downlink frequency.

Look for a pass with **peak elevation ≥ 25°** for the auto-record
threshold. Lower-elevation passes work but the image gets noisy
toward the horizons — you'll get a thin strip rather than a full
slice.

If the pass list is empty: click the ↻ (refresh) icon next to
**Last refreshed** to fetch fresh TLE data from Celestrak. The
predictor needs orbit elements no more than a few weeks old to
be accurate.

### 4. Toggle auto-record

Flip **Auto-record APT passes** on. The subtitle reads
"Tune to the satellite, start the decoder, save the image at
LOS." From here you can walk away — the recorder will:

- 5 seconds before AOS: tune the radio to the satellite's
  downlink (137.620 / 137.9125 / 137.100 MHz), set the channel
  bandwidth to 38 kHz, switch the demod to NFM, and start the
  source if it was stopped
- At AOS: open the live APT image viewer, zero the VFO offset,
  and start filling the canvas line-by-line as the pass
  progresses
- At LOS: save the image to PNG (filename includes satellite
  name + local timestamp), show a toast confirming the save,
  and restore your previous tune (frequency, VFO offset, mode,
  bandwidth, scanner state, playback state)

You'll get a status toast when the pass starts and a save toast
when the PNG is written.

### 5. Watch it build

The image fills top-to-bottom at exactly 2 lines per second —
that's APT's hard-coded line rate, baked into the protocol since
the late 1960s. A 12-minute pass produces ~1440 lines, which is
why APT images are tall and narrow.

Don't expect it to look meaningful for the first few minutes —
the satellite is low on the horizon and the image is mostly
noise. Once it climbs above ~20° elevation the picture cleans
up, peaks at the closest approach, and degrades again as it
sets.

The viewer auto-scales: zoom and pan with mouse wheel + drag if
you want to inspect detail while it's still building.

### 6. After LOS

The PNG lands in `~/sdr-recordings/` with a filename like
`apt-NOAA-19-2026-04-25-143022.png` (satellite slug + local
timestamp). The save toast shows the full path. Open it in any
image viewer.

That's it — you've received a satellite image.

---

## When things go wrong

A few common failure modes and what they look like.

### Image is mostly noise

Possibilities:

- **Pass too low** — peak elevation under ~15°. The signal is
  scraping the horizon and most of it is being blocked by terrain
  / buildings / atmosphere. Wait for a higher pass.
- **Antenna pointed wrong** — V-dipoles are roughly omnidirectional
  in azimuth, so this is rare, but a *vertical* antenna will null
  out APT (which is horizontally polarised once it reaches you, after
  the circular-polarised wave loses one rotation handedness on
  reflection). Lay the antenna horizontal.
- **No notch filter and you're near a strong FM broadcast tower** —
  see the [FM broadcast notch](#fm-broadcast-notch-filter) section.

### Image has wavy horizontal banding

FM broadcast IMD. Get a notch filter. The pattern often
correlates audibly with local FM content if you tune the radio
to the strongest local FM station and listen — same modulation
artefact reaches the image.

### Image is torn / has horizontal jumps

Sync detector lost lock momentarily, usually because the SNR
dropped below threshold (a building on the horizon, a sudden
fade). The decoder re-syncs on the next valid sync pulse but the
already-painted lines don't move. Cosmetic only; subsequent
lines are still in the right place.

### Image is bent / curved

Doppler shift exceeded the channel filter. We don't do Doppler
correction in v1 (the 38 kHz channel filter absorbs the ±3.5 kHz
shift), but if you've narrowed the bandwidth in the Radio panel
the curve can show up. Reset bandwidth to 38 kHz.

### Telemetry wedges look mangled

Bad SNR. The wedges are slow-changing greyscale bars, so a noisy
signal turns them into static. Match the visible appearance of
the wedges against the quality of the image — they're the
fastest visual SNR check available without a meter.

### Pass scheduled but nothing happens

Check the **Last refreshed** row in the TLE Data section. If
it's older than a few weeks, click ↻ to refresh — stale TLEs
produce wrong pass times. Celestrak rate-limits aggressive
clients, so don't refresh more than once a day.

### Auto-record toast says "PNG save failed"

Check disk space and permissions on `~/sdr-recordings/`. The
decoder kept the in-memory image around so you can still
hand-export from the viewer after fixing the underlying issue.

---

## Next steps

- **Meteor-M LRPT** (epic [#469](https://github.com/jasonherald/rtl-sdr/issues/469))
  — Russian polar weather satellites at 137 MHz, digital
  modulation, multi-channel imagery. Same antenna works.
  Shipped end-to-end — see
  [`lrpt-reception.md`](lrpt-reception.md) for the
  walkthrough.
- **ISS SSTV** (epic [#472](https://github.com/jasonherald/rtl-sdr/issues/472))
  — the International Space Station occasionally broadcasts
  SSTV images on 145.800 MHz during commemoration events.
  Different band, different antenna polarisation, completely
  different decoder. Shipped end-to-end — see
  [`sstv-reception.md`](sstv-reception.md) for the walkthrough.
  Watch [ariss-sstv.blogspot.com](https://ariss-sstv.blogspot.com)
  for upcoming events.
- **Calibration tuning** — if you're getting consistently good
  images, look at the AVHRR channel telemetry to identify
  which spectral channel the satellite is currently
  transmitting. NOAA rotates among visible / near-IR / thermal
  IR depending on day/night and season; the channel ID lives in
  the telemetry wedges and the decoder logs it.

If your first pass came through clean, congratulations — you've
just completed the same workflow that amateur radio operators
have been doing since the 1970s, except the protocol stack is
written in Rust now.
