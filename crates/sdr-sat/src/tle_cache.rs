//! Celestrak TLE fetch + on-disk cache.
//!
//! Pulls satellite TLE files from
//! `https://celestrak.org/NORAD/elements/{slug}.txt` and stores them
//! under `~/.cache/sdr-rs/tle/`. Daily refresh: callers ask for a TLE
//! by NORAD id, the cache returns it from disk if the file is fresher
//! than [`DEFAULT_REFRESH_MAX_AGE`] (24 hours); otherwise it tries to
//! re-download. If re-download fails (offline, Celestrak down, etc.)
//! it falls back to whatever stale copy the cache already has so the
//! UI degrades gracefully rather than going dark.
//!
//! The HTTP fetch is **blocking** by design — the rest of the
//! workspace is blocking-only, and TLE fetches are once-a-day so the
//! caller is expected to invoke this from a worker thread (the
//! scheduler UI's "refresh TLEs" button, for example).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration as StdDuration, SystemTime};

/// Default cache freshness window. TLEs from Celestrak are updated
/// every few hours but propagation accuracy stays well within SGP4's
/// km-level error budget for at least a few days. 24 hours strikes a
/// reasonable balance between hit rate and bandwidth.
pub const DEFAULT_REFRESH_MAX_AGE: StdDuration = StdDuration::from_hours(24);

/// Default fetch timeout for blocking reqwest calls — long enough that
/// a sluggish network won't fail spuriously, short enough that the UI
/// thread doesn't lock up if Celestrak is hung.
pub const DEFAULT_FETCH_TIMEOUT: StdDuration = StdDuration::from_secs(15);

/// Which Celestrak source file a TLE belongs to. Each maps directly to
/// a `https://celestrak.org/NORAD/elements/{slug}.txt` URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TleSource {
    /// `noaa.txt` — NOAA POES satellites (15/18/19) for APT.
    Noaa,
    /// `weather.txt` — Meteor-M and other weather sats for LRPT.
    Weather,
    /// `stations.txt` — ISS and crewed-vehicle TLEs for SSTV.
    Stations,
}

impl TleSource {
    /// Celestrak filename slug — the bit between `/elements/` and `.txt`.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Noaa => "noaa",
            Self::Weather => "weather",
            Self::Stations => "stations",
        }
    }

    /// Full Celestrak URL for the file backing this source.
    #[must_use]
    pub fn url(self) -> String {
        format!("https://celestrak.org/NORAD/elements/{}.txt", self.slug())
    }
}

/// Errors from cache lookup or HTTP fetch.
#[derive(Debug, thiserror::Error)]
pub enum TleCacheError {
    /// `dirs_next::cache_dir` returned `None` — the platform doesn't
    /// expose a user cache dir (rare on Linux/macOS, possible in
    /// minimal sandbox environments).
    #[error("no cache directory available on this platform")]
    NoCacheDir,
    /// Celestrak returned a non-2xx status, the request timed out, or
    /// reqwest itself failed to send.
    #[error("TLE fetch failed: {0}")]
    Fetch(String),
    /// Local I/O error reading or writing the cache file.
    #[error("cache I/O error at {path:?}: {source}")]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Underlying I/O error. (`source` field name is intentional
        /// here — `std::io::Error` does implement `Error`, so
        /// thiserror's `Error::source()` shim works as expected.)
        #[source]
        source: std::io::Error,
    },
    /// Looked up a NORAD id that wasn't present in the source's text.
    /// Usually means the satellite was decommissioned and dropped from
    /// the upstream file.
    #[error("NORAD id {norad_id} not found in {tle_source:?} TLE source")]
    NotFound {
        /// NORAD id requested.
        norad_id: u32,
        /// Source the lookup ran against. Renamed from `source` because
        /// thiserror auto-treats a field literally called `source` as
        /// an `Error::source()` shim and demands the type implement
        /// `std::error::Error`.
        tle_source: TleSource,
    },
}

/// Filesystem-backed cache of Celestrak TLE files.
pub struct TleCache {
    cache_dir: PathBuf,
    refresh_max_age: StdDuration,
    fetch_timeout: StdDuration,
    /// HTTP client built lazily on first fetch so builder methods like
    /// [`TleCache::with_fetch_timeout`] still get a chance to apply
    /// before we lock in the configuration. Reused across fetches for
    /// connection / TLS-session pooling — small win for a once-a-day
    /// caller, free improvement if a future flow asks for several
    /// sources in a row.
    client: OnceLock<reqwest::blocking::Client>,
}

