# APT integration-test fixtures

These WAV files come from the [noaa-apt](https://github.com/martinber/noaa-apt)
project's `test/` corpus and are used by `tests/apt_integration.rs` to validate
our APT decoder against real, known-good NOAA 19 audio.

| File | Source | Description |
|---|---|---|
| `noaa19_apt_11025hz.wav` | noaa-apt's `test/test_11025hz.wav` | ~14 minutes of NOAA 19 APT audio captured 2018-12-22, 11025 Hz mono PCM. Real pass — produces a recognizable image when decoded correctly. |
| `noise_apt_11025hz.wav` | noaa-apt's `test/noise_48000hz.wav` | 30 s of pure noise at 11025 Hz mono PCM (the file's name is a misnomer in noaa-apt; it's actually 11025 Hz per `file(1)`). Used as a negative control. |
| `noaa19_apt_tle.txt` | noaa-apt's `test/test_tle.txt` | Multi-satellite TLE set from the same era. Not currently consumed but kept alongside in case future tests want to cross-check the SGP4 path against the same reference timestamp. |

## License

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

The SDR-RS source code itself remains MIT-licensed. If the noaa-apt maintainer
objects, these fixtures can be removed without touching any production-path
code; the integration test would gate on `cfg!(any())` until alternatives are
sourced.
