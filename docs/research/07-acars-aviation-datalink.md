# ACARS (Aircraft Communications Addressing and Reporting System): Complete Implementation Reference

Receiving and decoding text messages between aircraft and ground stations on VHF aeronautical frequencies using an RTL-SDR. Pairs beautifully with ADS-B — where ADS-B gives you position/velocity/identity, ACARS gives you the content of the operational conversation: weather reports, ETAs, maintenance alerts, gate assignments, engine diagnostics, and occasional pilot free-text.

---

## 1. Background

### 1.1 What ACARS Is

ACARS is a data link system that lets aircraft exchange short text-based messages with airline operations, air traffic control, and ground handlers. Developed by ARINC in 1978 (yes, really — it's one of the oldest actively-used digital aviation data systems), it originally rode on VHF voice channels repurposed for data.

Types of messages sent:

- **OOOI reports** (Out-Off-On-In): taxi-out, takeoff, landing, arrival times
- **Position reports**: lat/lon every few minutes
- **Weather**: METAR/TAF requests and reports, pilot weather reports
- **Flight plans** and updates
- **Performance data**: fuel consumption, engine parameters
- **Maintenance**: ACMS (Aircraft Condition Monitoring System) auto-reports on engine health, faults
- **ATC messages**: CPDLC (Controller-Pilot Data Link Communications) — taxi clearances, altitude assignments
- **Free-text**: occasional human-typed messages between crew and dispatch

Typical message size: 100–200 bytes of text.

### 1.2 The Data Link Zoo

"ACARS" strictly means the **application layer protocol** — the message formats. The underlying RF transport can be several things:

| Transport | Frequency | Notes |
|-----------|-----------|-------|
| **VHF ACARS (POA — "Plain Old ACARS")** | 131.525, 129.125, 130.025, 130.425, 130.450, 131.550, etc. | 2400 bps MSK. What you decode with an RTL-SDR. |
| **VDL Mode 2** | 136.975, 136.9, etc. | 31500 bps D8PSK. Successor to VHF ACARS; higher capacity. |
| **HFDL** (HF Data Link) | 2.9–21 MHz | For oceanic/polar regions; out of RTL-SDR range without upconverter. |
| **SATCOM (Inmarsat/Iridium)** | L-band, 1.5 GHz | Decodable with RTL-SDR but challenging antenna-wise. |

For this guide, **VHF ACARS (POA)** is the primary target. VDL2 gets its own doc.

### 1.3 Why This Pairs Well with ADS-B

Your ADS-B decoder gives you: this aircraft is N12345, a Boeing 737, currently at 36,000 feet, heading 270°, 450 knots, at this lat/lon.

Your ACARS decoder adds: and it's sending "WX REQ KJFK" to its dispatcher, or "ETA 23:47Z GATE A22," or "ENG#2 N1 VIBRATION LVL 2 — MAINT ALERT."

Combining them gives you a real-time operational picture of the aviation system — position, intent, and conversation for thousands of flights. The correlation key is the aircraft registration/tail number (N-number, G-xxxx, etc.), which appears in both feeds.

---

## 2. RF Setup

### 2.1 Frequencies

Primary VHF ACARS frequencies in the US:

| Frequency | Primary use |
|-----------|-------------|
| **131.550 MHz** | Primary ACARS in the US (SITA network) |
| **131.525 MHz** | Secondary/backup |
| **130.025 MHz** | ARINC company |
| **130.425 MHz** | ARINC company |
| **130.450 MHz** | ARINC company |
| **129.125 MHz** | ARINC company |

In Europe:
- **131.725 MHz** (primary Europe)

Elsewhere varies. Most commercial airliners in the US will use 131.550 by default; a decoder listening there captures most traffic.

### 2.2 Tuning Parameters

| Parameter | Value |
|-----------|-------|
| Center frequency | 131.550 MHz (or others) |
| Sample rate | 48 kHz audio from FM demod; dongle at 2 MSPS, decimate |
| FM bandwidth | ~7.5 kHz (narrow FM, AM-compatible) |
| Modulation | Minimum Shift Keying (MSK), 2400 baud |
| Gain | 30–40 dB |

**Amplitude Modulation vs FM**: ACARS is technically AM — it's designed to coexist with voice AM on the aeronautical band. But the data is carried as a 1200/2400 Hz tone shifting within the AM envelope. **You can demodulate as either AM or narrow FM and still recover the subcarrier**; `acarsdec` uses AM-style amplitude demodulation.

### 2.3 Multi-Channel Reception

`acarsdec` and similar tools support **simultaneous reception on multiple channels** from a single RTL-SDR. At 2 MSPS, you can capture ~2 MHz of spectrum, which includes several ACARS frequencies. Decoding all of them in parallel gives much better coverage — one airline uses 131.525, another uses 131.550, etc.

Typical `acarsdec` invocation monitoring multiple channels:

```text
acarsdec -r 0 131.550 131.525 130.025 130.425 130.450 129.125
```

### 2.4 Antenna

Same as ADS-B's neighbor band. Anything that works at 120 MHz works at 130 MHz:

- **Quarter-wave vertical**: 57 cm element for 131 MHz
- **Airband discone**: broadband, covers 108–137 MHz plus more
- **Dedicated airband antenna** (commercial): Diamond D-130 or similar
- Your existing ADS-B antenna will underperform here (it's tuned for 1090 MHz) but may work for nearby aircraft

Aviation VHF transmitters are typically 25 W, with line-of-sight propagation. Expect 100–200 nm range from cruising aircraft.

---

## 3. Signal Format

### 3.1 Physical Layer

- **Carrier**: AM on aeronautical VHF
- **Subcarrier modulation**: MSK (Minimum Shift Keying), which is a form of 2FSK where the two frequencies are chosen so phase is continuous
- **Mark (binary 1)**: 1200 Hz
- **Space (binary 0)**: 2400 Hz
- **Bit rate**: 2400 bit/s
- **Bit duration**: 416.67 μs

(Note: some sources give this as 1200 Hz mark / 2400 Hz space — this is the audio subcarrier representation after AM demodulation. Others describe it as MSK with those two tones; they're equivalent.)

### 3.2 Character Encoding

ACARS uses **7-bit ASCII with odd parity** — each character is 7 data bits + 1 parity bit. The parity is checked per character; bad parity discards the character.

### 3.3 Frame Structure

An ACARS transmission consists of:

```text
| Pre-key  | Bit sync   | Char sync | SOH | Mode | Address | ACK | Label | Block ID | STX | Text              | Suffix | BCS       |
| tone     | `++++`     | `**`      | 0x01| 1 B  | 7 B     | 1 B | 2 B   | 1 B      | 0x02| variable, max ~220| ETX+ETB| 2 B CRC   |
```

Field breakdown:

- **Pre-key**: dead airtime keying up the transmitter (analog to an analog mic PTT)
- **Bit sync**: sequence of `0x2B 0x2B 0x2B 0x2B` characters — 32 bits of known pattern that lets the receiver lock bit timing
- **Character sync**: `0x2A 0x2A` — byte-level alignment
- **SOH** (Start of Header): `0x01`
- **Mode**: one character, type of message ("2" for category A aircraft-to-ground, "9" for ground-to-aircraft, etc.)
- **Address**: 7 characters — aircraft registration (e.g. ".N12345"). The leading dot is a separator.
- **ACK**: one character, acknowledgement of prior message ("NAK" if not acknowledged)
- **Label**: 2 characters, message type code (see below)
- **Block ID**: 1 character, sequential per-aircraft message counter
- **STX** (Start of Text): `0x02`
- **Text**: variable-length payload, up to ~220 characters
- **Suffix**: `ETX` (`0x03`) marks end of message; `ETB` (`0x17`) marks end of block in multi-block messages
- **BCS** (Block Check Sequence): 16-bit CRC-CCITT

### 3.4 Label Codes

The 2-character **label** tells you the message category. Common ones:

| Label | Meaning |
|-------|---------|
| `_d` (downlink) | Miscellaneous downlink |
| `Q0` | Link test |
| `5Z` | Airline-defined |
| `H1` | Message to/from crew |
| `14` | Uplink to CDU (Control Display Unit) |
| `16` | Free text |
| `B1` / `B2` / `B6` | Weather request/response |
| `B9` | Oceanic clearance |
| `OC` | Oceanic message |
| `A6` / `A8` | ATIS/weather |
| `BA` | PDC (Pre-Departure Clearance) |
| `RA` | Descent advisory |
| `M1` | Maintenance message |
| `QA–QZ` | Airline company messages |

There are hundreds of label codes in use. The **IATA/ARINC ACARS label list** documents official assignments; airlines also use proprietary labels.

### 3.5 Example Decoded Messages

```text
[2026-04-22 14:32:15] 131.550 MHz Level: -23 dB
.N12345 2 H1 B2 1
EXAMINED FUEL BURN; REQUEST CLEARANCE TO FL400 FOR EFFICIENCY

[2026-04-22 14:32:48] 131.550 MHz Level: -19 dB
.N54321 2 _d _ _
#DFB/FOB 0134.2/FF 2345/EPR 1.48/T 275

[2026-04-22 14:33:02] 131.550 MHz Level: -28 dB
.G-EUYA 2 QB 5
CGNRR1 DA:PARIS STDN 1820Z GATE D42
```

The first is a readable pilot-typed request. The second is an automated performance report. The third is airline company operational data (British Airways, based on G-EUYA registration).

---

## 4. Decoder Pipeline

```text
IQ samples → AM/FM demod → audio → MSK demod → bit recovery →
character framing → parity check → frame assembly → CRC check → parse → output
```

### 4.1 AM Demodulation

Envelope detection:

```python
import numpy as np
from scipy.signal import hilbert

# IQ samples → envelope (absolute magnitude)
envelope = np.abs(iq_samples)  # simplest AM demod

# Better: coherent detection with Hilbert-derived analytic signal
```

### 4.2 MSK Demodulation

MSK at 1200/2400 Hz is recoverable via multiple approaches:

**Approach 1: Zero-crossing rate**

Count zero crossings in sliding windows. A 2400 Hz signal has 4.8 zero crossings per millisecond; 1200 Hz has 2.4. Segment the audio into 416.67 μs bit-duration windows and classify by crossing count.

**Approach 2: Quadrature demod**

Mix with a 1800 Hz reference (midpoint between 1200 and 2400), lowpass, and look at the sign of instantaneous frequency deviation.

**Approach 3: Matched filter**

Build two reference tone-bursts (one at 1200 Hz for one bit duration, one at 2400 Hz). Correlate the audio with each; whichever correlation is higher wins.

For ACARS, zero-crossing or quadrature approaches work fine — SNR is usually high enough.

### 4.3 Bit Timing Recovery

MSK has convenient phase continuity between bits, and the bit sync sequence (`0x2B 0x2B ...`) gives 32+ bits of known pattern to lock onto. Standard approaches:

- **Gardner timing error detector**
- **Early-late gate**

For a hobbyist implementation: find the bit sync header by sliding-correlation against the known pattern; set your bit clock accordingly; sample at bit centers.

### 4.4 Character Alignment

After bit recovery, you have a bitstream. Characters are 8 bits (7 data + 1 parity), transmitted LSB-first. The `0x2A` character sync pattern gives byte alignment.

### 4.5 Parity Check

Each character should have odd parity (number of 1 bits is odd). Discard characters with even parity as corrupt.

### 4.6 Frame Assembly

Collect characters between SOH (`0x01`) and ETX (`0x03`). Parse fields according to the frame structure in section 3.3.

### 4.7 CRC Validation

The last 2 bytes are a **CRC-CCITT-16 (KERMIT variant)** over the frame (from Mode through ETX/ETB inclusive). Polynomial `0x1021` reflected (= `0x8408`), initial value `0x0000`. ACARS feeds bytes LSB-first, so the receiver folds the entire frame including the trailing 2-byte BCS through the same CRC and expects the register to read 0 on a valid frame. This is the variant `acarsdec` uses (`acars.c:159`: `crc = 0;`) and what the Rust port implements (`crates/sdr-acars/src/crc.rs`); an earlier draft of this section had it as the X-25 variant (`init = 0xFFFF`, MSB-first), which is a different CRC and would never validate ACARS frames.

```python
def crc_ccitt_kermit(data, initial=0x0000):
    """CRC-CCITT-16 KERMIT — LSB-first byte feed, reflected
    polynomial 0x8408. Yields 0 over `frame + bcs_lo + bcs_hi`
    for a valid ACARS frame."""
    crc = initial
    for byte in data:
        crc ^= byte
        for _ in range(8):
            if crc & 0x0001:
                crc = (crc >> 1) ^ 0x8408
            else:
                crc = crc >> 1
    return crc
```

(For the canonical KERMIT check string `"123456789"` with init=0, this returns `0x2189`. The published "0x8921" check value is the same number byte-swapped, reflecting the protocol's convention of transmitting BCS low byte first; ACARS uses the same low-byte-first wire order.)

### 4.8 Multi-Block Messages

Messages longer than ~220 characters span multiple **blocks**, each with its own frame. Indicated by `ETB` (`0x17`) at end of non-final blocks, `ETX` at the final. Reassembly is a matter of concatenating text across blocks with the same Block ID chain.

---

## 5. Practical Tools

### 5.1 acarsdec

**The standard open-source ACARS decoder**. Takes RTL-SDR input directly; handles multi-channel reception; outputs plain text, JSON, or network-forwarded frames.

Source: https://github.com/TLeconte/acarsdec

Install on Arch: AUR (`acarsdec`).

Usage:

```text
acarsdec -o 4 -j 127.0.0.1:5555 -r 0 131.550 131.525 130.025 130.425
```

Output (text mode):

```text
[#3 (L:-21 E:-) 2026-04-22 14:32:15.123 --------------------------------
Mode : 2 Label : H1 Id : 9 Ack : !
Aircraft reg: .N12345 Flight id: AA1234
No: D23A
[....MSG TEXT HERE....]
```

### 5.2 acarsdec with Acarsdeco2, PlanePlotter

Acarsdec can forward decoded frames to aggregators. **acarsdeco2** and **PlanePlotter** are Windows alternatives.

### 5.3 airframes.io

The community aggregator — https://airframes.io. Similar idea to ADS-B's FlightAware or ADS-B Exchange, but for ACARS/VDL2/HFDL/SATCOM:

- Feeders worldwide stream their decoded messages
- Site provides live dashboards, aircraft-specific message histories, research tools
- Free to feed, free to view

Submitting your local ACARS receptions to airframes.io makes your setup useful to a global research community.

### 5.4 Integration with ADS-B

Some people run **both** dump1090 and acarsdec on the same antenna feed (with bandpass splitter), producing a unified aircraft view that merges position + telemetry + text traffic. **tar1090** and **readsb** ecosystems have hooks for ACARS overlay.

---

## 6. What You'll See

Typical observations from a reasonable setup over a day:

- **Hundreds to thousands of messages** from various aircraft
- **Most messages are automated**: OOOI, position reports, telemetry, weather requests/responses
- **Some are human-typed**: dispatch updates, maintenance complaints, crew communications
- **Occasional oddities**: test messages, incorrectly formatted data, airline proprietary codes you won't be able to decode

Interesting patterns emerge:

- Airline-specific message formats (American, Delta, United each have distinctive telemetry)
- Peak message rates correlate with flights over your area
- Maintenance alerts reveal aircraft health statistics
- Pre-departure clearances give you timing of airport operations you can't see

---

## 7. Legal and Ethical

Same general principles as ADS-B and paging:

- **Reception is legal** in the US (and most other jurisdictions) as this is unencrypted aviation data
- **Redistribution** is mostly fine since there's no personal-privacy concern (it's corporate operational data), but some airlines may object to publication of proprietary message formats
- **Acting on** the information (e.g. contacting airlines about their operations) is inadvisable
- **Never transmit** on these frequencies unless you're a licensed aircraft operator

airframes.io has handled the legal landscape for years without issue — it's a well-established community resource.

---

## 8. References

**Specifications:**

- **ARINC 618**: Air/Ground Character-Oriented Protocol Specification (the ACARS core spec) — https://aviation-ia.sae-itc.com/standards (paywalled)
- **ARINC 620**: Datalink Ground System Standard and Interface Specification (message content)
- **ARINC 622**: ATS Data Link Applications Over ACARS Air-Ground Network

Paywalled, but community documentation fills in gaps:

- **sigidwiki ACARS page**: https://www.sigidwiki.com/wiki/ACARS
- **airframes.io docs**: https://app.airframes.io/about

**Software:**

- **acarsdec** — https://github.com/TLeconte/acarsdec (RTL-SDR direct)
- **dumpvdl2** — https://github.com/szpajder/dumpvdl2 (VDL Mode 2, sister project)
- **acars_router** — https://github.com/sdr-enthusiasts/acars_router (Dockerized pipeline)
- **acarshub** — https://github.com/sdr-enthusiasts/docker-acarshub (web UI for local message storage)

**Community:**

- **airframes.io** — https://airframes.io
- **r/acars** subreddit (smaller than r/ADSB but active)

**Label references:**

- Various community-maintained wikis. `acarsdec` ships a label table you can consult.

---

## 9. Suggested Build Order

**Fast path to working receiver:**

1. Install `acarsdec` on Arch
2. Point airband antenna (or any 131 MHz-capable antenna) skyward
3. Run `acarsdec -r 0 131.550 131.525 130.025 130.425 130.450 129.125`
4. Watch messages arrive
5. Register at airframes.io and configure feeding
6. Correlate with your ADS-B output — match tail numbers

**Build-your-own path:**

1. Record IQ on 131.550 MHz for an hour over a busy airport
2. Implement AM envelope demodulation
3. Implement MSK bit recovery at 2400 bps (matched filter approach recommended)
4. Find bit sync header → lock bit timing
5. Implement character framing with parity check
6. Parse SOH–ETX frames
7. Implement CRC-16 validation
8. Decode and display messages
9. Validate against `acarsdec` on same IQ
10. Extend to multi-channel simultaneous decoding

**ADS-B + ACARS integrated pipeline:**

1. Run both decoders in parallel on the same RTL-SDR (different channels, same antenna)
2. Tag messages with tail number
3. Build a correlation layer that joins ADS-B position reports with ACARS messages per aircraft
4. Web dashboard showing aircraft on map + recent message traffic
5. This is a genuinely cool personal project with no commercial equivalent

---

*End of reference.*
