# Digital Trunked Radio (P25 / DMR / NXDN / TETRA): Complete Implementation Reference

Receiving and decoding digital land mobile radio (LMR) traffic — the public safety, commercial, and government radio systems that replaced the analog scanner era. Using an RTL-SDR to monitor control channels, track talkgroups, and (where unencrypted) decode voice traffic.

---

## 1. Background

### 1.1 The Shift from Analog to Digital Trunked

Through the 1990s and 2000s, most public safety agencies migrated from analog FM (what scanners traditionally received) to **digital trunked** radio systems. Reasons:

- **Spectrum efficiency**: trunking shares a pool of channels among many talkgroups
- **Audio quality**: digital vocoders sound consistent regardless of signal strength (until they fall apart entirely)
- **Features**: group calls, unit IDs, emergency alerts, data
- **Encryption**: increasingly common (though often unencrypted in many jurisdictions)

For a hobbyist with an RTL-SDR, this means two things:

1. **Analog scanner skills partially transfer** — listen to analog fire/EMS on VHF lowband, or ham radio, or NOAA weather, or FRS/GMRS
2. **Digital decoding requires specific protocol handling** — each system uses different modulation, framing, and vocoding

### 1.2 The Major Digital Standards

| System | Primary use | Modulation | Vocoder |
|--------|------------|-----------|---------|
| **P25 Phase 1** | US public safety | C4FM (4-level FSK) | IMBE |
| **P25 Phase 2** | US public safety (newer) | H-DQPSK (TDMA) | AMBE+2 |
| **DMR** (Mototrbo) | Commercial, public safety | 4FSK TDMA | AMBE+2 |
| **NXDN** (IDAS/NEXEDGE) | Commercial, utilities | 4FSK | AMBE+ |
| **TETRA** | Europe public safety / commercial | π/4 DQPSK TDMA | ACELP |
| **ProVoice / EDACS** | Legacy commercial | Various | IMBE |

In the **US**, P25 dominates public safety; DMR dominates commercial (taxis, security, utilities, some public safety); NXDN is less common but present in utilities.

In **Europe**, TETRA dominates public safety; DMR is widespread commercial.

### 1.3 Trunking — What It Means

Traditional (conventional) radio: one transmitter per channel, users manually pick channels.

**Trunked radio**: a pool of RF channels shared by many users/talkgroups. One channel is designated the **control channel** which continuously broadcasts which talkgroup is currently using which voice channel. When a user keys up, the system assigns an available voice channel and announces it on the control channel. Receivers monitor the control channel and follow their talkgroup to whatever voice channel it's on.

To monitor a trunked system, you need to:

