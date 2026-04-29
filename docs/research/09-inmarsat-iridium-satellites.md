# Inmarsat and Iridium L-Band Satellite Reception: Complete Implementation Reference

Receiving L-band satellite signals from the top of the RTL-SDR's tuning range (1.5–1.6 GHz). Inmarsat STD-C gives you worldwide maritime safety broadcasts (weather warnings, EGC messages, distress alerts). AERO gives aircraft SATCOM messaging. Iridium gives pager-like ring alerts and (with reverse engineering) short data bursts. All decodable with an RTL-SDR and the right antenna.

---

## 1. Background

### 1.1 L-Band Satellite Services

**L-band** (1–2 GHz) is allocated internationally for mobile satellite services. Three major systems are accessible to hobbyists:

| System | Frequency | Orbit | Coverage | Primary use |
|--------|-----------|-------|----------|-------------|
| **Inmarsat** (3F, 4F, 5F, 6F series) | 1525–1559 MHz downlink | Geostationary (~35,786 km) | 4 satellites cover ~80% of Earth (excluding poles) | Maritime, aviation, emergency services |
| **Iridium** | 1616–1626.5 MHz | LEO (780 km) | 66 satellites, full global coverage including poles | Worldwide voice/data, pagers, IoT |
| **Thuraya** | 1525–1660 MHz | Geostationary | Middle East, Africa, Asia, Australia | Regional mobile satellite |

All three tune within RTL-SDR's range (R820T2 goes to ~1.766 GHz).

### 1.2 Why This Is Rewarding

- **Geographic scale**: receiving signal from a satellite 36,000 km away with a small antenna still feels a bit miraculous
- **Content is interesting**: STD-C broadcasts real maritime safety messages you'd see on a ship's bridge; Iridium ring alerts show satellite paging in action
- **Technically challenging**: much harder than HF/VHF hobby radio; you're operating at the edge of RTL-SDR's capability
- **Corroborates global news**: maritime SAR events, weather disasters, navigational hazards all appear in STD-C before (or alongside) mainstream coverage

### 1.3 Why It's Hard

- **Small signals**: geostationary satellites are far, low-Earth-orbit satellites are moving fast. Link budgets are tight.
- **Antenna gain required**: omnidirectional won't work. You need a dish, patch antenna, or helical for usable performance.
- **LNA nearly mandatory**: RTL-SDR's noise figure at L-band is inadequate without amplification.
- **RFI challenges**: 1.5 GHz is near GPS (1.575 GHz L1) and various terrestrial services; filtering matters.

---

## 2. Antenna and Frontend

This is the main hurdle. Get this right and everything else follows.

### 2.1 Inmarsat Antenna (Geostationary)

Inmarsat satellites are fixed in the sky (geostationary). You point at them once and they stay there.

**Options:**

- **Patch antenna** (cheap, small, ~8 dBic gain): Inmarsat-specific patches sold on eBay/Aliexpress in $10–30 range. Small square with SMA connector. Mount pointing at the visible Inmarsat satellite (elevation depends on your latitude).
- **Helical** (higher gain, ~12 dBic): 4-turn right-hand helix wound on a coffee can or PVC form
- **Dish with L-band feed** (best, ~20 dBic): old satellite TV dish repurposed with an L-band feedhorn. Significant project but gives excellent margin.

### 2.2 Iridium Antenna (LEO Constellation)

Iridium satellites are in low Earth orbit (780 km) and move — each is visible for ~8 minutes per pass. You need omni coverage or mechanical tracking.

**Options:**

- **RHCP patch antenna** (best): designed for Iridium's 1621 MHz band, ~5–8 dBic gain
- **Quadrifilar helix** (QFH): hemispherical pattern, works well for LEO with no steering
- **Commercial Iridium antenna** (from scrapped Iridium modems): these exist on eBay, usually include integrated LNA

### 2.3 LNA

L-band LNAs are nearly essential. Key specs:

- **Frequency range**: 1525–1625 MHz covers both Inmarsat and Iridium
- **Gain**: 20–30 dB
- **Noise figure**: < 1 dB
- **Bias tee power**: some LNAs are powered via bias tee from the SDR; RTL-SDR v3 supports this

Popular choices:

