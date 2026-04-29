# VDL Mode 2 (VHF Data Link Mode 2): Complete Implementation Reference

Receiving and decoding VHF Data Link Mode 2 on ~136 MHz using an RTL-SDR. VDL2 is the digital successor to VHF ACARS — higher bandwidth, proper networking stack (ISO OSI layers), carries modern data services including CPDLC and ADS-C. Decoding is harder than POA ACARS but well within reach.

---

## 1. Background

### 1.1 Why VDL2 Exists

ACARS over POA (Plain Old ACARS / VHF analog) tops out around **2400 bit/s** shared among all aircraft on a channel. As airline operational data volumes grew through the 1990s, this became painfully inadequate. ICAO developed **VDL Mode 2** (defined in ICAO Annex 10 Volume III) as a proper digital replacement:

- **31,500 bit/s** per channel (13× POA ACARS)
- **CSMA/CA** media access (aircraft detect channel use before transmitting)
- **Proper network layer** with routing, addressing, reliable delivery
- **AOA** (ACARS Over AVLC) — carries legacy ACARS traffic while also supporting new services

Rollout began in the early 2000s and (as of April 2026) is the primary VHF aviation data link in much of the world. Many aircraft operate VDL2 and POA ACARS simultaneously, using VDL2 where it's available and falling back.

### 1.2 Services Carried

VDL2 transports:

- **AOA (ACARS over AVLC)**: legacy ACARS messages tunneled through VDL2. Looks similar to POA ACARS but with much higher throughput.
- **CPDLC** (Controller-Pilot Data Link Communications): ATC clearances, altitude assignments, frequency changes — all the things a controller used to say on voice, now as text
- **ADS-C** (ADS-Contract): like ADS-B but point-to-point and on a timer/contract basis (used primarily in oceanic regions to supplement radar gaps)
- **Company operational data**: OOOI, telemetry, weather, crew messages
- **FANS 1/A** services: future air navigation system applications

### 1.3 Relationship to Other Data Links

| Data link | Band | Rate | Use |
|-----------|------|------|-----|
| **POA ACARS** | VHF (131 MHz) | 2.4 kbit/s | Legacy, still in use as fallback |
| **VDL2** | VHF (136 MHz) | 31.5 kbit/s | Primary modern VHF data link |
| **HFDL** | HF (2.9–21 MHz) | 300–1800 bit/s | Oceanic, polar regions (no VHF coverage) |
| **SATCOM (Inmarsat ACARS)** | L-band (1.5 GHz) | Variable, higher | Worldwide ocean coverage |
| **SATCOM (Iridium)** | L-band (1.6 GHz) | Lower | Polar + worldwide |

VDL2 has eclipsed POA ACARS where VHF is available. HFDL and SATCOM complement for oceanic gaps.

---

## 2. RF Setup

### 2.1 Frequencies

VDL2 channels in the aeronautical band:

| Frequency | Notes |
|-----------|-------|
| **136.975 MHz** | Primary VDL2 common signaling channel (CSC), worldwide |
| **136.725 MHz** | Secondary |
| **136.775 MHz** | |
| **136.875 MHz** | |
| **136.900 MHz** | |
| **136.925 MHz** | |
| **131.525 MHz** | Some regions |

**136.975 is the one to monitor first** — if VDL2 is in range of your receiver, this channel will be busy.

### 2.2 Tuning Parameters

| Parameter | Value |
|-----------|-------|
| Center frequency | 136.975 MHz (primary) |
| Sample rate | 2 MSPS from RTL-SDR; decimate internally |
| Signal bandwidth | ~25 kHz |
| Modulation | D8PSK (Differential 8-PSK) |
| Symbol rate | 10,500 symbols/sec |
| Bit rate | 31,500 bits/sec (3 bits/symbol) |
| Gain | 35–45 dB |

### 2.3 Multi-Channel Reception

Like acarsdec, **dumpvdl2** supports simultaneous multi-channel reception. At 2 MSPS you can capture 2 MHz of spectrum; the VDL2 channels at 136.725, 136.775, 136.875, 136.900, 136.925, 136.975 all fit within that window. Monitoring all of them from one dongle is the norm.

