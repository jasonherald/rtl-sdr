# POCSAG and FLEX Pager Decoding: Complete Implementation Reference

Receiving and decoding paging traffic on VHF/UHF paging bands using an RTL-SDR. Despite the 1990s feel, pagers are still widely used in hospitals, fire services, and IT operations — the protocols are simple, the decoders are well-understood, and the resulting text stream is a window into a slice of communication infrastructure most people don't know exists.

---

## 1. Background

### 1.1 Why Pagers Still Exist

Pagers remain entrenched in specific industries:

- **Healthcare**: hospitals use pagers because RF paging works in elevators, basements, and shielded areas where cell coverage fails. Also redundant to cell networks during outages.
- **Fire/EMS**: volunteer fire departments rely on pagers for station alerts. Works without infrastructure dependencies.
- **IT operations**: some data centers still page on-call engineers for critical alerts.
- **Restaurants**: table-ready pagers (short-range, typically 467 MHz).

The installed base is millions of devices worldwide, and new hardware is still manufactured.

### 1.2 The Protocols

Two protocols dominate:

| Protocol | Year | Baud rates | Capacity | Notes |
|----------|------|-----------|----------|-------|
| **POCSAG** | 1982 | 512, 1200, 2400 | ~10 msgs/min per channel | Simple, widespread |
| **FLEX** | 1993 | 1600, 3200, 6400 | ~200 msgs/min per channel | Motorola, higher throughput, adopted by large carriers |

POCSAG (Post Office Code Standardisation Advisory Group, UK origin) is older, simpler, and used by smaller operators (fire departments, hospitals). FLEX is used by the big commercial paging networks (e.g. Spok/USA Mobility).

### 1.3 Legal Status (US)

This is important and worth reading carefully.

Receiving and demodulating paging signals is legal under FCC rules — the airwaves are public. **Acting on intercepted content, or disclosing it, can run into ECPA (Electronic Communications Privacy Act, 18 USC § 2511)** depending on jurisdiction, intent, and whether the communication is "readily accessible to the general public."

Case law on unencrypted paging has generally held that it's not protected — these protocols are standardized and widely documented, and the FCC has explicitly noted pagers are not secure. But recent interpretations are sometimes more conservative. And separately, **HIPAA** applies to healthcare content: receiving a pager message containing patient information doesn't violate HIPAA on your end, but redistributing it may. The safe posture:

1. Treat pager decoding as a **spectrum analysis / protocol research** hobby
2. **Do not publish** received messages, even anonymized
3. **Do not act** on information received (e.g. respond to someone else's page)
4. **Do not target** specific pagers or persons
5. If you inadvertently receive health/personal info, discard it

Consult a lawyer if this matters for your specific situation. The below is not legal advice.

---

## 2. RF Setup

### 2.1 Frequencies

Paging occupies several bands. In the US:

| Band | Use |
|------|-----|
| **152–159 MHz** | VHF paging, common for fire/EMS and hospitals |
| **454–460 MHz** | UHF paging, common for commercial |
| **929–932 MHz** | 900 MHz band, heavy commercial FLEX traffic |
| **35–36 MHz** | Low-band VHF, older/rural systems (some fire services) |

Active frequencies vary by region. To find local ones:

1. **radioreference.com** — community-maintained database. Filter by county and "paging" services.
2. **FCC ULS search** (wireless.fcc.gov) — find licensed paging transmitters near you by geographic search
3. **Spectrum analysis** — scan the bands with your dongle (gqrx, SDR++) and look for the characteristic POCSAG/FLEX signals (periodic bursts of digital data, usually with bell-like audio signature)

For **Christiansburg, VA**, RadioReference's Montgomery County page will list current paging frequencies for local fire and EMS.

### 2.2 Tuning Parameters

| Parameter | Value |
|-----------|-------|
| Center frequency | Specific paging frequency (varies) |
| Sample rate | 22050 Hz audio (from FM demod) is standard for multimon-ng |
| Signal bandwidth | ~15 kHz (narrow FM) |
| Gain | 30–40 dB |
| Modulation | Direct 2FSK or 4FSK on FM carrier |

### 2.3 Antenna

Any antenna reasonable for the chosen band:

- **VHF (150 MHz)**: quarter-wave = 50 cm. A simple vertical works well; pager signals are strong (typically 250W transmitter).
- **UHF (460 MHz)**: quarter-wave = 16 cm.
- **900 MHz**: quarter-wave = 8 cm. A small telescoping whip tunes broadly here.

A discone antenna (broadband 25 MHz–1.3 GHz) works for all paging bands at once — good for exploration.

Paging transmitters are often high-power with wide coverage, so antenna demands are low. A basic setup receives from tens of km away.

---

## 3. POCSAG Protocol

### 3.1 Physical Layer

- **Modulation**: 2-FSK (two-tone frequency shift keying)
- **Deviation**: ±4.5 kHz
- **Frequency deviation for "1"**: -4.5 kHz (below center)
- **Frequency deviation for "0"**: +4.5 kHz (above center)
- **Baud rates**: 512, 1200, or 2400 bits/sec (transmitter picks one)

### 3.2 Structure

Transmission is organized as:

```text
| Preamble        | Frame 1 | Frame 2 | ... | Frame N |
| 576 bits (all alternating 1010...) | 544 bits each |
```

- **Preamble**: 576 bits of alternating 1/0 — provides bit timing sync
- **Frame**: one sync codeword + 16 data codewords

### 3.3 Codewords

Each codeword is **32 bits**: 1 flag bit + 31 content bits.

**Sync codeword** (every frame starts with this): `0x7CD215D8`. Your decoder searches for this pattern to align to frame boundaries.

**Address codeword** (flag bit = 0):

```text
| 0 | 18-bit pager address (top 18 bits) | 2-bit function code | 10-bit BCH | 1-bit parity |
```

The pager's full address is 21 bits: the 18-bit address above, plus the 3-bit frame number (which of the 8 frames in the batch contained this codeword). This means 2^21 ≈ 2 million unique addresses per channel. The 2-bit function code is typically the message type/bank.

**Message codeword** (flag bit = 1):

```text
| 1 | 20-bit message data | 10-bit BCH | 1-bit parity |
```

Message codewords follow an address codeword and carry the payload. Format of the 20 data bits depends on message type:

- **Numeric**: 5 4-bit BCD digits (digits 0-9, and special chars: space = 1010, hyphen = 1011, etc.)
- **Alphanumeric**: 7-bit ASCII characters, LSB-first, packed across codewords

### 3.4 BCH Error Correction

Every codeword includes a **BCH(31,21)** error correcting code — 10 parity bits protecting 21 data bits. Corrects up to 2 bit errors per codeword.

Generator polynomial: `x^10 + x^9 + x^8 + x^6 + x^5 + x^3 + 1` → `0x769`.

A well-written BCH decoder gives you significant noise immunity. Raw POCSAG without BCH is useless; with BCH, you get clean output even from marginal signal.

### 3.5 Example Decoded Message

```text
POCSAG-512: Address: 1234567  Function: 0  Alpha: "CODE BLUE ROOM 412"
POCSAG-1200: Address: 2345678  Function: 2  Numeric: "555-1234"
```

---

## 4. FLEX Protocol

More complex than POCSAG — designed for higher throughput and better error correction.

### 4.1 Physical Layer

- **Modulation**: 2-FSK at 1600 bit/s, or 4-FSK at 3200 or 6400 bit/s
- **Deviation**: ±4.8 kHz
- **Frame structure**: synchronous — transmissions start exactly every 1.875 seconds, aligned to absolute time (GPS-disciplined at the transmitter)

### 4.2 Frame Structure

Each frame is 1.875 seconds long and contains:

```text
| Sync 1 | Frame Info | Sync 2 | Blocks (11 of them) |
| 115 bits | 32 bits | 45 bits | 8 codewords each |
```

Each block contains 8 codewords of 32 bits each. With 11 blocks per frame, that's 88 codewords per frame, at 32 bits each = 2816 bits per frame. At 1600 bps, one frame = 1.76 seconds (plus sync overhead = 1.875 seconds total).

### 4.3 FLEX Addressing and Messaging

FLEX uses a more sophisticated capcode (address) scheme than POCSAG. Messages can be:

- **Short instructional**: numeric
- **Alphanumeric** (standard text)
- **Binary/secure**: encrypted payload (less common)

FLEX also supports broadcast messages to groups and has timing/roaming features POCSAG lacks.

### 4.4 Error Correction

FLEX uses a **(31,21) BCH code** per codeword, plus **block interleaving** to spread burst errors across multiple codewords. Decoding:

1. De-interleave the 8 codewords in a block
2. BCH-decode each codeword
3. Reassemble the original data

---

## 5. Decoder Pipeline

```text
IQ samples → FM demod → lowpass → symbol slicing →
sync word search → codeword extraction → BCH decode →
address/message parse → text output
```

### 5.1 The Easy Path: multimon-ng

**multimon-ng** is a mature, well-maintained decoder supporting POCSAG (all baud rates) and FLEX. Usage:

```bash
rtl_fm -f 152.840M -s 22050 -g 30 -o 4 - | \
    multimon-ng -t raw -a POCSAG512 -a POCSAG1200 -a POCSAG2400 -a FLEX -f alpha /dev/stdin
```

That's the whole pipeline — FM-demodulated PCM audio into multimon, decoded messages out. Output looks like:

```text
POCSAG1200: Address: 1234567  Function: 0  Alpha: FIRE STATION 3 RESPOND
FLEX|2026-04-22 14:32:15|1600/2/A|1234567 (GroupMsg)|Test message
```

For actually reading pagers, multimon-ng is the way. Writing your own decoder is a learning project.

### 5.2 Writing Your Own (POCSAG)

If you want to understand it:

**FM demod**: standard quadrature discriminator, output sampled at 22 kHz or higher.

**Symbol slicing**: POCSAG is 2FSK, so the FM demod output directly reveals the bit values. Positive = one bit, negative = other. Use a DC-blocking filter to remove any residual carrier offset, then a comparator:

```python
def slice_bits(fm_audio, baud_rate, sample_rate):
    samples_per_bit = sample_rate / baud_rate
    # Align to first zero crossing for timing
    bits = []
    for i in range(0, len(fm_audio), samples_per_bit):
        bits.append(0 if fm_audio[int(i + samples_per_bit/2)] < 0 else 1)
    return bits
```

**Sync word search**: slide through the bitstream looking for `0x7CD215D8`. Account for possible bit inversion.

**BCH decode**: use a library (e.g. `pyFEC`) or implement polynomial long division + syndrome-based error correction. There are many textbook implementations.

**Codeword parse**: address vs. message determined by top bit; split remaining fields; accumulate messages across multiple codewords.

**Character decoding**: 7-bit ASCII packed LSB-first across codeword boundaries is the fiddly bit. Keep a bit accumulator and shift characters out as you get enough bits.

### 5.3 Writing Your Own (FLEX)

This is substantially harder. The frame alignment is time-synchronous, the error correction is interleaved, and the documentation is less freely available. For FLEX, **just use multimon-ng**.

---

## 6. Identifying Pager Traffic on a Scanner

If you haven't found active frequencies yet, paging is distinctive on a waterfall display:

- **POCSAG**: bursts of signal 1–3 seconds long, with a distinctive "warbling" audio quality. Bursts occur every few minutes on busy channels.
- **FLEX**: regular periodic bursts every 1.875 seconds. On a waterfall, FLEX looks like a solid continuous signal punctuated by regular structure. Very recognizable once you've seen it.
- **Both** show up as narrowband signals (~15 kHz wide) with clean FSK spectral signatures (two distinct tones on the waterfall).

Tune to a suspected paging frequency in NFM mode and listen — POCSAG sounds like a modem chirping; FLEX sounds like a continuous buzz.

---

## 7. Applications Beyond Just Reading

Some paging-adjacent projects:

### 7.1 Alert Correlation

If you can identify your local fire/EMS paging channel, you can correlate pages with:
- CAD (Computer-Aided Dispatch) feeds from radioreference
- Broadcastify fire/EMS audio streams
- Scanner traffic

Builds a real-time emergency services dashboard.

### 7.2 Frequency Coordination Research

Decoded page headers include transmitter ID. Monitoring over time reveals which tower sites serve which pagers, letting you map network topology.

### 7.3 Statistics

Analyze paging traffic volume over time — useful for understanding service utilization, identifying shift changes, etc. (Aggregate statistics are fine; individual message content is not.)

### 7.4 Historical Interest

POCSAG was designed by the UK Post Office in 1982. The protocol has been essentially unchanged for 40+ years and is one of the longest-lived digital communication standards still in active use. Worth appreciating as computing archaeology.

---

## 8. References

**Specifications:**

- **POCSAG**: ITU-R M.584-2 — https://www.itu.int/rec/R-REC-M.584/en (originally CCIR Recommendation 584)
- **FLEX**: protocol details are covered by Motorola patents (now expired); no single open specification, but extensive community documentation exists

**Decoders:**

- **multimon-ng** — https://github.com/EliasOenal/multimon-ng — handles POCSAG and FLEX, actively maintained
- **PDW (PagerDisplay Windows)** — classic Windows-only decoder with GUI
- **SDR#** plugins for POCSAG/FLEX (Windows)

**Technical resources:**

- POCSAG format description: https://www.sigidwiki.com/wiki/POCSAG
- FLEX format description: https://www.sigidwiki.com/wiki/FLEX
- SigIDWiki in general — excellent reference for identifying unknown signals

**Frequency databases:**

- **RadioReference.com** — US frequency database, community-maintained
- **FCC ULS** — https://wireless.fcc.gov/UlsApp/UlsSearch/searchAdvanced.jsp
- **RadioReference Blacksburg/Christiansburg VA** — direct link for your local area

---

## 9. Suggested Build Order

**For quick results:**

1. Find local paging frequencies via RadioReference
2. Install `multimon-ng` (it's in Arch's AUR)
3. `rtl_fm -f <freq>M -s 22050 -g 30 | multimon-ng -a POCSAG1200 -f alpha /dev/stdin`
4. Watch messages scroll by
5. Appreciate the protocol, don't publish anything

**For protocol understanding:**

1. Record IQ of a paging frequency during a busy period
2. FM demodulate; save the audio
3. Find the POCSAG sync word in the audio
4. Implement FSK slicing → bitstream
5. Implement sync word detection + codeword extraction
6. Implement BCH(31,21) decoding
7. Parse address/message codewords
8. Decode numeric/alphanumeric character sets
9. Validate against multimon-ng on the same recording
10. Optionally tackle FLEX (much harder)

---

## 10. Ethical Coda

Paging decoding is a fascinating technical project. It can also intercept personal medical information, emergency responder coordination, and private communications. Treat what you receive with the same discretion you'd want applied to your own communications. The legal environment is murky; the ethical one is not — just be cool about it.

---

*End of reference.*