- **SAWbird+ GOES** (originally for GOES weather satellites at 1694 MHz, works fine for Inmarsat 1537 and marginal for Iridium)
- **Nooelec SAWbird+ Inmarsat** (purpose-built)
- **Nooelec SAWbird+ Iridium**
- Generic eBay 1.5 GHz LNAs — variable quality, buyer beware

Place LNA **at the antenna**, not at the dongle — cable loss at L-band is significant; even a short RG-58 run attenuates before amplification hurts.

### 2.4 Pointing

**For Inmarsat**: use a tool like dishpointer.com or satellite tracking apps to find azimuth and elevation of the satellite serving your region. From Virginia, **Inmarsat 4F3 (Americas)** is approximately:

- Azimuth: ~175° (south-southeast)
- Elevation: ~35°

Mount the patch on a fixed arm, dial in the angles, and leave it.

**For Iridium**: with a hemispherical antenna (QFH or RHCP patch pointing up), no tracking needed. Passes come and go.

---

## 3. Inmarsat STD-C

### 3.1 What It Is

**STD-C** (Standard-C) is Inmarsat's low-data-rate messaging service, primarily used by:

- Ships for **GMDSS** (Global Maritime Distress and Safety System) compliance
- **EGC (EnhancedGroup Call)** broadcasts: safety, search-and-rescue, meteorological warnings
- **LRIT** (Long Range Identification and Tracking) reports from ships

The EGC broadcasts are the most interesting to monitor — they're public safety information transmitted continuously in the clear.

### 3.2 Signal Parameters

| Parameter | Value |
|-----------|-------|
| Frequency (downlink to earth) | 1537.700 MHz (typical channels: 1537.70, 1541.45, etc., depending on region) |
| Modulation | BPSK |
| Symbol rate | 1200 symbols/sec |
| Channel bandwidth | ~5 kHz |
| Forward error correction | Convolutional, rate 1/2, constraint length 7 |

Specific frequency varies by region and satellite — check scytale.xyz or similar for current assignments. For Americas with Inmarsat 4F3, common channel is around **1537.70 MHz**.

### 3.3 Decoder: Tekmanoid / scytale-c

**tekmanoid STD-C Decoder** is the long-standing closed-source Windows decoder (free).

**scytale-c** is the open-source equivalent: https://bitbucket.org/scytalec/scytalec

Workflow with scytale-c:

1. Tune SDR to 1537.700 MHz
2. Demodulate BPSK (use GNU Radio flowgraph included with scytale-c, or SDR++ + pipe)
3. Pipe demodulated frames to scytale-c for Viterbi decoding and message parsing
4. Output: decoded messages with routing information

### 3.4 What You'll Receive

EGC messages include:

- **MetArea bulletins**: weather forecasts for oceanic regions
- **NAVAREA warnings**: navigational hazards (lighthouse outages, oil rig locations, military exercises)
- **Search and Rescue**: distress alerts, SAR coordination
- **Piracy warnings**: particularly in Indian Ocean, Gulf of Guinea
- **Ice reports**: polar regions
- **Satellite system messages**: Inmarsat infrastructure announcements

Sample decoded message:

```text
EGC Message
From: LES Burum (Netherlands) 
To: All ships in MetArea IV
Priority: SAFETY
Subject: Atlantic weather forecast

WARNING 1234
ATLANTIC NORTH 40N-50N 040W-060W
GALE FORCE WINDS 45KT EXPECTED
SEAS 6-8M
VALID 00Z 2026-04-23 FOR 24 HOURS
```

### 3.5 Writing Your Own STD-C Decoder

Stages:

1. **BPSK demod**: standard. Costas loop for carrier recovery, Gardner for timing, hard or soft bits out.
2. **Frame sync**: STD-C uses Unique Words (UWs) as sync markers — specific bit patterns
3. **Viterbi decoding**: rate 1/2 convolutional, constraint length 7. Use libfec.
4. **Descrambling**: STD-C uses a scrambling sequence
5. **Frame parsing**: STD-C bulletin board (BB) frames, with message reassembly across frames
6. **Character decoding**: ITA2 (5-bit Baudot) or IA5 (7-bit ASCII) depending on message type

The full stack is non-trivial (~weeks of work). Study `scytale-c` source.

---