### 2.4 Antenna

Same aeronautical band as POA ACARS. Any 125–140 MHz capable antenna:

- **Quarter-wave vertical**: 55 cm at 136 MHz
- **Airband discone**: broadband, perfect
- **Commercial airband antennas** (Diamond D-130, Icom AH-7000, etc.)

Aircraft transmit at higher power on VDL2 than POA ACARS in some cases (to ensure reliability at long range), and ground stations are often higher-ERP than airline radio stations. Expect comparable or slightly better range than POA ACARS — 200–400 nm from high-altitude aircraft.

An LNA with 136 MHz bandpass filter helps in RF-noisy environments.

---

## 3. Signal Format

### 3.1 Modulation: D8PSK

**Differential 8-Phase Shift Keying**:

- 8 constellation points (3 bits per symbol)
- Differential encoding: each symbol's phase is relative to the previous, not absolute
  - Resolves carrier phase ambiguity
  - Receiver doesn't need to know absolute carrier phase
- Raised cosine pulse shaping, roll-off factor 0.6

Constellation:

```text
      010
  011     001
100         000
  101     111
      110
```

(Symbol-to-bits mapping is Gray-coded so adjacent phase errors cause 1-bit errors.)

Differential encoding: if you want to transmit bits `bbb`, the transmitted phase is `prev_phase + symbol_phase(bbb)`. On receive, the decoded symbol is the phase difference between consecutive symbols, which maps back to the original 3 bits.

### 3.2 Burst Structure

VDL2 transmissions are **bursts** (not continuous). Each burst:

```text
| Ramp-up | Synchronization | Transmission length | Header FEC | Data + FEC | Ramp-down |
| 5 sym   | 16 sym          | 17 bits             | 3 bits     | variable   | 2 sym     |
```

The **synchronization sequence** is 16 known symbols that the receiver correlates against to detect burst arrivals and establish symbol timing.

### 3.3 Forward Error Correction

VDL2 uses **Reed-Solomon (255, 249)** for data FEC, with interleaving. Corrects up to 3 byte errors per 255-byte block.

Plus **BCH(32, 26)** on the header. Short, fast, protects the transmission length announcement.

### 3.4 AVLC Frame Structure (Data Link Layer)

Above the physical layer, VDL2 uses **AVLC** (Aviation VHF Link Control), adapted from HDLC:

```text
| Flag | Address | Control | Information | FCS | Flag |
| 0x7E | 7 B     | 1-2 B   | variable    | 2 B | 0x7E |
```

- **Flag**: 0x7E, delimits frames (with bit-stuffing to prevent 0x7E in the middle of data)
- **Address**: source + destination, each 4 bytes (3 for ICAO address + 1 for address type/last-address)
- **Control**: HDLC control field (I-frame, S-frame, U-frame)
- **Information**: payload — typically carries CLNP, ACARS, or XID packets
- **FCS**: 16-bit CRC-CCITT frame check sequence

### 3.5 Upper Layers

AVLC carries one of:

- **CLNP/TP4**: proper ISO OSI network stack (used for ADS-C, CPDLC over the ATN)
- **ACARS over AVLC (AOA)**: legacy ACARS messages tunneled, same format as POA ACARS text but running over VDL2's transport
- **XID**: link management, aircraft registration to ground stations

For a hobbyist decoder, the most interesting content is AOA — same human-readable messages as POA ACARS, but arriving at 13× the rate.

---

## 4. Decoder Pipeline

```text
IQ samples → matched filter → symbol timing → differential decode →
symbol-to-bits → burst detection (sync correlation) → FEC decode (RS) →
AVLC frame extraction → bit destuffing → FCS check → content parse
```

### 4.1 Matched Filter

Root-raised-cosine with β=0.6 (matching transmit filter).

### 4.2 Timing Recovery

Gardner detector or similar. Converge on 10,500 sym/s exact timing.

### 4.3 Carrier Recovery (or not)

