# Receive your first Meteor-M LRPT pass

A walkthrough from "I have an RTL-SDR, the app installed, and I've
already received an APT image" to "I just decoded a multi-channel
weather satellite image off my own antenna." If you haven't done
APT first, start with [apt-reception.md](apt-reception.md) — most
of the antenna and ground-station setup is the same and that
guide explains it from scratch.

The whole thing takes about as long as the pass itself — 10–15
minutes of attended time once you're set up.

---

## What's different from NOAA APT

NOAA APT and Meteor-M LRPT both ride 137 MHz from polar weather
satellites about 800–900 km up, and the same antenna receives
both. Past that they have almost nothing in common at the
protocol level:

|                       | NOAA APT                            | Meteor-M LRPT                                  |
|-----------------------|-------------------------------------|------------------------------------------------|
| Modulation            | Analog AM-on-FM (audible)           | QPSK (digital, sounds like white noise)        |
| Channel bandwidth     | ~38 kHz                             | ~144 kHz                                       |
| Resolution            | ~4 km / pixel                       | ~1 km / pixel                                  |
| Channels              | 2 fixed (visible + IR side-by-side) | up to 6 AVHRR-style (each a separate image)    |
| Decoder               | Single FM → AM-envelope chain       | QPSK demod → Viterbi FEC → Reed-Solomon → JPEG |
| What goes to the disk | One PNG per pass                    | One **directory** per pass, one PNG per APID   |

In practice that means LRPT passes give you sharper imagery in
multiple spectral channels (visible-light, near-IR, thermal IR),
but the receive chain is much pickier about signal quality. Where
APT will give you something useful from a clip-lead antenna on a
balcony, LRPT really wants a properly tuned V-dipole and a notch
filter, and it's noticeably less forgiving on low passes.

---

## Antenna and notch filter