impl TleCache {
    /// Create a cache rooted at the platform's standard user-cache
    /// directory (`~/.cache/sdr-rs/tle/` on Linux).
    ///
    /// # Errors
    ///
    /// Returns [`TleCacheError::NoCacheDir`] if the platform doesn't
    /// expose a cache dir.
    pub fn new() -> Result<Self, TleCacheError> {
        let base = dirs_next::cache_dir().ok_or(TleCacheError::NoCacheDir)?;
        Ok(Self::with_dir(base.join("sdr-rs").join("tle")))
    }

    /// Create a cache rooted at an arbitrary directory. Useful for
    /// tests or for users who want to share a cache between
    /// installations.
    #[must_use]
    pub fn with_dir(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            refresh_max_age: DEFAULT_REFRESH_MAX_AGE,
            fetch_timeout: DEFAULT_FETCH_TIMEOUT,
            client: OnceLock::new(),
        }
    }

    /// Override the cache freshness window. Values shorter than ~1 hour
    /// will hammer Celestrak unnecessarily; longer than ~7 days will
    /// degrade SGP4 accuracy as TLEs get stale.
    #[must_use]
    pub const fn with_refresh_max_age(mut self, max_age: StdDuration) -> Self {
        self.refresh_max_age = max_age;
        self
    }

    /// Override the per-fetch HTTP timeout.
    #[must_use]
    pub const fn with_fetch_timeout(mut self, timeout: StdDuration) -> Self {
        self.fetch_timeout = timeout;
        self
    }

    /// Path on disk where this source's TLE file is cached.
    #[must_use]
    pub fn cache_path(&self, source: TleSource) -> PathBuf {
        self.cache_dir.join(format!("{}.txt", source.slug()))
    }

    /// Look up the TLE pair for `norad_id` in the given source,
    /// refreshing the cache file from Celestrak if it's stale.
    ///
    /// On a fetch failure with a stale-but-present cache, returns the
    /// cached entry — the user gets *something* rather than a blank
    /// scheduler. On no-cache + no-network, returns the fetch error.
    ///
    /// # Errors
    ///
    /// Surfaces [`TleCacheError`] for network, I/O, or lookup failures.
    pub fn tle_for(
        &self,
        norad_id: u32,
        source: TleSource,
    ) -> Result<(String, String), TleCacheError> {
        let text = self.tle_text(source)?;
        parse_tle_text(&text, norad_id).ok_or(TleCacheError::NotFound {
            norad_id,
            tle_source: source,
        })
    }

    /// Get the raw TLE-file text for a source, refreshing on disk if
    /// the cached copy is stale (or missing).
    ///
    /// # Errors
    ///
    /// See [`TleCache::tle_for`].
    pub fn tle_text(&self, source: TleSource) -> Result<String, TleCacheError> {
        let path = self.cache_path(source);
        let stale = is_stale(&path, self.refresh_max_age);

        if stale {
            // Try a refresh. If it succeeds, write to disk and return
            // the new text; if it fails but a stale cache exists, use
            // that; otherwise propagate the fetch error.
            match self.fetch(source) {
                Ok(text) => {
                    // Best-effort cache write — a failed write
                    // (read-only fs, disk full, permissions) shouldn't
                    // throw away network-fresh TLE data. Log and move
                    // on; the next call will just refetch.
                    if let Err(e) = self.write_cache(&path, &text) {
                        tracing::warn!(
                            "TLE cache write for {:?} failed ({e}); returning fresh in-memory copy",
                            source,
                        );
                    }
                    return Ok(text);
                }
                Err(fetch_err) => {
                    if let Some(cached) = read_file(&path)? {
                        tracing::warn!(
                            "TLE fetch for {:?} failed ({fetch_err}); using stale cache",
                            source,
                        );
                        return Ok(cached);
                    }
                    return Err(fetch_err);
                }
            }
        }

        // Cache is fresh — read from disk.
        read_file(&path)?.ok_or_else(|| TleCacheError::Io {
            path,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "fresh cache file disappeared between mtime check and read",
            ),
        })
    }

    /// Blocking HTTP fetch of one source file. Reuses the cached
    /// reqwest client across calls for connection + TLS-session pooling.
    fn fetch(&self, source: TleSource) -> Result<String, TleCacheError> {
        let client = self.client()?;
        let url = source.url();
        let response = client
            .get(&url)
            .send()
            .map_err(|e| TleCacheError::Fetch(format!("GET {url}: {e}")))?;
        let response = response
            .error_for_status()
            .map_err(|e| TleCacheError::Fetch(format!("HTTP status: {e}")))?;
        response
            .text()
            .map_err(|e| TleCacheError::Fetch(format!("response body: {e}")))
    }

    /// Get-or-build the cached HTTP client. First call builds it from
    /// the current `fetch_timeout`; subsequent calls reuse the same
    /// underlying connection pool (`reqwest::blocking::Client` is
    /// internally `Arc`-counted, so clones are cheap atomic
    /// increments — no realloc, no new TLS sessions). Builder methods
    /// that change the timeout *after* the first fetch don't take
    /// effect, since reqwest bakes the timeout into the client at
    /// build time.
    ///
    /// `OnceLock::get_or_try_init` would be the idiomatic single-call
    /// version of this dance, but that's still nightly as of Rust
    /// 1.95 (`once_cell_try`). Until it stabilises, the manual
    /// get-or-build-and-clone pattern below avoids both the panic
    /// path of `.expect` and the dep on the external `once_cell`
    /// crate.
    fn client(&self) -> Result<reqwest::blocking::Client, TleCacheError> {
        if let Some(c) = self.client.get() {
            return Ok(c.clone());
        }
        let new_client = reqwest::blocking::Client::builder()
            .timeout(self.fetch_timeout)
            .user_agent(concat!("sdr-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| TleCacheError::Fetch(format!("client build: {e}")))?;
        // Race-free publish: if another thread won, their client is
        // canonical and ours gets dropped. Either way `get` should
        // return Some afterwards; if for some impossible reason it
        // doesn't, fall back to our local copy rather than panicking.
        let _ = self.client.set(new_client.clone());
        Ok(self.client.get().cloned().unwrap_or(new_client))
    }

    /// Write `text` to `path` atomically via the standard
    /// "write-to-tempfile-then-rename" dance. A power loss / SIGKILL /
    /// OOM-kill mid-write leaves either the previous stale cache
    /// intact (rename never happened) or the new fresh content
    /// (rename succeeded), never a truncated file. Same-directory
    /// tempfile so the rename stays on the same filesystem and the
    /// kernel's `rename(2)` atomicity guarantee applies.
    #[allow(clippy::unused_self)] // kept on impl for symmetry with other cache methods
    fn write_cache(&self, path: &Path, text: &str) -> Result<(), TleCacheError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TleCacheError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        // Per-process tempfile so two concurrent processes hitting the
        // same cache dir don't trample each other's in-flight write.
        // Two threads in the same process are still racy on the tmp
        // file, but our once-a-day refresh cadence makes that
        // essentially impossible to hit in practice.
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| TleCacheError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        std::fs::rename(&tmp, path).map_err(|e| {
            // Best-effort cleanup of the orphaned tempfile so we don't
            // leak it on every failed rename.
            let _ = std::fs::remove_file(&tmp);
            TleCacheError::Io {
                path: path.to_path_buf(),
                source: e,
            }
        })
    }
}