With D8PSK, absolute carrier phase doesn't matter — differential decoding eliminates phase offset. You still need frequency offset correction (frequency offset manifests as constant rotation of the constellation, which differential doesn't handle). A **frequency-only** PLL suffices.

### 4.4 Differential Decoding

For received complex samples `r[n]` at the symbol times:

```python
differential = r[n] * np.conj(r[n-1])   # phase difference
angle = np.angle(differential)          # in radians, [-π, π]

# Normalize to symbol index 0..7
symbol_index = int(round(angle * 8 / (2 * np.pi))) % 8
bits = gray_to_bits[symbol_index]        # Gray-decode to 3 bits
```

### 4.5 Burst Sync Detection

Cross-correlate the incoming symbol stream with the known 16-symbol sync sequence. Peaks in correlation indicate burst starts.

### 4.6 Reed-Solomon Decoding

**RS(255, 249)** over GF(2^8). Use a library — **libfec**'s `decode_rs_8()` or similar. Each 249-byte block of data has 6 parity bytes appended; decoder corrects up to 3 byte errors.

VDL2 interleaves the RS-encoded bytes across multiple blocks for burst-error resilience. De-interleave before RS decoding.

### 4.7 AVLC Frame Extraction

After FEC, you have a byte stream. Find `0x7E` flags to delimit frames. Between flags:

1. **De-stuff bits**: within frame data, any `0` bit inserted after 5 consecutive `1` bits is removed (bit-stuffing, inherited from HDLC)
2. **Parse fields**: Address (7-8 bytes), Control (1-2 bytes), Information (rest minus 2), FCS (last 2 bytes)
3. **Validate FCS**: CRC-16 over address + control + information

### 4.8 Content Parsing

Based on Control field and Information contents:

- **ACARS data** (identified by header pattern): parse as ACARS message (same as POA ACARS frame format)
- **XID frames**: link management, mostly operational/uninteresting
- **CLNP packets**: needs network-layer parsing to reach CPDLC/ADS-C content

---

## 5. Practical Tools: dumpvdl2

### 5.1 dumpvdl2

**The standard open-source VDL2 decoder**. Written by Tomasz "szpajder" Szewczyk, well-maintained, feature-complete.

Source: https://github.com/szpajder/dumpvdl2

Features:
- Takes RTL-SDR input directly (also Airspy, SDRplay, file input)
- Multi-channel simultaneous reception
- Decodes ACARS over AVLC (readable text)
- Decodes CPDLC (air traffic control messages, human-readable)
- Decodes ADS-C position reports
- Decodes XID link management
- Output: plain text, JSON, network forwarding (to acars_router, airframes.io, etc.)

Install on Arch: AUR (`dumpvdl2-git`) or build from source.

Usage:

```text
dumpvdl2 --rtlsdr 0 --gain 40 136725000 136775000 136875000 136900000 136925000 136975000
```

Output example:

```text
[2026-04-22 14:33:15 UTC] [136.975] [-15 dBFS]
AVLC:
 Src: C01234 (Aircraft), Dst: B01234 (Ground Station)
 I-frame, N(S)=2, N(R)=1, P/F=0
ACARS:
 Reg: .N12345  Flight: AA1234
 Label: H1  Block ID: 4
 Message: WX REQ KORD ARRIVAL RUNWAY 28C CONDITIONS
```

### 5.2 Comparison with Hand-Rolled

Writing your own VDL2 decoder is a **substantial** project — harder than POA ACARS and probably harder than ADS-B. Rough estimate: 2–4 weeks of focused work to match `dumpvdl2`'s reliability. The D8PSK demod, RS FEC, and AVLC frame parsing are all individually tractable but combining them reliably takes effort.

For learning, do the **physical layer** (demod + burst detection + RS decode) yourself; let `dumpvdl2`'s AVLC parser handle the upper layers. Or just use `dumpvdl2` end-to-end.

---

## 6. Integrating with Other Data Sources

### 6.1 airframes.io Feed