1. Find and decode the **control channel** (tells you what's happening system-wide)
2. Tune rapidly to the **voice channel** assigned to a talkgroup of interest
3. Decode the voice channel's modulation + vocoder

This requires either:
- Two or more SDRs (one for control, one for voice channel following)
- A single **wideband SDR** that captures the whole trunked system's RF footprint simultaneously (e.g. an RTL-SDR at 2.4 MSPS can cover a 2 MHz slice — enough for small systems)

### 1.4 Legal Status

**United States**: receiving unencrypted public safety radio traffic is generally legal. FCC rules and ECPA (18 USC § 2511) explicitly protect scanning of public safety and some other services. Some states have laws against using scanners in moving vehicles, or against using intercepted content to commit crimes — consult your state law.

**Encrypted** traffic: decryption is illegal (Computer Fraud and Abuse Act + ECPA). Most systems use AES-256 or ADP when encrypted; these are infeasible to break and legally off-limits anyway.

**Acting on** intercepted information (e.g. responding to a 911 call you overheard) is at minimum a bad idea and potentially illegal depending on context.

**Virginia specifically**: scanner use is broadly legal; in-vehicle use is permitted with some restrictions. Act in good faith.

---

## 2. Finding Your Local Systems

Before any decoding, figure out what's on the air around you.

### 2.1 RadioReference

The essential resource: **radioreference.com**. It maintains a community database of radio systems, per county:

- System type (P25 Phase 1, P25 Phase 2, DMR, conventional, etc.)
- Control channel frequencies
- Voice channel frequencies
- Talkgroup lists with labels (PD dispatch, EMS, fire, schools, etc.)
- Encryption status per talkgroup

For **Montgomery County, Virginia** (where Christiansburg is), RadioReference's page will tell you exactly what to expect. Most of Southwest VA public safety is on the **Statewide Agencies Radio System (STARS)** (as of April 2026), which is a Motorola Astro P25 Phase 2 trunked system. County EMS and fire may also have local VHF analog or their own systems.

### 2.2 FCC License Search

For more exhaustive/authoritative info: **wireless.fcc.gov ULS (Universal Licensing System)**. Search by geographic coordinates to find every licensed transmitter nearby. This finds systems RadioReference may not have cataloged.

### 2.3 Spectrum Scanning

Plug your dongle into **gqrx** or **SDR++** and scan the major public safety allocations:

- **VHF high**: 150–174 MHz
- **UHF**: 450–470 MHz
- **700 MHz**: 763–776 MHz / 793–806 MHz (narrowband public safety)
- **800 MHz**: 806–824 MHz / 851–869 MHz (common trunked frequencies)

Digital signals have a distinctive waterfall appearance: narrow vertical bars, clean edges, bursty or continuous depending on system. The control channel of a trunked system is usually continuously keyed — a solid signal you can spot easily.

---

## 3. Modulation Details

### 3.1 P25 Phase 1 — C4FM

- **Modulation**: 4-level Continuous-phase FSK
- **Symbol rate**: 4800 sym/s → 9600 bits/s (2 bits per symbol)
- **Channel bandwidth**: 12.5 kHz
- **Vocoder**: IMBE (voice → 88 bits per 20 ms frame)
- **Error correction**: Golay + Hamming codes, extensive

Each symbol is one of four frequency deviations:

| Symbol | Bits | Deviation |
|--------|------|-----------|
| 0 | 01 | +1800 Hz |
| 1 | 00 | +600 Hz |
| 2 | 10 | -600 Hz |
| 3 | 11 | -1800 Hz |

A discriminator-based demodulator can recover C4FM as audio, then threshold into four levels. RTL-SDR handles P25 Phase 1 comfortably.

### 3.2 P25 Phase 2 — H-DQPSK / TDMA

- **Modulation**: Harmonized Differential QPSK (similar to π/4-DQPSK)
- **Symbol rate**: 6000 sym/s → 12000 bits/s
- **TDMA**: 2 slots per RF channel (doubled capacity vs Phase 1)
- **Vocoder**: AMBE+2
- **Channel bandwidth**: 12.5 kHz

Harder to decode than Phase 1 — requires a proper QPSK demod and TDMA slot handling. Dominated current US public safety migration; what STARS in Virginia is using.

### 3.3 DMR (Mototrbo)

- **Modulation**: 4FSK (similar to P25 Phase 1's C4FM but different filter/deviation)
- **Symbol rate**: 4800 sym/s → 9600 bits/s
- **TDMA**: 2 slots per 12.5 kHz channel
- **Vocoder**: AMBE+2
- **Error correction**: Trellis, BPTC, Hamming

DMR has three tiers:

- **Tier I**: license-free simplex (unlicensed in some regions)
- **Tier II**: licensed conventional
- **Tier III**: licensed trunked

Many commercial systems use Tier III for trunking; amateur radio uses DMR heavily on the Brandmeister and TGIF networks.

### 3.4 NXDN

- **Modulation**: 4FSK
- **Symbol rate**: 4800 (NXDN48) or 2400 (NXDN96 wider version)
- **Vocoder**: AMBE+
- **Used by**: utilities (water, power), some rail, taxis, small commercial

Less common than P25/DMR. OP25 decodes it.

### 3.5 TETRA (for reference)

- **Modulation**: π/4-DQPSK
- **Symbol rate**: 18000 sym/s → 36000 bits/s
- **TDMA**: 4 slots per 25 kHz channel
- **Vocoder**: ACELP
- **Error correction**: RCPC (Rate-Compatible Punctured Convolutional)

Used throughout Europe, some Asian/South American countries. Essentially zero deployment in North America. Mentioned here for completeness; rtl-sdr + `tetra-rx` or `TETRA-toolkit` can decode.

---

## 4. The Vocoder Problem

Every modern digital voice radio standard uses a **proprietary voice codec**:

- **IMBE** (P25 Phase 1): DVSI Corp. patent, expired 2016
- **AMBE+2** (DMR, P25 Phase 2): DVSI Corp. patent, some claims expired, others active
- **ACELP** (TETRA): various patents

Open-source decoders historically couldn't include vocoders due to patents. Options now:

1. **MBElib** — open-source software vocoder for IMBE/AMBE+2 by mbelib developers. Legal gray area; "clean room" implementation based on reverse engineering. Used by many decoder projects.
2. **DSD (Digital Speech Decoder)** — originally used mbelib; newer versions support hardware vocoder dongles
3. **DVSI AMBE3000 USB dongle** — official hardware vocoder, ~$100. Fully licensed, no legal worries.
4. **DSD-FME** (DSD fork, actively maintained) — uses mbelib by default

In practice: **mbelib works well**. The IMBE patent has expired so P25 Phase 1 is fully legal to decode with software vocoder. AMBE+2 (P25 Phase 2, DMR) remains patent-encumbered for some claims but enforcement against hobby decoders is essentially nil.

**For a completely clean conscience**: buy a DVSI dongle. It also gives you better audio quality.

---

## 5. The Practical Decoder: OP25 or DSD-FME

### 5.1 OP25

**OP25** is the most complete open-source P25 (Phase 1 and Phase 2) decoder. Full trunking support, multiple simultaneous control channels, talkgroup following, web UI showing live system activity.

Features:
- Decodes P25 Phase 1 and Phase 2
- Control channel decoding → talkgroup tracking
- Automatic voice channel tuning
- Encrypted passes are labeled but audio is not played
- Works with a single RTL-SDR for small systems (site bandwidth <2 MHz)
- Web dashboard showing active talkgroups, units, system stats

Install on Arch: from AUR (`op25-git`) or build from source.

Source: https://github.com/osmocom/op25

### 5.2 DSD-FME

**DSD-FME** (Digital Speech Decoder — Florida Man Edition, seriously) is an actively maintained DSD fork with broader codec support:

- P25 Phase 1 and Phase 2
- DMR (Mototrbo Tier II/III, Connect Plus, Capacity Plus, Hytera)
- NXDN (NXDN48, NXDN96)
- ProVoice, EDACS (legacy)
- YSF (Yaesu System Fusion, amateur)
- dPMR, D-STAR (amateur)

Input: can take IQ from an SDR, audio from a dongle, or UDP stream from other software (e.g. SDR++ ingests RF → DSD-FME decodes).

Source: https://github.com/lwvmobile/dsd-fme

### 5.3 Trunk Recorder

For **long-term system logging**: **Trunk Recorder** (github.com/robotastic/trunk-recorder) is designed to record entire trunked systems continuously. Every call, every talkgroup, with metadata. Requires multiple SDRs for large systems (one per RF channel pool). Used by broadcastify and openmhz to feed public live scanner streams.

### 5.4 Boatbod OP25 (community fork)

Boatbod's fork of OP25 is widely-used and well-maintained. If you're setting up a P25 decoder today, start here: https://github.com/boatbod/op25

---

## 6. Hardware Considerations

### 6.1 Single vs. Multiple SDRs

A typical P25 trunked site uses 5–20 RF channels spread across a few MHz. An RTL-SDR at 2.4 MSPS can simultaneously capture ~2 MHz of spectrum.

If the site's channels fit within 2 MHz: **one RTL-SDR is sufficient**. Software tunes digitally within the captured IQ stream.

If the site spans more than 2 MHz: **multiple RTL-SDRs needed**, each covering different slices.

For the STARS system in Virginia, channels span wider than 2 MHz at most sites, so a single dongle may only cover control channel + a subset of voice channels. Check RadioReference for channel spread at your nearest tower.

### 6.2 Airspy and Other Alternatives

For serious trunked monitoring, **Airspy Mini** (10 MSPS) or **SDRplay RSP1A** (10 MHz bandwidth) cover larger RF footprints. Two or three RTL-SDRs together also work.

### 6.3 Antennas

Digital is less forgiving of weak signals than analog. A good antenna matters:

- **VHF/UHF discone** — broadband, works 25 MHz to 1.3 GHz; excellent for scanning
- **800 MHz quarter-wave** — trivially cheap, optimized for the common trunking band
- **Log periodic** — directional gain if all towers are in the same direction

Outdoor mounted, above the roofline, with good ground plane — standard antenna hygiene applies.

---

## 7. Control Channel Decoding

Control channel is the fastest way to "understand" a system without decoding voice.

### 7.1 What the Control Channel Broadcasts

For P25:
- **Group voice channel update**: "Talkgroup 12345 is now active on frequency 855.2375 MHz"
- **Unit-to-unit call assignments**
- **System identification, site identification, WACN (Wide Area Communications Network) ID**
- **Adjacent site information** (for roaming)
- **Network status**: encryption in use, emergency calls, system alerts

### 7.2 Real-Time System View

With OP25 decoding just the control channel, you get a live display:

```text
Talkgroup    Unit      Duration    Encryption
45201        1138452   3.2 sec     No
45203        1138455   12.7 sec    Yes
45100        1139001   0.8 sec     No
...
```

Even if you can't decode voice (Phase 2 without vocoder, or encrypted), watching the control channel reveals the system's operational tempo — shift changes, incident escalations, multi-agency coordination.

### 7.3 Mapping Talkgroups

RadioReference lists talkgroup labels. Correlate with your decoder's output:

| Talkgroup | Label |
|-----------|-------|
| 45201 | Blacksburg PD Dispatch |
| 45203 | Blacksburg Fire Tactical |
| 45100 | Montgomery Co EMS |

(Example; not real.)

---

## 8. Writing Your Own Decoder

Trying to decode P25 or DMR from scratch is a **major** project — months of work at minimum. Reasons to do it:

- Deep understanding of TDMA + FEC + vocoders
- Learning exercise in real-world radio protocols

Reasons not to:

- OP25 and DSD-FME are mature, free, and better than you'll write
- The vocoders are patent-encumbered (for AMBE+2)

If you still want to: the stages look like:

```text
IQ samples → 4FSK/QPSK demodulation → symbol sync → frame sync (NAC/CC) →
deinterleave → FEC decode (Golay, BPTC, Trellis) → voice frame extraction →
vocoder → PCM audio
```

Each stage is a textbook DSP topic. The **Osmocom OP25** source is the best reference — well-commented Python + C/C++ mix, modular, follows the TIA-102 standard structure for P25.

---

## 9. Applications

### 9.1 Public Safety Monitoring

Live situational awareness of your local emergency services. Some hobbyists maintain web dashboards for their area.

### 9.2 Broadcastify Feeding

**Broadcastify** (broadcastify.com) accepts feeds of decoded public safety audio from community volunteers. Set up a decoder + audio streaming pipeline → contribute to public availability of scanner audio. (Check with local agencies — some are sensitive about this despite legality.)

### 9.3 OpenMHz

Similar concept, non-commercial, often with more complete talkgroup decoding: https://openmhz.com.

### 9.4 Research / Journalism

Investigative journalists sometimes use scanner traffic to track emergency events. OP25 + Trunk Recorder is a standard setup.

### 9.5 Understanding RF Infrastructure

Mapping out how your local public safety network is organized — which sites, which frequencies, which talkgroups — is a meditation on infrastructure you normally don't think about.

---

## 10. References

**Software:**

- **OP25** — https://github.com/osmocom/op25 (original)
- **Boatbod OP25** — https://github.com/boatbod/op25 (active maintained fork)
- **DSD-FME** — https://github.com/lwvmobile/dsd-fme
- **Trunk Recorder** — https://github.com/robotastic/trunk-recorder
- **SDRTrunk** — https://github.com/DSheirer/sdrtrunk (Java, broadest protocol support)

**Specifications:**

- **P25 (TIA-102 series)** — https://www.tiaonline.org/ (some free, most paywalled)
- **DMR (ETSI TS 102 361)** — https://www.etsi.org/standards (free)
- **NXDN** — https://www.nxdn-forum.com/ (specifications free after registration)
- **TETRA (ETSI EN 300 392)** — https://www.etsi.org/standards (free)

**Frequency databases:**

- **RadioReference** — https://www.radioreference.com/apps/db/
- **FCC ULS** — https://wireless.fcc.gov/UlsApp/UlsSearch/searchAdvanced.jsp

**Community:**

- **r/scanners** subreddit
- **RadioReference forums**
- **Ham radio Slack / Discord** servers (digital voice channels)

**Hardware vocoders:**

- **DVSI AMBE3000 USB** — https://www.dvsinc.com/products/a3000.shtml

---

## 11. Suggested Build Order

**For local public safety monitoring:**

1. RadioReference → identify your county's primary trunked system (for Montgomery Co VA, likely STARS P25 Phase 2)
2. Check RF channel spread at nearest site; confirm one RTL-SDR is sufficient (or plan for more)
3. Install Boatbod OP25 on Arch
4. Configure for your system (control channel frequency, system ID, talkgroup labels from RadioReference)
5. Point antenna at nearest STARS tower
6. Watch the control channel go; tune into specific talkgroups
7. Verify encryption status — some Virginia talkgroups are encrypted (state police tactical), many are not (routine dispatch, fire, EMS)

**For commercial / ham DMR:**

1. Use DSD-FME
2. Find active DMR frequencies (commercial: RadioReference; amateur: 2m/70cm DMR repeaters)
3. Tune, decode — simpler than trunked P25 for a conventional repeater
4. For amateur networks, listen to Brandmeister talkgroups globally

**For deep protocol learning:**

1. Pick ONE protocol (P25 Phase 1 is the simplest)
2. Read the OP25 source top to bottom
3. Write your own C4FM demodulator; verify symbol output against OP25
4. Implement frame sync and basic header decoding
5. Consider this step 1 of a year-long project; buy a DVSI dongle for vocoding

---

## 12. Coda

The shift from analog to digital trunked radio is, among other things, a shift in cultural accessibility. Analog scanners were consumer products for decades. Digital decoders require much more technical involvement — they're a hobby for people willing to set up OP25, manage SDR hardware, and troubleshoot TDMA frame sync.

That barrier is a feature and a bug. On one hand, it reduces casual interception. On the other, it raises the floor for transparency and community awareness of how our emergency services operate.

Do this project with respect for the people whose communications you're monitoring. They're often doing hard work. Never interfere, never publish identifying details, and treat the information as you'd want your own communications treated.

---

*End of reference.*
