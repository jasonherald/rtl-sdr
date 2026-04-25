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
                    self.write_cache(&path, &text)?;
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
    /// instance. Builder methods that change the timeout *after* the
    /// first fetch don't take effect — by design, since reqwest's
    /// timeout is baked into the client at build time.
    fn client(&self) -> Result<&reqwest::blocking::Client, TleCacheError> {
        if let Some(c) = self.client.get() {
            return Ok(c);
        }
        let new_client = reqwest::blocking::Client::builder()
            .timeout(self.fetch_timeout)
            .user_agent(concat!("sdr-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| TleCacheError::Fetch(format!("client build: {e}")))?;
        // If another thread won the race we just drop our local copy;
        // either way, `get` after `set` returns the canonical client.
        let _ = self.client.set(new_client);
        Ok(self
            .client
            .get()
            .expect("client::set succeeded or another thread won the race"))
    }

    #[allow(clippy::unused_self)] // kept on impl for symmetry with other cache methods
    fn write_cache(&self, path: &Path, text: &str) -> Result<(), TleCacheError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TleCacheError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        std::fs::write(path, text).map_err(|e| TleCacheError::Io {
            path: path.to_path_buf(),
            source: e,
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
#[must_use]
pub fn parse_tle_text(text: &str, norad_id: u32) -> Option<(String, String)> {
    let mut iter = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .peekable();

    while iter.peek().is_some() {
        // A TLE entry is either:
        //   3 lines: NAME / "1 ..." / "2 ..."
        //   2 lines: "1 ..." / "2 ..."
        let first = iter.next()?;
        let (line1, line2) = if first.starts_with("1 ") {
            (first.to_string(), iter.next()?.to_string())
        } else {
            // First was a name; next two are the TLE.
            let l1 = iter.next()?;
            let l2 = iter.next()?;
            (l1.to_string(), l2.to_string())
        };
        if let Some(parsed) = norad_id_from_line1(&line1)
            && parsed == norad_id
        {
            return Some((line1, line2));
        }
    }
    None
}

/// Extract the NORAD catalog number from columns 3..=7 of a TLE
/// line 1 (`"1 NNNNNX ..."`). Returns `None` for malformed lines —
/// the caller skips and keeps scanning.
fn norad_id_from_line1(line: &str) -> Option<u32> {
    if line.len() < 7 {
        return None;
    }
    line[2..7].trim().parse::<u32>().ok()
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