Like ACARS, VDL2 contributions are welcome at airframes.io. Configure `dumpvdl2` with the `--output network:...:airframes.io:5555` option. Your decoded messages contribute to a global research aggregator.

### 6.2 Combined ACARS + VDL2

Run both `acarsdec` and `dumpvdl2` simultaneously. `acars_router` merges their outputs into a unified stream. This gives you coverage of both data links from one antenna.

Some airlines use POA ACARS for some message types and VDL2 for others — coverage of both maximizes what you see.

### 6.3 Integration with ADS-B

Tail numbers from VDL2 (and ACARS) match tail numbers from ADS-B. Correlating the three streams:

- **ADS-B**: real-time position, altitude, velocity
- **ACARS/VDL2**: operational context (what's the flight doing, what are they talking to dispatch about)
- **Result**: a rich real-time aviation situational picture

`tar1090` and some community projects overlay ACARS/VDL2 labels onto ADS-B aircraft tracks, showing recent messages next to each aircraft icon. Very cool live display.

---

## 7. What You'll See

Typical observations at a decent receiving location:

- **High-altitude airline traffic dominates** VDL2 — regional carriers and small aircraft mostly use POA ACARS
- **Message rates**: hundreds to thousands per day with modest antenna
- **CPDLC is relatively rare** but visible — seeing actual ATC clearances in plain text is fun
- **ADS-C** almost entirely invisible over land (used for oceanic); you'd need SATCOM to see it
- **Company operational traffic** dominates — same kinds of content as ACARS but more of it

---

## 8. References

**Specifications:**

- **ICAO Annex 10, Volume III** — VDL Mode 2 specification. Paywalled through ICAO, but available in some libraries.
- **ARINC 631**: VHF Digital Link (VDL) Mode 2 Implementation Provisions
- **ARINC 750**: VHF Data Radio (VDR)

Community references:

- **dumpvdl2 README and wiki** — https://github.com/szpajder/dumpvdl2/wiki
- **sigidwiki VDL page** — https://www.sigidwiki.com/wiki/VHF_Data_Link_(VDL)
- **airframes.io About page** — explains the protocols

**Software:**

- **dumpvdl2** — https://github.com/szpajder/dumpvdl2
- **libacars** — https://github.com/szpajder/libacars (shared library for decoding ACARS, VDL2, HFDL, MIAM content; dependency of dumpvdl2)
- **acars_router** — https://github.com/sdr-enthusiasts/acars_router (merges acarsdec + dumpvdl2 streams)
- **docker-vdlm2dec** and **docker-acarshub** — containerized pipeline for serious logging

**Community:**

- **airframes.io** — https://airframes.io (primary global aggregator)
- **r/aviation**, **r/adsb**, **r/sdr** subreddits (discussion, troubleshooting)

---

## 9. Suggested Build Order

**Fast path to working receiver:**

1. Install `dumpvdl2` on Arch
2. Install `libacars` dependency
3. Tune to 136.975 MHz + companions (single RTL-SDR, multi-channel mode)
4. Run `dumpvdl2` with logging enabled
5. Feed to airframes.io
6. Watch decoded messages
7. Compare volume and content with your POA ACARS feed (spoiler: VDL2 will vastly outnumber POA in most regions now)

**Deeper understanding:**

1. Read `dumpvdl2` source (C, modular, readable)
2. Record IQ of 136.975 MHz for an hour
3. Implement matched filter + timing recovery for D8PSK
4. Implement differential decoding; visualize the constellation
5. Implement burst detection via sync correlation
6. Use libfec for RS decoding
7. Implement AVLC frame extraction and bit destuffing
8. Use `libacars` for content parsing
9. Validate against `dumpvdl2` on same recording

**Ambitious integrated project:**

1. Run dump1090 (ADS-B), acarsdec (POA ACARS), dumpvdl2 (VDL2) simultaneously
2. Build a correlation layer keyed on tail number
3. Web dashboard showing live aircraft map with recent message content displayed per plane
4. Historical archive with full-text search across received messages
5. This becomes a genuinely valuable research tool

---

*End of reference.*