## 4. AERO (Inmarsat Aviation)

### 4.1 What It Is

**AERO** is Inmarsat's aviation SATCOM service. Aircraft flying oceanic routes (or anywhere out of VHF range) use AERO for:

- ACARS messaging over satellite
- CPDLC when out of VDL2 range
- Voice (separate channels, not easily decoded)
- ADS-C position reports in oceanic regions

### 4.2 Signal Parameters

AERO has several channel types:

- **P channel** (Packet): unmodulated pilot + data bursts at 600, 1200, 10500 bit/s depending on C-band profile
- **C channel** (Circuit-mode): voice
- **R channel** (Random-access): aircraft-to-ground requests
- **T channel** (TDMA): scheduled data

For hobbyist decoding, the **P-channel at 10500 bps** (Classic Aero-H+) carries ACARS/CPDLC text and is decodable.

### 4.3 Tools

- **jaero** (open source, Windows/Linux) — https://github.com/jontio/JAERO — the standard decoder
- Output feeds into `acars_router` and airframes.io

### 4.4 Frequency

AERO operates on sub-channels within the 1545–1555 MHz range. Specific frequencies vary by Inmarsat satellite and channel assignment. jaero can scan and identify.

### 4.5 What You'll See

Similar to VDL2/ACARS content — OOOI reports, weather requests, CPDLC clearances — but from oceanic flights that VHF can't reach. An **evening of AERO monitoring might capture trans-Atlantic position reports from dozens of flights** you'd never see on VHF.

---

## 5. Iridium

### 5.1 What It Is

Iridium is a 66-satellite LEO constellation providing global voice/data coverage. Services include:

- Satellite phones
- Pager-like **Ring Alerts** (public)
- Short Burst Data (SBD) for IoT devices
- Iridium NEXT (newer satellites) adds broadband

### 5.2 Signal Characteristics

- **Frequency**: 1616–1626.5 MHz downlink
- **Modulation**: DQPSK at 25 ksym/s
- **Frame structure**: TDMA with 90 ms frames, multiple slots

Iridium L-band is one of the **strongest** signals at 1.6 GHz — the satellites transmit high power for pager/phone reception. With a simple patch antenna you can detect passes.

### 5.3 What's Decodable

**Ring Alerts** (public pager-like broadcasts) are the easiest target:

- Iridium broadcasts "you have a call" messages to all phones in a paging area
- These are unencrypted system messages
- Receiving them reveals the constellation's paging traffic

Short Burst Data (SBD) from IoT devices — some of these are transmitted in the clear:

- GPS tracker updates
- Ship AIS position reports relayed via Iridium
- Utility/industrial monitoring

**Voice and real user data are encrypted.** Ring alerts and system overhead are what you can actually see.

### 5.4 Tools

- **gr-iridium** — GNU Radio blocks for Iridium demodulation. https://github.com/muccc/gr-iridium
- **iridium-toolkit** — Post-processing, frame analysis, ring alert extraction. https://github.com/muccc/iridium-toolkit

Workflow:

1. Record IQ samples of 1626 MHz for several minutes (capturing multiple passes)
2. Feed to `gr-iridium`'s offline decoder → produces demodulated burst file
3. Feed bursts to `iridium-toolkit`'s `parse.pl` → classified frame types
4. Extract ring alerts via specific subcommands

### 5.5 What You'll See

- **IRA frames** (Iridium Ring Alerts): satellite ID, beam number, pager message. Not linked to specific phone numbers but occasionally revealing.
- **Satellite traffic statistics**: how many bursts per satellite, which beams active
- **SBD traffic**: binary data bursts from IoT devices, mostly opaque without decoders for specific products
- **Watching the constellation**: you can see satellites rise and set, mapping your local sky's Iridium coverage

---

## 6. Comparison Table

| Aspect | Inmarsat STD-C | AERO | Iridium |
|--------|---------------|------|---------|
| Antenna requirement | Patch or small helix pointed at GEO sat | Same as STD-C | RHCP patch or QFH, omni |
| Cost to set up | $30–80 | $30–80 | $30–80 |
| Signal strength | Moderate (geostationary) | Moderate | Strong (LEO) |
| Pointing required | Yes (fixed) | Yes (fixed) | No |
| Content interest | Maritime safety, weather | Aviation ops (oceanic) | System/paging traffic |
| Decoding complexity | Moderate | High | High |
| Free tools | scytale-c, tekmanoid | jaero | gr-iridium + iridium-toolkit |

