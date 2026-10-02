# Decoder test fixtures

Real, known-good audio recordings used to validate the SDR-RS decoders against
actual signals.

## APT (NOAA satellite)

These WAV files come from the [noaa-apt](https://github.com/martinber/noaa-apt)
project's `test/` corpus and are used by `tests/apt_integration.rs` to validate
our APT decoder against real, known-good NOAA 19 audio.

| File | Source | Description |
|---|---|---|
| `noaa19_apt_11025hz.wav` | noaa-apt's `test/test_11025hz.wav` | ~14 minutes of NOAA 19 APT audio captured 2018-12-22, 11025 Hz mono PCM. Real pass — produces a recognizable image when decoded correctly. |
| `noise_apt_11025hz.wav` | noaa-apt's `test/noise_48000hz.wav` | 30 s of pure noise at 11025 Hz mono PCM (the file's name is a misnomer in noaa-apt; it's actually 11025 Hz per `file(1)`). Used as a negative control. |
| `noaa19_apt_tle.txt` | noaa-apt's `test/test_tle.txt` | Multi-satellite TLE set from the same era. Not currently consumed but kept alongside in case future tests want to cross-check the SGP4 path against the same reference timestamp. |

## WEFAX (HF radiofax)

| File | Source | Description |
|---|---|---|
| `wefax_surface_48hr_11025hz.wav` | [2bsailing.ca](https://www.2bsailing.ca/info/Weatherfax_and_shortwave_radio.php) — `wefax/48HrSurface_Valid201302011200.wav` | ~10 minutes of a NOAA/NWS **48-hour surface-analysis** radiofax, received on 17151 kHz at Victoria BC on 2013-01-30 (chart valid 2013-02-01 1200Z). 11025 Hz mono 16-bit PCM. Decodes to a fully legible surface chart (date header, isobars, pressure centres, GALE/STORM annotations, NOAA logo) via `cargo run -p sdr-dsp --example wefax_decode_wav -- <this> out.png --sync`. |

## WEFAX fax-presence detector fixtures

| File | Source | Description |
|---|---|---|
| `wefax_nmf_present_12k.wav` | Off-air capture of station NMF (US Coast Guard, Boston) HF radiofax, 4235 kHz USB | **Provenance:** captured off-air by the repository author (Jason Herald) with an Airspy R2 + SpyVerter R2 on 2026-09-19, then demodulated and down-sampled to a 12 s excerpt (12 kHz mono 32-bit float) of the NMF fax subcarrier. Used by `tests/wefax_presence.rs` to assert `WefaxPresenceDetector` reads a real signal as present. **Rights:** NMF is a US Coast Guard (US Government) broadcast; per 17 U.S.C. §105 US Government works are not subject to domestic copyright, and a faithful mechanical radio capture adds no new copyrightable authorship, so the recording carries no separate copyright — it is contributed to this repository by its author under the repository's own license, and treated as public domain (same rationale as the WEFAX surface-chart fixture above). |
| `wefax_static_12k.wav` | Synthetic (`sox -n synth whitenoise`) | 12 s of synthesized white noise (12 kHz mono 32-bit float), used as the negative control in `tests/wefax_presence.rs`. |

## License

### APT fixtures

noaa-apt is licensed under the GNU GPL v3.0. These files are committed to this
repository's source tree and so are redistributed as part of the git history
and any source archive that includes the `crates/sdr-dsp/tests/data/` path.
They are NOT included in compiled / published crate artifacts:
`crates/sdr-dsp/Cargo.toml` excludes `tests/data/**` from `cargo package`
output, so any binary or `crates.io` publish drops the fixtures.

The fixtures are used exclusively as input to `tests/apt_integration.rs`,
which feeds them to the SDR-RS APT decoder and asserts on the decoder's
output. Anyone who clones / forks this repo is governed by the GPL-3.0 terms
WITH RESPECT TO THESE FILES (i.e. the fixture WAVs themselves) — that is, if
they redistribute the fixture files, they must do so under GPL-3.0 and
include the original noaa-apt license/copyright notice.

### WEFAX fixture

`wefax_surface_48hr_11025hz.wav` is a capture of a NOAA / National Weather
Service marine radiofax product. NWS/NOAA works are US Government works and are
therefore in the **public domain** under 17 U.S.C. §105; a faithful mechanical
radio capture of a public-domain broadcast adds no new copyrightable authorship,
so the recording is treated as public domain. It was published by 2bsailing.ca
expressly to help test radiofax-decoding software, and is kept here solely as
input to the manual `wefax_decode_wav` example / decoder regression checks.

### WEFAX decoded-row fixtures (`wefax_*.u8`)

Raw decoded scanlines: 1809 bytes per row, one greyscale byte per pixel,
rows concatenated with no header. They were cut from charts this project
decoded off-air from NOAA NMF Boston (4235 kHz) on the nights of
2026-09-23/24 (public-domain US Government broadcasts, as above) and are
used by the phasing / sync unit tests via `src/wefax/test_fixtures.rs`
(#921):

| file | source image, rows | what it is |
|---|---|---|
| `wefax_phasing_strong.u8` | `wefax-2026-09-23-083900`, 1280–1349 | strong phasing band, pulse edge ~1411–1417, stray specks left of the pulse |
| `wefax_phasing_weak.u8` | `wefax-2026-09-24-052514`, 1178–1231 | weak, heavily speckled phasing band, pulse edge ~1591–1598 |
| `wefax_content_surface.u8` | `wefax-2026-09-24-052514`, 120–519 | surface forecast chart content, no phasing |
| `wefax_content_satellite.u8` | `wefax-2026-09-24-083706`, 3200–3599 | satellite IR imagery content, no phasing |
| `wefax_content_analysis.u8` | `wefax-2026-09-23-083900`, 1500–1899 | analysis chart content, no phasing |

### SDR-RS source

The SDR-RS source code itself remains MIT-licensed. If the noaa-apt maintainer
objects, the APT fixtures can be removed without touching any production-path
code; the integration test would gate on `cfg!(any())` until alternatives are
sourced.