Same as NOAA APT — both satellites broadcast right-hand circular
polarisation at 137 MHz, so a single horizontal V-dipole receives
both with no retuning. See [the APT antenna and notch
sections](apt-reception.md#antenna) for the detail.

The same caveat holds: a clear horizon helps more than antenna
upgrades. LRPT is even more horizon-sensitive than APT because
the QPSK loop loses lock faster than the analog AM detector does
when SNR drops.

The FM broadcast notch filter is more important for LRPT than for
APT. APT's intermod artefacts show up as cosmetic banding in the
image; LRPT's show up as Viterbi un-locking and missing scan
lines, which can mean entire seconds of the pass produce nothing.
If you live within ~30 km of an FM broadcast tower and you're
serious about LRPT, get the notch.

---

## Active satellites (2026)

Two operational LRPT satellites are in the catalog by default:

- **METEOR-M 2** @ 137.100 MHz — degraded, intermittent imagery
  but passes are predictable and sometimes still produce usable
  output
- **METEOR-M2 3** @ 137.900 MHz — the daily-driver Meteor LRPT
  source; reliable multi-channel imagery on every pass

A third operational satellite, METEOR-M2 4, is **not** in the
catalog because Celestrak's GP API returns 404 for its NORAD ID
(it launched 2024 and a usable TLE was never published — see
issue #506 for the rationale). If you need it, hand-import a
TLE; the receive chain is identical.

---

## Your first pass

The auto-record flow is the same as APT: pick a pass, flip the
toggle, walk away. The differences are all in what lands on disk
and what the live viewer shows you.

### 1. Open the Satellites panel

Click the satellite icon in the left activity bar (or `Ctrl+7`).
The panel layout is the same as APT — Ground Station, TLE Data,
Recording, Upcoming Passes from top to bottom.

### 2. Set your ground station and refresh TLEs

Same as APT: type your US ZIP, or enter lat/lon directly, then
click ↻ next to **Last refreshed** if the TLE data is stale. The
upcoming-passes list will repopulate within a second or two.

Meteor passes happen on roughly the same cadence as NOAA — half
a dozen visible passes per day from a typical mid-latitude
location, more from higher latitudes. A good Meteor pass is
peak-elevation ≥ 25° just like APT.

### 3. Toggle auto-record

Flip **Auto-record satellite passes** on. The same toggle that
arms NOAA APT also arms METEOR-M passes — the recorder branches
on the catalog entry's protocol field internally. The "Also save
audio (.wav)" sub-toggle is honoured for APT but ignored for
LRPT (LRPT's demod is a silent passthrough — recording 10+
minutes of stereo silence at 144 ksps would waste ~170 MB per
pass for no benefit).

That's it for setup. The rest happens automatically when the
next eligible Meteor pass arrives.

### 4. Watch the live viewer build

5 seconds before AOS, the recorder takes over your radio:

- Tunes to 137.100 MHz (M 2) or 137.900 MHz (M2 3)
- Switches the demod to LRPT (a new mode in the dropdown — see
  the next section if you want to use it manually)
- Sets the channel bandwidth to 144 kHz
- Zeros the VFO offset
- Opens a non-modal **Meteor-M LRPT** viewer window alongside
  the main radio window
- Clears the canvas so back-to-back passes start fresh

For the first ~30 seconds you'll see the viewer's channel
dropdown sit empty and dimmed — the QPSK loop is locking onto
the carrier and the FEC chain hasn't yet decoded any image
packets. Once the first Application Process IDentifier (APID)
appears in the dropdown, the canvas starts filling top-to-bottom
with that channel's scan lines.

LRPT scan lines arrive at roughly 6 per second per channel.
Compared to APT's 2 lines per second on a single image, the
LRPT viewer fills noticeably faster — a 10-minute pass produces
~3600 lines per active channel.

The **channel selector** dropdown in the viewer's header bar
populates as the satellite's downlink reveals which AVHRR
channels are active on this pass. You can switch between them
freely at any time — the rendered surfaces are independent
per APID, so switching is instant and lossless.

> **TODO:** hero screenshot — a real LRPT image we received
> during overnight testing, ideally showing the channel selector
> dropdown populated with several APIDs.
> `docs/guides/images/lrpt-hero.png`

### 5. After LOS

When the satellite drops below the horizon, three things happen:

- **Per-channel PNGs are written** into a directory at
  `~/sdr-recordings/lrpt-METEOR-M-2-2026-04-25-143022/` (sat
  slug + local AOS timestamp). One file per APID — typical
  filenames are `apid64.png`, `apid65.png`, etc. The save
  toast reports how many channels landed.
- **The radio restores** to whatever frequency / mode /
  bandwidth / scanner state you had before AOS. If playback was
  off pre-AOS, it stops; if you were tuned to a different mode,
  you go back to it.
- **The viewer window stays open** so you can review the imagery
  before the next pass clears it.

The save is decoupled from the live viewer — even if you
dismissed the viewer mid-pass to free screen space, the LOS
save still produces the per-channel directory. The DSP keeps
decoding into a shared image buffer regardless of viewer
presence.

---

## Manual LRPT mode

The auto-record path is the recommended way to use LRPT, but
the demod is also exposed in the header dropdown for manual use:
**WFM / NFM / AM / USB / LSB / DSB / CW / RAW / LRPT**.

When you select LRPT manually:

- The IF chain switches to 144 ksps (the LRPT working sample
  rate)
- The "demod" itself is a silent passthrough — your speakers
  produce nothing
- The decoder runs against the post-VFO IQ stream as long as
  the mode stays LRPT
- `Ctrl+Shift+L` opens the LRPT viewer window, attaches the
  shared image, and the canvas fills as the decoder produces
  scan lines

This is useful if you've hand-tuned to a downlink Celestrak
doesn't predict (e.g. during a Meteor handover when one
satellite goes silent), or if you want to capture a pass
without the auto-record's 5-second pre-AOS lead-in. Closing
the viewer in manual mode doesn't stop the decoder — it just
hides the canvas. To fully stop, switch the demod mode away
from LRPT or stop the source.

---

## When things go wrong

LRPT's failure modes are different from APT's because the
decoder is digital — instead of analog noise creeping into the
image, you get binary "the FEC unlocked" outcomes.

### Channel dropdown stays empty for the whole pass

The QPSK loop never locked, or it locked but the Viterbi
decoder couldn't find a valid sync marker. Most common causes,
in order:

1. **Pass too low.** LRPT needs ~5 dB more SNR than APT to
   produce useful imagery. A 15° pass that gives APT a usable
   strip will give LRPT nothing decodable.
2. **No notch filter near a strong FM tower.** FM broadcast
   IMD raises the noise floor enough to break QPSK lock even
   when APT would have produced something.
3. **Antenna polarisation mismatch.** A vertical antenna nulls
   out 137 MHz polar-orbit downlinks — make sure the V-dipole
   is horizontal.

### "Pass complete, but no LRPT channels decoded"

The recorder armed and the radio tuned, but no CCSDS image
packets reached the JPEG decoder. Same root causes as the
empty-dropdown case — the recorder ran the full lifecycle and
the per-pass directory is created but stays empty.

### "Pass complete, but every LRPT channel was empty"

Stranger case: the demux saw APID metadata (so the FEC chain
locked at some point) but no actual pixel data made it through
JPEG decode. Usually means a very low-elevation pass where the
SNR fluctuated enough to lock briefly but not enough to sustain
useful packet decode. The per-pass directory is still created,
just empty.

### Image is striped with horizontal black gaps

Decoder lost lock partway through the pass — usually a building
or terrain on the horizon that briefly blocked the signal. The
already-painted lines stay where they were; the decoder
re-syncs when the signal returns and continues from the next
valid frame. Cosmetic only.

### Some channels decode and others don't

Normal — Meteor satellites can selectively transmit different
combinations of AVHRR channels depending on operational mode
and time-of-day. Don't expect all six APIDs on every pass; a
typical daytime pass produces three.

### Viewer window gets buried under the main window

Hit `Ctrl+Shift+L` again. The action raises the existing viewer
to the front rather than opening a duplicate.

### Auto-record toast says "couldn't create {dir}"

Check disk space and permissions on `~/sdr-recordings/`. The
shared image is still in memory — open the viewer, pick a
channel, and use the manual Export PNG button to write what's
been decoded so far to a different location.

---

## Next steps

- **NOAA APT** (epic [#468](https://github.com/jasonherald/rtl-sdr/issues/468))
  — analog 137 MHz weather sat. Same antenna works. If you
  haven't received an APT image yet, do that first — it's much
  more forgiving and the same flow.
- **ISS SSTV** (epic [#472](https://github.com/jasonherald/rtl-sdr/issues/472))
  — the International Space Station occasionally broadcasts
  SSTV images on 145.800 MHz during commemoration events.
  Different band, similar antenna, completely different
  decoder. Watch
  [ariss-sstv.blogspot.com](https://ariss-sstv.blogspot.com)
  for upcoming events.
- **False-colour composites.** AVHRR channels 1 (visible),
  2 (near-IR), and 4 (thermal IR) combine into useful
  false-colour imagery — vegetation in red, snow / ice
  highlighted, clouds easy to distinguish from snow. Today the
  app saves each channel as a separate greyscale PNG; combining
  them into RGB is a one-line `convert` (ImageMagick) or a
  ten-line Python script. A built-in composite mode is
  follow-up work.

If your first LRPT pass came through clean — multiple channels
populated, image fills the canvas top-to-bottom, no horizontal
gaps — congratulations. You're now decoding QPSK + Viterbi +
Reed-Solomon + CCSDS + JPEG end-to-end on commodity SDR
hardware, which is a mouthful and an accomplishment.