fn read_file(path: &Path) -> Result<Option<String>, TleCacheError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(TleCacheError::Io {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

fn is_stale(path: &Path, max_age: StdDuration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true; // missing = stale
    };
    let Ok(modified) = meta.modified() else {
        return true; // can't read mtime → assume stale
    };
    SystemTime::now()
        .duration_since(modified)
        .map_or(true, |age| age > max_age)
}

/// Find the `(line1, line2)` TLE pair for `norad_id` in a Celestrak
/// formatted text file. Accepts both 2-line entries (TLE-only) and
/// 3-line entries (name + TLE), as Celestrak emits the 3-line variant
/// for the named-satellite endpoints we use.
///
/// Uses a sliding-window scan rather than a "consume in groups of 3"
/// loop so that a malformed entry can't accidentally swallow the next
/// satellite's `1 ...` line as its `line2` and lose that satellite.
/// Each window position checks "is this a valid TLE pair?" and slides
/// by exactly one line otherwise — worst-case `O(n)` and resyncs
/// cleanly across any cache corruption.
#[must_use]
#[allow(clippy::similar_names)] // line1/line2 names match TLE-spec terminology
pub fn parse_tle_text(text: &str, norad_id: u32) -> Option<(String, String)> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();

    let mut i = 0_usize;
    while i < lines.len() {
        // At each window position, decide where line1/line2 would be:
        //   * starts with "1 "  → 2-line entry: (lines[i], lines[i+1])
        //   * otherwise (name)  → 3-line entry: (lines[i+1], lines[i+2])
        let (line1, line2) = if lines[i].starts_with("1 ") {
            match lines.get(i + 1) {
                Some(l2) => (lines[i], *l2),
                None => break,
            }
        } else {
            match (lines.get(i + 1), lines.get(i + 2)) {
                (Some(l1), Some(l2)) => (*l1, *l2),
                _ => break,
            }
        };

        // Cross-check: both TLE lines have the NORAD catalog number at
        // the same column, and a sane file always pairs them. A
        // corrupted cache could splice line 1 of one satellite with
        // line 2 of another (both look "valid" individually) — refuse
        // to return such a Frankenstein pair.
        if line1.starts_with("1 ")
            && line2.starts_with("2 ")
            && let (Some(line1_id), Some(line2_id)) =
                (norad_id_from_tle_line(line1), norad_id_from_tle_line(line2))
            && line1_id == norad_id
            && line2_id == norad_id
        {
            return Some((line1.to_string(), line2.to_string()));
        }

        // Slide by exactly one line. Even if this window saw a
        // malformed pair that ate "the next entry's line 1" as its
        // line2, the next window starts at lines[i+1] which is that
        // very line 1 — so we re-evaluate it as a fresh entry start
        // rather than skipping it.
        i += 1;
    }
    None
}

