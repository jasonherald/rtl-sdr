# Radiosonde Tracking: Complete Implementation Reference

Receiving and decoding weather balloon telemetry on 400–406 MHz using an RTL-SDR. Radiosondes broadcast GPS position, temperature, humidity, and pressure as they ascend through the atmosphere — you get a live map of atmospheric soundings from balloons launched worldwide twice a day.

---

## 1. Background

### 1.1 What a Radiosonde Is

A **radiosonde** is a small disposable instrument package carried aloft by a weather balloon. It measures:

- **Pressure** (via onboard sensor or computed from GPS altitude)
- **Temperature** (thermistor)
- **Humidity** (capacitive sensor)
- **Position and velocity** (GPS)

And broadcasts all of this via a VHF/UHF radio link on **400–406 MHz** (the "meteorological aids" band). A typical flight profile:

```text
Launch (ground)
    │ ~5 m/s ascent rate
    ▼
~30 km altitude (~100,000 ft), balloon bursts
    │ parachute descent
    ▼
Landing, typically 50–200 km downrange from launch site
```

Flight duration: ~90–120 minutes. The sonde transmits continuously from launch until the battery dies on the ground (hours later).

### 1.2 Launch Schedule

Radiosondes launch twice daily from ~1,300 sites worldwide at synoptic times:

- **00:00 UTC** and **12:00 UTC** (main launches)
- Some sites also at 06:00 and 18:00 UTC

In the US, the NWS operates ~70 upper-air stations (as of April 2026). You can see the full list at https://www.weather.gov/upperair/nws_upper. Nearest ones to **Christiansburg, VA** are likely Blacksburg, VA itself, Sterling, VA (Dulles), Greensboro, NC, or Roanoke-area WSR stations. Check current operating sites; the list changes.

### 1.3 Common Radiosonde Types

Different manufacturers use different modulation schemes. A decoder needs to handle each one. The big ones:

| Type | Modulation | Typical frequency | Notes |
|------|-----------|-------------------|-------|
| **Vaisala RS41** | GFSK 4800 baud | 400–406 MHz | Most common worldwide; used by US NWS |
| **Vaisala RS92** | GFSK 2400 baud | 400–406 MHz | Older, being phased out |
| **Graw DFM-17 / DFM-09** | GFSK | 400–406 MHz | Common in Europe (Germany) |
| **Meisei RS-11G / iMS-100** | GFSK | 400–406 MHz | Japan |
| **Meteomodem M10 / M20** | GFSK | 400–406 MHz | France |
| **MRZ-3MK** | GFSK | 400–406 MHz | Russia |
| **Intermet iMet-1/4** | GFSK | 400–406 MHz | Various (research/military) |

**In the United States, you'll almost exclusively see RS41s.** A good first decoder to implement or use.

### 1.4 Why It's Fun

1. **Live tracking**: watch a balloon ascend in real time on a map
2. **Recovery**: sondes are abandoned by the weather service after landing. Many hobbyists retrieve them. An RS41 is a working GPS/sensor module with a microcontroller — some people reprogram them for other purposes (APRS trackers, pico balloons).
3. **Community**: sondehub.org aggregates received telemetry from receivers worldwide, showing a global map of balloons in flight.
4. **Atmospheric science**: you get real atmospheric profile data — temperature, humidity, wind speed/direction up to 30 km — that meteorologists use for forecasting.

---

## 2. RF Setup

### 2.1 Tuning Parameters

| Parameter | Value |
|-----------|-------|
| Frequency band | 400.000–406.000 MHz (search across the band) |
| Typical frequencies | 400.5, 401.5, 402.5, 403.0, 404.0, 405.0 (varies by site) |
| Sample rate | 48–250 kSPS per sonde; RTL-SDR at 2 MSPS can monitor ~2 MHz of band simultaneously |
| Signal bandwidth | ~10–15 kHz (narrow GFSK) |
| Gain | 30–45 dB |
| Modulation | GFSK, various baud rates (2400, 4800, 9600) |

