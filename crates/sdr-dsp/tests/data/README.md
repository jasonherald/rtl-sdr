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

noaa-apt is licensed under the GNU GPL v3.0. We use these files **only as test
fixtures** — they are not redistributed as part of the SDR-RS binary, do not
appear in any built artifact, and are not "linked" into our codebase by any
reasonable interpretation. The fixtures are exclusively input to
`tests/apt_integration.rs`, which feeds them to the SDR-RS APT decoder and
asserts on the decoder's output.

This usage is consistent with how test corpora are routinely shared across
permissive and copyleft projects (e.g. how `cargo` itself ships test inputs
sourced from various projects). Should the noaa-apt maintainer object, these
fixtures can be removed without affecting any production-path code; the
integration test would simply gate on `cfg!(any())` until alternatives are
sourced.

The SDR-RS source code itself remains MIT-licensed.