---

## 7. Aggregation

**airframes.io** accepts AERO feeds (via jaero). Feeding contributes to global oceanic flight data.

As of April 2026, no equivalent public aggregator for STD-C that I'm aware of; the data is broadcast publicly but there's no central hobby feed. Some private researchers aggregate for weather/maritime analysis.

For Iridium, as of April 2026 some reverse-engineering communities share data; this remains a more research-oriented area than a polished "feed to community" setup.

---

## 8. Legal Notes

- **Receiving Inmarsat STD-C EGC**: explicitly legal — these are public safety broadcasts specifically designed to be received by any ship.
- **Receiving AERO**: legal in the US and most jurisdictions under the same principles as VHF ACARS/VDL2. Unencrypted aviation operational data.
- **Receiving Iridium**: reception is legal. Decoding and publishing content of **encrypted** voice or SBD traffic would be illegal, but system overhead (ring alerts, paging) is designed as broadcast and safe to decode.

Generally treat all of this as "interesting engineering artifacts, don't act on content, don't redistribute beyond known-public channels like airframes.io."

---

## 9. References

**Specifications:**

- **Inmarsat STD-C technical specs**: limited public availability; decoder projects rely on reverse engineering + community knowledge
- **Iridium**: commercial spec is proprietary; community documentation at https://github.com/muccc/iridium-toolkit/wiki
- **AERO**: ARINC 741 describes, paywalled

**Software:**

- **scytale-c** (STD-C open source) — https://bitbucket.org/scytalec/scytalec
- **tekmanoid** (STD-C Windows, closed source but free) — https://www.tekmanoid.com/
- **jaero** (AERO) — https://github.com/jontio/JAERO
- **gr-iridium** — https://github.com/muccc/gr-iridium
- **iridium-toolkit** — https://github.com/muccc/iridium-toolkit

**Hardware:**

- **Inmarsat patches**: search "Inmarsat patch antenna" on eBay/Aliexpress
- **Nooelec SAWbird+ LNAs**: https://www.nooelec.com/
- **Commercial Iridium antennas**: sometimes appear on eBay as scrapped modem parts

**Pointing tools:**

- **dishpointer.com** — web-based satellite azimuth/elevation calculator
- **SatSat** / **SDR++ plugins** — overlay satellite positions on SDR displays

**Community:**

- **r/RTLSDR, r/amateursatellites** subreddits
- **airframes.io forum** — AERO discussion
- **MUCCC** (Munich CCC) — Iridium research community, ongoing reverse engineering

---

## 10. Suggested Build Order

**Inmarsat STD-C (easiest, most rewarding):**

1. Buy an Inmarsat patch antenna and a SAWbird+ Inmarsat LNA ($40–60 total)
2. Mount patch at correct azimuth/elevation for your region's Inmarsat satellite
3. Connect: antenna → LNA → RTL-SDR (with bias tee to power LNA)
4. Tune to 1537.700 MHz (or your region's equivalent); verify you see signal
5. Install scytale-c or tekmanoid
6. Decode; start receiving maritime safety broadcasts
7. Watch for interesting content: pirate attacks, storms, SAR events

**AERO (aviation oceanic):**

1. Same antenna + LNA as Inmarsat
2. Use jaero to scan for AERO channels
3. Feed decoded messages to airframes.io
4. Correlate with oceanic flight trackers — you'll see trans-Atlantic routes

**Iridium (most challenging):**

1. Iridium patch antenna or QFH (omnidirectional)
2. Same LNA works
3. Record IQ samples of 1626 MHz for 30+ minutes
4. Offline-decode with gr-iridium
5. Classify bursts with iridium-toolkit
6. Extract ring alerts
7. Possibly join the muccc/iridium-toolkit community to contribute to ongoing research

**Hardware-first exploration:**

- If budget is tight: start with whichever LNA + antenna is cheapest and most available
- Inmarsat satellites being fixed makes that the easiest first target
- Iridium doesn't need pointing but needs more signal processing

---

*End of reference.*