### 2.2 Wideband Scanning vs. Narrowband Tuning

Two strategies:

**Scan mode**: monitor the whole 400–406 MHz band at 2 MSPS, continuously FFT-scanning for signals and identifying/decoding each sonde type in parallel. This is what `radiosonde_auto_rx` does — impressively elegant, catches every sonde in range automatically.

**Targeted mode**: tune to a known frequency. Useful if you know a specific station's schedule and frequency.

For a new setup, start with `radiosonde_auto_rx` scan mode — it's trivial to configure and does the detection for you.

### 2.3 Antenna

A radiosonde transmitting from 30 km altitude is **line-of-sight visible from hundreds of km away**. With a modest antenna you can often receive sondes from 300–400 km when they're near apogee.

**400 MHz quarter-wave**: 17.6 cm element. Build from a panel-mount SMA connector and a stiff wire. Excellent for its price (near zero).

**Commercial options**: Diamond RH-series, any 70 cm (430 MHz) ham antenna — 400 MHz is close enough that amateur UHF antennas work well.

**For distance/weak signal**: a small yagi (3–5 elements) aimed at launch site direction gives +6–10 dB gain. Rotate by hand during the flight as the sonde drifts.

FM broadcast interference is less of a problem than for APT (different band), but an LNA with 400 MHz BPF still helps.

---

## 3. Signal Format — Vaisala RS41 (the common case)

Since RS41 is dominant in the US, covering it in detail. Other types follow broadly similar structures with different specifics.

### 3.1 Physical Layer

- **Modulation**: GFSK (Gaussian-filtered Frequency Shift Keying)
- **Baud rate**: 4800 bit/s
- **Deviation**: ±2.4 kHz
- **Center**: sonde's assigned frequency

Each bit is a frequency shift: +2.4 kHz above center = 1, -2.4 kHz below = 0 (or inverted — check the spec).

### 3.2 Frame Structure

RS41 transmits frames continuously at **2 Hz** (one frame every 500 ms). Each frame contains:

```text
| Header        | Frame       | CRC | Padding |
| 0x1016F8C4... | Varies      |     |         |
```

Frame header: `0x86 35 F4 40 93 DF 1A 60` (64 bits). This is the sync marker your decoder searches for.

Frames are Manchester-encoded and XOR-scrambled with a fixed PN sequence — you need to reverse both layers before parsing.

### 3.3 Frame Contents

Each frame contains multiple **sub-blocks**, each with a type byte, length byte, payload, and CRC-16. Known sub-block types include:

| Type | Content |
|------|---------|
| 0x79 | STATUS — frame number, battery voltage, sonde ID |
| 0x7A | PTU — pressure, temperature, humidity raw readings |
| 0x7B | GPSPOS — ECEF position, velocity (primary GPS data) |
| 0x7C | GPSINFO — satellites visible, HDOP, etc. |
| 0x7D | GPSRAW — raw ephemeris (not always present) |
| 0x7E | XDATA — auxiliary ozonesonde or other extension data |

### 3.4 Decoding the GPS Position Sub-block (0x7B)

Contains ECEF (Earth-Centered Earth-Fixed) coordinates as signed 32-bit integers in millimeters, plus velocity components. Decode:

```python
x_mm = struct.unpack('<i', block[4:8])[0]
y_mm = struct.unpack('<i', block[8:12])[0]
z_mm = struct.unpack('<i', block[12:16])[0]
x, y, z = x_mm / 1000.0, y_mm / 1000.0, z_mm / 1000.0

# Convert ECEF to lat/lon/alt
a = 6378137.0  # WGS84 semi-major
f = 1 / 298.257223563
e2 = f * (2 - f)

lon = math.atan2(y, x)
p = math.sqrt(x*x + y*y)
lat = math.atan2(z, p * (1 - e2))
# Iterate for better accuracy
for _ in range(5):
    N = a / math.sqrt(1 - e2 * math.sin(lat)**2)
    alt = p / math.cos(lat) - N
    lat = math.atan2(z, p * (1 - e2 * N / (N + alt)))

lat_deg = math.degrees(lat)
lon_deg = math.degrees(lon)
```