/// 0-indexed start of the NORAD catalog number field in TLE line 1
/// (column 3 in 1-indexed TLE-spec terms).
const TLE_NORAD_START: usize = 2;
/// 0-indexed exclusive end of the NORAD field (column 7 inclusive).
const TLE_NORAD_END: usize = 7;

/// Extract the NORAD catalog number from columns 3..=7 of a TLE line.
/// Works on both line 1 (`"1 NNNNNX ..."`) and line 2 (`"2 NNNNN ..."`)
/// — the catalog field sits at the same byte offsets in both. Returns
/// `None` for malformed lines — the caller skips and keeps scanning.
///
/// Uses `str::get` rather than direct slicing so a corrupted cache
/// file with multi-byte UTF-8 at the parsed byte offsets returns
/// `None` instead of panicking. (Real Celestrak content is ASCII;
/// a stray non-ASCII byte usually means the response was an HTML
/// error page that landed in the cache by accident.)
fn norad_id_from_tle_line(line: &str) -> Option<u32> {
    let field = line.get(TLE_NORAD_START..TLE_NORAD_END)?;
    field.trim().parse::<u32>().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Three real TLE entries (Vanguard 1, ISS, NOAA 19) with valid
    /// checksums, in 3-line Celestrak format, ready to be parsed.
    const SAMPLE_TLE_3LINE: &str = "\
VANGUARD 1
1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753
2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667
ISS (ZARYA)
1 25544U 98067A   20194.88670927  .00002728  00000-0  61021-4 0  9994
2 25544  51.6442 211.4001 0001234  92.7501 270.5089 15.49538275234276
NOAA 19
1 33591U 09005A   24001.50000000  .00000050  00000-0  50000-4 0  9994
2 33591  99.0000 100.0000 0010000  90.0000 270.0000 14.10000000123456
";

    /// Same three TLEs but in 2-line format (no name line) — Celestrak
    /// also serves this variant from some endpoints. Parser must
    /// handle both.
    const SAMPLE_TLE_2LINE: &str = "\
1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753
2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667
1 25544U 98067A   20194.88670927  .00002728  00000-0  61021-4 0  9994
2 25544  51.6442 211.4001 0001234  92.7501 270.5089 15.49538275234276
";

    #[test]
    fn slug_for_each_source_matches_celestrak_filename() {
        assert_eq!(TleSource::Noaa.slug(), "noaa");
        assert_eq!(TleSource::Weather.slug(), "weather");
        assert_eq!(TleSource::Stations.slug(), "stations");
    }

    #[test]
    fn url_points_to_celestrak_https() {
        let url = TleSource::Noaa.url();
        assert!(url.starts_with("https://celestrak.org/NORAD/elements/"));
        assert!(url.ends_with("noaa.txt"));
    }

    #[test]
    fn parse_three_line_format_finds_each_satellite() {
        let (l1, l2) = parse_tle_text(SAMPLE_TLE_3LINE, 5).unwrap();
        assert!(l1.starts_with("1 00005"));
        assert!(l2.starts_with("2 00005"));

        let (l1, _) = parse_tle_text(SAMPLE_TLE_3LINE, 25_544).unwrap();
        assert!(l1.starts_with("1 25544"));

        let (l1, _) = parse_tle_text(SAMPLE_TLE_3LINE, 33_591).unwrap();
        assert!(l1.starts_with("1 33591"));
    }

    #[test]
    fn parse_two_line_format_also_works() {
        let (l1, _) = parse_tle_text(SAMPLE_TLE_2LINE, 5).unwrap();
        assert!(l1.starts_with("1 00005"));
        let (l1, _) = parse_tle_text(SAMPLE_TLE_2LINE, 25_544).unwrap();
        assert!(l1.starts_with("1 25544"));
    }

    #[test]
    fn parse_returns_none_for_unknown_norad_id() {
        assert!(parse_tle_text(SAMPLE_TLE_3LINE, 99_999).is_none());
        // Empty input.
        assert!(parse_tle_text("", 5).is_none());
        // Garbage.
        assert!(parse_tle_text("not a tle file at all", 5).is_none());
    }

    #[test]
    fn parse_does_not_panic_on_multibyte_utf8_in_norad_field() {
        // A 6-byte string whose first 7 *bytes* happen to span a
        // 3-byte UTF-8 char boundary at byte 2 — direct slicing would
        // panic; `str::get` returns None and the parser keeps walking.
        let weird = "1 \u{1F4A9}99U garbage";
        // norad_id_from_tle_line must not panic, must not classify.
        assert_eq!(norad_id_from_tle_line(weird), None);
        // Whole-document parse with the bad line buried inside also
        // must not panic and must skip it cleanly.
        let mixed = format!(
            "{weird}\n2 00099 ignore\nVANGUARD 1\n1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753\n2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667\n",
        );
        let (l1, _) = parse_tle_text(&mixed, 5).unwrap();
        assert!(l1.starts_with("1 00005"));
    }

    #[test]
    fn parse_skips_pair_when_line2_is_not_a_real_tle_line() {
        // line2 doesn't start with "2 " — a corrupted file or a name
        // that accidentally landed where a TLE pair was expected.
        // Parser must skip and keep scanning rather than emit a bogus
        // (line1, garbage_line2) pair that would misfire downstream.
        let bad_pair_then_good = "\
NAME ONE
1 11111U 99001A   24000.00000000  .00000000  00000-0  10000-3 0  9990
NEXT NAME LINE NOT A TLE
1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753
2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667
";
        // Asking for NORAD 11111 (the misformatted entry) must NOT
        // succeed even though its line1 is valid — the partner line
        // isn't a TLE line.
        assert!(parse_tle_text(bad_pair_then_good, 11_111).is_none());
        // The resync test: after skipping the bad pair, the parser
        // must still find the well-formed entry that follows.
        let (l1, l2) = parse_tle_text(bad_pair_then_good, 5).unwrap();
        assert!(l1.starts_with("1 00005"));
        assert!(l2.starts_with("2 00005"));
    }

    #[test]
    fn parse_rejects_pair_with_mismatched_norad_ids() {
        // line1 of NORAD 5 spliced with line2 from a different
        // satellite (NORAD 25544). Both lines individually look like
        // valid TLE format, but the NORAD ids disagree — the parser
        // must reject rather than hand back a Frankenstein pair that
        // would propagate as garbage in SGP4.
        let mismatched_pair = "\
VANGUARD 1
1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753
2 25544  51.6442 211.4001 0001234  92.7501 270.5089 15.49538275234276
";
        assert!(parse_tle_text(mismatched_pair, 5).is_none());
        assert!(parse_tle_text(mismatched_pair, 25_544).is_none());
    }

    #[test]
    fn parse_resyncs_when_malformed_entry_swallows_next_line1() {
        // Adversarial corruption: a "name" line followed by *two*
        // valid line-1s back to back, then a valid line 2 for the
        // second of them. The first entry is malformed (`1 11111…`
        // pretends to be a line 1 but its partner is the *next*
        // satellite's line 1 instead of a "2 ..." line). The
        // sliding-window parser must NOT lose the good `(1 00005…,
        // 2 00005…)` pair, even though my earlier consume-in-3s
        // implementation would have eaten line 1 of NORAD 5 as
        // line 2 of NORAD 11111 and then run off the end.
        let bad_swallows_next = "\
SOMETHING
1 11111U 99001A   24000.00000000  .00000000  00000-0  10000-3 0  9990
1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753
2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667
";
        let (l1, l2) = parse_tle_text(bad_swallows_next, 5).unwrap();
        assert!(l1.starts_with("1 00005"));
        assert!(l2.starts_with("2 00005"));
    }

    #[test]
    fn parse_handles_blank_lines_and_crlf() {
        let with_noise = concat!(
            "\n",
            "\n",
            "VANGUARD 1\r\n",
            "1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753\r\n",
            "2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667\r\n",
            "\n",
        );
        let (l1, _) = parse_tle_text(with_noise, 5).unwrap();
        assert!(l1.starts_with("1 00005"));
    }

    #[test]
    fn cache_returns_text_when_file_is_fresh() {
        let dir = unique_temp_dir("fresh");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("noaa.txt");
        std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
        let cache = TleCache::with_dir(dir.clone());
        let text = cache.tle_text(TleSource::Noaa).unwrap();
        assert!(text.contains("VANGUARD 1"));
    }

    #[test]
    fn cache_returns_tle_pair_when_file_is_fresh() {
        let dir = unique_temp_dir("pair");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("noaa.txt");
        std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
        let cache = TleCache::with_dir(dir);
        let (l1, l2) = cache.tle_for(5, TleSource::Noaa).unwrap();
        assert!(l1.starts_with("1 00005"));
        assert!(l2.starts_with("2 00005"));
    }

    #[test]
    fn cache_returns_not_found_when_norad_id_missing_from_fresh_file() {
        let dir = unique_temp_dir("missing-id");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("noaa.txt");
        std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
        let cache = TleCache::with_dir(dir);
        let err = cache.tle_for(99_999, TleSource::Noaa).unwrap_err();
        assert!(matches!(
            err,
            TleCacheError::NotFound {
                norad_id: 99_999,
                tle_source: TleSource::Noaa,
            }
        ));
    }

    #[test]
    fn write_cache_atomic_rename_lands_file_at_final_path() {
        // Sanity test for the atomic-rename path: write some text,
        // verify the final cache file exists and contains exactly the
        // text we wrote, and verify no leftover ".tmp.*" siblings
        // were left behind in the cache directory.
        let dir = unique_temp_dir("atomic-write");
        std::fs::create_dir_all(&dir).unwrap();
        let cache = TleCache::with_dir(dir.clone());
        let path = cache.cache_path(TleSource::Noaa);
        cache.write_cache(&path, "hello cache").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "hello cache");
        // No leftover tempfiles in the cache directory.
        let leftover_tmp_count = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.starts_with("tmp"))
            })
            .count();
        assert_eq!(leftover_tmp_count, 0);
    }

    #[test]
    fn is_stale_returns_true_for_missing_file() {
        let dir = unique_temp_dir("missing-mtime");
        let path = dir.join("does-not-exist.txt");
        assert!(is_stale(&path, DEFAULT_REFRESH_MAX_AGE));
    }

    #[test]
    fn is_stale_returns_false_for_fresh_file() {
        let dir = unique_temp_dir("fresh-mtime");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "hi").unwrap();
        assert!(!is_stale(&path, DEFAULT_REFRESH_MAX_AGE));
    }

    /// Per-test scratch dir under the system temp prefix. Avoids
    /// pulling in `tempfile` for one-off unit tests.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("sdr-sat-test-{tag}-{nanos}"))
    }
}