### 3.5 Decoding PTU (Pressure/Temperature/Humidity)

Raw sensor readings in the PTU block are **not** directly in physical units. They're ADC counts from the sensors. Converting to real pressure/temperature/humidity requires **calibration coefficients** that are transmitted once-per-minute in calibration frames (also within the sonde's data stream).

The calibration frames contain polynomial coefficients specific to that individual sonde. Applying them:

```python
# Temperature (simplified; real formula has more terms)
T_celsius = cal_coefs['T0'] + cal_coefs['T1']*raw_T + cal_coefs['T2']*raw_T**2 + ...
```

For a first-cut decoder, you can skip calibration and just output raw counts — the position data is the fun part anyway. The existing `auto_rx` tool applies full calibration.

---

## 4. Decoder Pipeline

```text
IQ samples → FM demod → bit timing recovery → sync marker search →
Manchester decode → descramble → frame parse → CRC check →
GPS position decode → track on map
```

### 4.1 FM Demodulation

Standard quadrature FM discriminator (same as APT section). Output is an audio-rate signal where "high" means the instantaneous frequency was above center.

### 4.2 Bit Timing Recovery

GFSK at 4800 baud. If your audio sample rate is 48 kHz, you have exactly **10 samples/bit**. A simple clock recovery:

1. Find zero-crossings in the FM output
2. Use them to align a symbol clock
3. Sample at the midpoint of each bit period

**Gardner** or **Mueller-Müller** timing error detectors are more robust but overkill for this SNR regime — signals are strong.

### 4.3 Sync Marker Search

Slide the RS41 sync marker `86 35 F4 40 93 DF 1A 60` through the bitstream. When matched, you know where frames start.

Handle bit inversion — if modulator polarity is wrong, you'll see the inverted sync word `79 CA 0B BF 6C 20 E5 9F`. Try both.

### 4.4 Manchester Decode

In Manchester, each bit is transmitted as a transition:
- `01` represents bit 1
- `10` represents bit 0

So your bitstream is twice as long as the actual data. De-Manchester by taking every other bit (with appropriate phase alignment — determined by sync word location).

### 4.5 Descramble

XOR with the RS41 scrambling sequence. The sequence is:

```text
0x96, 0x83, 0x3E, 0x51, 0xB1, 0x49, 0x08, 0x98, 0x32, 0x05, 0x59, ...
```

(Full 64-byte periodic sequence. Published in rs41 decoder source.)

### 4.6 CRC Validation

Each sub-block ends with CRC-16. The polynomial used is CRC-16/CCITT-FALSE (poly `0x1021`, init `0xFFFF`). Discard any sub-block with a bad CRC.

### 4.7 Track on Map

You now have lat/lon/alt every 500 ms. Render on Leaflet / OpenStreetMap. Easy.

---

## 5. Practical Path: Just Use radiosonde_auto_rx

Similar story to LRPT — there's a mature open-source tool that does everything well. `radiosonde_auto_rx`:

- Scans the full 400–406 MHz band automatically
- Identifies sonde type by signal characteristics
- Decodes all major types (RS41, RS92, DFM, M10, M20, iMet, Meisei)
- Uploads to SondeHub for global tracking
- Runs continuously in the background (systemd service)
- Sends email/Twitter notifications when sondes land near you (for recovery)

Source: https://github.com/projecthorus/radiosonde_auto_rx

Installation on Arch is straightforward via AUR (`radiosonde_auto_rx-git`) or manual Python setup.

For a learning project, still write your own — RS41 is a great target. For actual sonde chasing, use auto_rx.

---

## 6. SondeHub

**SondeHub** (https://sondehub.org) is the community-run tracker. Anyone running `radiosonde_auto_rx` can upload their received telemetry; the site aggregates data from thousands of receivers worldwide and shows live maps of:

- Every balloon currently in flight
- Predicted landing locations
- Historical flight paths
- Statistics (burst altitude, drift distance, etc.)

Even if you never build a receiver, SondeHub is worth bookmarking — it's genuinely fascinating to watch the atmosphere's data collection in real time.

---

## 7. Radiosonde Recovery

This is what makes radiosondes uniquely fun — they're free to recover.

### 7.1 Legal Status (US)

NWS radiosondes bear a **return address postcard**: "If found, mail back, postage paid." Most finders recycle them back to NWS. Recovering them yourself is legal — they're considered abandoned once they've landed.

Other countries may differ. In some EU countries, radiosondes are not technically abandoned and formal recovery is gray-area. Check local rules.

### 7.2 Finding Them

Once a sonde lands, its radio may keep transmitting on the ground for hours until battery exhaustion. You can:

1. **Predict landing** from trajectory (auto_rx does this live)
2. **Drive toward predicted landing zone** (typically 20–100 km from launch)
3. **Hunt the final signal** with a directional antenna (Yagi + attenuator for foxhunting)
4. Typically find the sonde hanging in a tree or in a field

### 7.3 Reprogrammability

Vaisala RS41 sondes have been fully reverse-engineered. The firmware is replaceable via their SWD header. Projects like **RS41ng** (https://github.com/mikaelnousiainen/RS41ng) turn them into:

- APRS trackers for amateur high-altitude balloons (HAB)
- Pico balloons (ultra-lightweight, can circumnavigate globe)
- General-purpose GPS telemetry platforms
- Weather monitoring beacons

---

## 8. References

**Decoders:**

- **`radiosonde_auto_rx`** — https://github.com/projecthorus/radiosonde_auto_rx
- **`RS`** by Zilog — https://github.com/rs1729/RS — standalone decoders for individual sonde types, excellent reference code
- **`dxlAPRS`** — older toolchain, still maintained

**Tracking:**

- **SondeHub** — https://sondehub.org (live map, tracker historian, predictions)
- **radiosondy.info** — alternative tracker, mostly Europe-focused

**Recovery community:**

- **r/RadiosondeRecovery** subreddit
- **SondeHub Sonde Hunters** — Discord and forums

**Sonde reprogramming:**

- **RS41ng** — https://github.com/mikaelnousiainen/RS41ng
- **DFM-17 open firmware** — similar efforts for Graw sondes

**Launch schedules:**

- **NOAA upper air**: https://www.weather.gov/upperair/nws_upper
- **Local launch times**: the sites at Sterling, Blacksburg, and Greensboro all launch at 00Z and 12Z

---

## 9. Suggested Build Order

If you want **to chase a sonde this week**:

1. Install `radiosonde_auto_rx` on Arch
2. Point your antenna at the sky during a launch window (12:00 UTC or 00:00 UTC = 7am or 7pm local for Eastern time)
3. Watch sondes appear on SondeHub under your callsign
4. Pick a sonde landing near you, drive to predicted landing zone, fox-hunt it
5. Bring it home — you now have a free weather station / hackable GPS tracker

If you want to **understand the protocol**:

1. Record IQ samples of an RS41 in flight (1–2 MHz bandwidth, 10 minutes)
2. Implement FM demod + bit timing → bitstream
3. Find the sync marker in your bitstream; confirm frame spacing at 500 ms
4. Write Manchester decoder + descrambler
5. Parse sub-blocks; validate CRCs
6. Decode the GPS ECEF → lat/lon/alt → plot on a map
7. Extend to other sonde types (rs1729's `RS` repo is the reference)

---

*End of reference.*
