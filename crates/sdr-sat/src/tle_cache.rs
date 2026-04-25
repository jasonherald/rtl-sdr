//! Celestrak TLE fetch + on-disk cache.
//!
//! Pulls satellite TLEs from Celestrak's stable per-catalog endpoint
//!
//! ```text
//! https://celestrak.org/NORAD/elements/gp.php?CATNR={id}&FORMAT=tle
//! ```
//!
//! and stores them under `~/.cache/sdr-rs/tle/{id}.tle`. Daily refresh:
//! callers ask for a TLE by NORAD id, the cache returns it from disk if
//! the file is fresher than [`DEFAULT_REFRESH_MAX_AGE`] (24 hours);
//! otherwise it tries to re-download. If re-download fails (offline,
//! Celestrak down, etc.) it falls back to whatever stale copy the cache
//! already has so the UI degrades gracefully rather than going dark.
//!
//! The HTTP fetch is **blocking** by design — the rest of the
//! workspace is blocking-only, and TLE fetches are once-a-day so the
//! caller is expected to invoke this from a worker thread (the
//! scheduler UI's "refresh TLEs" button, for example).
//!
//! ## Why per-NORAD instead of group files
//!
//! Earlier versions of this cache fetched whole group files
//! (`noaa.txt`, `weather.txt`, `stations.txt`) keyed by a `TleSource`
//! enum. Celestrak deprecated those URLs in 2024-2025: `noaa.txt`
//! returns 404 outright, the `noaa` group is gone from the GP API,
//! and the surviving `.txt` paths only redirect to the new `gp.php`
//! form. NOAA 15/18/19 (the APT-capable POES) aren't grouped under
//! any current GROUP slug — only `gp.php?CATNR=…` reliably returns
//! them. Per-NORAD fetches dodge the group-churn problem entirely
//! and let the catalog in [`crate::KNOWN_SATELLITES`] grow without
//! someone having to figure out which group each satellite lives in.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration as StdDuration, SystemTime};

/// Process-wide monotonic counter for unique tempfile names. Combined
/// with the process pid below it gives every concurrent write its own
/// path, so even two threads of the same process can't trample each
/// other's in-flight cache replacement.
static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(0);

/// Default cache freshness window. TLEs from Celestrak are updated
/// every few hours but propagation accuracy stays well within SGP4's
/// km-level error budget for at least a few days. 24 hours strikes a
/// reasonable balance between hit rate and bandwidth.
pub const DEFAULT_REFRESH_MAX_AGE: StdDuration = StdDuration::from_hours(24);

/// Default fetch timeout for blocking reqwest calls — long enough that
/// a sluggish network won't fail spuriously, short enough that the UI
/// thread doesn't lock up if Celestrak is hung.
pub const DEFAULT_FETCH_TIMEOUT: StdDuration = StdDuration::from_secs(15);

/// Build the Celestrak GP URL for a single NORAD catalog number. Public
/// so [`TleCache::with_fetcher`] callers (custom HTTP stacks) can mirror
/// the production URL shape without re-deriving it.
#[must_use]
pub fn celestrak_gp_url(norad_id: u32) -> String {
    format!("https://celestrak.org/NORAD/elements/gp.php?CATNR={norad_id}&FORMAT=tle")
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
    /// Cache file existed and parsed but didn't contain a TLE pair
    /// matching `norad_id`. Almost always the cached body got truncated
    /// or corrupted — the canonical path on a successful fetch is for
    /// the per-NORAD response to contain exactly the requested entry,
    /// so a `NotFound` after a clean fetch means upstream returned
    /// something other than the asked-for satellite.
    #[error("NORAD id {norad_id} not found in cached TLE response")]
    NotFound {
        /// NORAD id requested.
        norad_id: u32,
    },
}

/// Custom fetcher used by [`TleCache::with_fetcher`]. Receives the
/// requested NORAD id and returns the raw TLE text or a fetch-style
/// error. Useful for unit tests that need hermetic refetch-path
/// behaviour, and for users who want to plug in a non-reqwest HTTP
/// stack (proxy-aware client, custom auth, etc.).
pub type Fetcher = dyn Fn(u32) -> Result<String, TleCacheError> + Send + Sync;

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
    /// Optional fetcher override. When `Some`, [`TleCache::fetch`]
    /// short-circuits to this closure instead of building / using the
    /// reqwest client. Tests use this to make the refetch-path
    /// regression tests hermetic — no live HTTP, no DNS, no flakiness
    /// from celestrak.org being slow on a particular CI run.
    fetcher: Option<Box<Fetcher>>,
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
            fetcher: None,
        }
    }

    /// Override the network fetcher with a custom closure. Production
    /// callers normally don't need this — the default reqwest-based
    /// fetcher Just Works against celestrak.org. Two real uses:
    ///
    /// * **Tests** that exercise the refetch path can inject a canned
    ///   response so the unit suite stays hermetic (no live HTTP,
    ///   no DNS, no flaky CI from upstream slowness).
    /// * **Custom HTTP stacks** — a corporate proxy that needs auth,
    ///   a SOCKS tunnel, etc. Build the request through whatever
    ///   client is appropriate and hand the body string back.
    ///
    /// The closure is called once per refetch attempt; its return
    /// value goes through the same `has_any_tle_pair` validation as
    /// the default fetcher, so a closure that returns garbage still
    /// gets rejected before poisoning the cache.
    #[must_use]
    pub fn with_fetcher<F>(mut self, fetcher: F) -> Self
    where
        F: Fn(u32) -> Result<String, TleCacheError> + Send + Sync + 'static,
    {
        self.fetcher = Some(Box::new(fetcher));
        self
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

    /// Path on disk where this NORAD id's TLE is cached. Filename uses
    /// a `.tle` extension so a glance at `~/.cache/sdr-rs/tle/` makes
    /// it obvious what's there even if the satellite catalog grows.
    #[must_use]
    pub fn cache_path(&self, norad_id: u32) -> PathBuf {
        self.cache_dir.join(format!("{norad_id}.tle"))
    }

    /// Look up the TLE pair for `norad_id`, refreshing from Celestrak
    /// if the cache file is stale (or missing).
    ///
    /// On a fetch failure with a stale-but-present cache, returns the
    /// cached entry — the user gets *something* rather than a blank
    /// scheduler. On no-cache + no-network, returns the fetch error.
    ///
    /// # Errors
    ///
    /// Surfaces [`TleCacheError`] for network, I/O, or lookup failures.
    pub fn tle_for(&self, norad_id: u32) -> Result<(String, String), TleCacheError> {
        let text = self.tle_text(norad_id)?;
        parse_tle_text(&text, norad_id).ok_or(TleCacheError::NotFound { norad_id })
    }

    /// Get the raw TLE text for one satellite, refreshing on disk if
    /// the cached copy is stale (or missing).
    ///
    /// # Errors
    ///
    /// See [`TleCache::tle_for`].
    pub fn tle_text(&self, norad_id: u32) -> Result<String, TleCacheError> {
        let path = self.cache_path(norad_id);

        // Fast path: cache file is fresh, readable, AND structurally
        // a TLE file. Mtime + read alone aren't enough — older builds
        // or a manual `echo $whatever > cache.txt` could have left HTML
        // or garbage at this path, and we'd otherwise keep serving it
        // until the file ages out (and `tle_for` would surface a
        // misleading `NotFound` even when upstream is healthy). If a
        // TOCTOU race steals the file between mtime check and read,
        // or the cached content fails validation, fall through to
        // the refetch path so the cache self-heals.
        if !is_stale(&path, self.refresh_max_age)
            && let Some(cached) = read_file(&path)?
        {
            if has_any_tle_pair(&cached) {
                return Ok(cached);
            }
            tracing::warn!("ignoring corrupted fresh TLE cache for NORAD {norad_id}; refetching");
        }

        // Slow path: fetch from upstream, validate, write to disk.
        // If the fetch (or validation) fails, fall back to whatever
        // stale copy still happens to exist *and validates*. If even
        // that's gone or corrupted, surface the original fetch error.
        let fetch_result = self.fetch(norad_id).and_then(|text| {
            if has_any_tle_pair(&text) {
                Ok(text)
            } else {
                Err(TleCacheError::Fetch(
                    "response body did not contain any valid TLE pair (captive portal? HTML error page?)"
                        .to_string(),
                ))
            }
        });
        match fetch_result {
            Ok(text) => {
                // Best-effort cache write — a failed write (read-only
                // fs, disk full, permissions) shouldn't throw away
                // network-fresh TLE data. Log and move on.
                if let Err(e) = self.write_cache(&path, &text) {
                    tracing::warn!(
                        "TLE cache write for NORAD {norad_id} failed ({e}); returning fresh in-memory copy",
                    );
                }
                Ok(text)
            }
            Err(fetch_err) => {
                if let Some(cached) = read_file(&path)?
                    && has_any_tle_pair(&cached)
                {
                    tracing::warn!(
                        "TLE fetch for NORAD {norad_id} failed ({fetch_err}); using stale cache",
                    );
                    return Ok(cached);
                }
                Err(fetch_err)
            }
        }
    }

    /// Blocking HTTP fetch of one satellite's TLE. Reuses the cached
    /// reqwest client across calls for connection + TLS-session pooling.
    /// If [`TleCache::with_fetcher`] supplied an override, that closure
    /// runs instead — letting tests stay hermetic and users plug in
    /// custom HTTP stacks.
    fn fetch(&self, norad_id: u32) -> Result<String, TleCacheError> {
        if let Some(override_fetcher) = &self.fetcher {
            return override_fetcher(norad_id);
        }
        let client = self.client()?;
        let url = celestrak_gp_url(norad_id);
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
        // Per-call tempfile name: pid + a process-wide atomic counter.
        // pid disambiguates between concurrent processes hitting the
        // same cache dir, the counter disambiguates between concurrent
        // threads of the same process. Either way, every in-flight
        // write owns its own tempfile path, so no thread can rename
        // a half-written file out from under another.
        let tmp_id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{}.{tmp_id}", std::process::id()));
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

/// Read a UTF-8 file or return `None` for "not present-ish".
///
/// `NotFound` and `InvalidData` (non-UTF-8 contents — e.g. a corrupt
/// or binary cache file from another tool) are both treated as a
/// cache miss. `tle_text` falls through to the refetch path on miss,
/// so the cache self-heals immediately on the next successful fetch
/// rather than blocking the user until the mtime ages the bad file
/// out. Other I/O errors (permissions, mid-read failures) propagate.
fn read_file(path: &Path) -> Result<Option<String>, TleCacheError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            Ok(None)
        }
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
/// Uses a sliding-window scan over consecutive non-empty line pairs.
/// 3-line entries are handled implicitly: when the window sits on a
/// name + `1 …` pair, [`pair_matches`] rejects it (line 1 doesn't
/// start with `"1 "`); next iteration the window sits on `1 …` /
/// `2 …`, which matches. Worst-case `O(n)` and resyncs cleanly across
/// any cache corruption — even an entry whose "line 2" is actually
/// the next satellite's "line 1" (the previous consume-in-3s
/// implementation was vulnerable to that case).
#[must_use]
#[allow(clippy::similar_names)] // line1/line2 names match TLE-spec terminology
pub(crate) fn parse_tle_text(text: &str, norad_id: u32) -> Option<(String, String)> {
    let lines = tle_lines(text);
    let last = lines.len().saturating_sub(1);
    for i in 0..last {
        let (line1, line2) = (lines[i], lines[i + 1]);
        if pair_matches(line1, line2, Some(norad_id)) {
            return Some((line1.to_string(), line2.to_string()));
        }
    }
    None
}

/// Does `text` contain at least one structurally-consistent TLE pair?
/// Used as a sanity check on fetched bodies before they replace the
/// cache: a captive portal, proxy error page, or HTML maintenance
/// response from upstream would otherwise poison the on-disk cache
/// and break the offline fallback.
///
/// "Structurally consistent" means [`pair_matches`] accepts at least
/// one pair of consecutive non-empty lines without an id constraint.
/// Doesn't validate checksums or orbital fields — the cheap structural
/// check is enough to reject HTML responses (no `1 ` prefix at the
/// right offset, no NORAD id at the right column, mismatched ids).
#[must_use]
pub(crate) fn has_any_tle_pair(text: &str) -> bool {
    let lines = tle_lines(text);
    let last = lines.len().saturating_sub(1);
    (0..last).any(|i| pair_matches(lines[i], lines[i + 1], None))
}

/// Collect the non-empty, right-trimmed lines of `text` into a `Vec`.
/// Shared between [`parse_tle_text`] and [`has_any_tle_pair`] so the
/// preprocessing stays identical (CRLF tolerance, blank-line skipping)
/// no matter which entry point the caller used.
fn tle_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect()
}

/// Is `(line1, line2)` a structurally-valid TLE pair, optionally
/// matching `expected` NORAD id?
///
/// Required for any "yes":
///
/// * `line1` starts with `"1 "` (canonical TLE line-1 prefix);
/// * `line2` starts with `"2 "`;
/// * both lines parse a NORAD id from the catalog field
///   (columns 3..=7);
/// * the two ids agree — a corrupted cache could splice line 1 of
///   sat A with line 2 of sat B and they'd pass the prefix checks
///   individually; the cross-check catches the Frankenstein case;
/// * if `expected` is `Some(id)`, the parsed id equals `id`.
#[allow(clippy::similar_names)] // line1/line2 names match TLE-spec terminology
fn pair_matches(line1: &str, line2: &str, expected: Option<u32>) -> bool {
    if !line1.starts_with("1 ") || !line2.starts_with("2 ") {
        return false;
    }
    let (Some(id1), Some(id2)) = (norad_id_from_tle_line(line1), norad_id_from_tle_line(line2))
    else {
        return false;
    };
    if id1 != id2 {
        return false;
    }
    match expected {
        Some(target) => id1 == target,
        None => true,
    }
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
#[allow(clippy::unwrap_used, clippy::panic)]
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
    fn celestrak_gp_url_uses_stable_per_catnr_endpoint() {
        // Pin the URL shape — we deliberately do NOT use the legacy
        // `noaa.txt` / `weather.txt` group files (Celestrak deprecated
        // them in 2024-2025) or the redirector at `redirect.php`.
        // Per-CATNR is the documented stable interface.
        let url = celestrak_gp_url(25_338);
        assert_eq!(
            url,
            "https://celestrak.org/NORAD/elements/gp.php?CATNR=25338&FORMAT=tle"
        );
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

    /// Canned fetcher for hermetic refetch-path tests: always returns
    /// a `Fetch` error so we exercise the "fetch failed, fall back to
    /// stale" branch without touching the network.
    fn always_fail_fetcher() -> impl Fn(u32) -> Result<String, TleCacheError> + Send + Sync {
        |_| Err(TleCacheError::Fetch("test: fetcher disabled".to_string()))
    }

    /// NORAD id used as the fixture key across the cache tests below.
    /// Matches NOAA 19 — meaningful in case a test file actually leaks
    /// to disk during debugging, but every test injects a custom
    /// fetcher / cache dir so no real network call is ever made.
    const TEST_NORAD: u32 = 33_591;

    #[test]
    fn cache_does_not_trust_html_blob_in_fresh_cache_file() {
        // Pre-validation-gate versions (or a manual
        // `echo $whatever > cache.txt`) could have left HTML or
        // arbitrary text in the cache path. Mtime says it's fresh,
        // read says it's UTF-8 — but if we trust it, every call
        // serves garbage until the file ages out and `tle_for`
        // returns a misleading `NotFound`. The cache must validate
        // the content and treat invalid bodies as a miss → refetch.
        // With a canned-fail fetcher, the only valid outcome is
        // `Fetch(_)` — the test would have proved nothing if it
        // accepted whatever the live network happened to return.
        let dir = unique_temp_dir("html-blob");
        std::fs::create_dir_all(&dir).unwrap();
        let cache = TleCache::with_dir(dir.clone()).with_fetcher(always_fail_fetcher());
        let path = cache.cache_path(TEST_NORAD);
        let html = "<html><head><title>503</title></head><body>oops</body></html>\n";
        std::fs::write(&path, html).unwrap();

        match cache.tle_text(TEST_NORAD) {
            Err(TleCacheError::Fetch(_)) => {}
            Ok(text) => {
                panic!("cache returned the HTML blob verbatim — corruption was trusted: {text:?}")
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn cache_treats_non_utf8_cache_file_as_a_miss() {
        // A binary blob in the cache (say, a partial download where
        // gzip decompression got skipped, or some other tool wrote
        // bytes there) shows up to `read_to_string` as
        // `ErrorKind::InvalidData`. Should self-heal as a miss, NOT
        // surface as a hard `Io` error.
        let dir = unique_temp_dir("non-utf8");
        std::fs::create_dir_all(&dir).unwrap();
        let cache = TleCache::with_dir(dir.clone()).with_fetcher(always_fail_fetcher());
        let path = cache.cache_path(TEST_NORAD);
        // 0xFF 0xFE 0x80 0x81 0x82 are invalid as UTF-8 lead bytes.
        std::fs::write(&path, [0xFF_u8, 0xFE, 0x80, 0x81, 0x82]).unwrap();

        match cache.tle_text(TEST_NORAD) {
            Err(TleCacheError::Fetch(_)) => {}
            Err(TleCacheError::Io { ref source, .. })
                if source.kind() == std::io::ErrorKind::InvalidData =>
            {
                panic!("non-UTF-8 cache file should self-heal as a miss, not surface as Io error")
            }
            Ok(text) => panic!("unexpected Ok with binary cache: {text:?}"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn cache_falls_through_to_fetch_when_fresh_file_disappears() {
        // TOCTOU window: cache file passes the mtime freshness check,
        // then gets deleted (concurrent process, cache cleaner,
        // manual rm) before the read. tle_text() must NOT raise a
        // hard `Io(NotFound)` — it must fall through to the refetch
        // path so the race becomes a recoverable network condition,
        // not a file-not-found bug. Canned-fail fetcher pins the
        // expected outcome to `Fetch(_)` exactly.
        let dir = unique_temp_dir("toctou");
        std::fs::create_dir_all(&dir).unwrap();
        let cache = TleCache::with_dir(dir.clone()).with_fetcher(always_fail_fetcher());
        let path = cache.cache_path(TEST_NORAD);
        std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
        // Sanity: fresh-cache fast path works (doesn't go through fetch).
        assert!(cache.tle_text(TEST_NORAD).is_ok());
        // Race: delete the file before the next call.
        std::fs::remove_file(&path).unwrap();
        match cache.tle_text(TEST_NORAD) {
            Err(TleCacheError::Fetch(_)) => {} // expected: fell through to (failing) fetch
            Err(other @ TleCacheError::Io { .. }) => {
                panic!("TOCTOU race should fall through to fetch, got Io error: {other:?}")
            }
            Ok(text) => panic!("unexpected Ok with deleted cache + failing fetcher: {text:?}"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn cache_uses_injected_fetcher_when_set() {
        // Round-trip the fetcher injection itself: a stale cache
        // path with a canned-OK fetcher should return the canned
        // text, NOT make a network call.
        let dir = unique_temp_dir("inject");
        std::fs::create_dir_all(&dir).unwrap();
        let cache = TleCache::with_dir(dir).with_fetcher(|_| Ok(SAMPLE_TLE_3LINE.to_string()));
        // No file present → goes through the fetch path.
        let text = cache.tle_text(TEST_NORAD).unwrap();
        assert!(text.contains("VANGUARD 1"));
    }

    #[test]
    fn cache_fetcher_receives_requested_norad_id() {
        // The fetcher closure must see the actual NORAD id the caller
        // asked for — otherwise a custom HTTP stack couldn't build the
        // right `gp.php?CATNR=…` query. Round-trip an `AtomicU32` so
        // the test pins the contract regardless of how many times
        // the fetcher is called.
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = unique_temp_dir("fetcher-id");
        std::fs::create_dir_all(&dir).unwrap();
        let last_seen = StdArc::new(AtomicU32::new(0));
        let last_seen_clone = StdArc::clone(&last_seen);
        let cache = TleCache::with_dir(dir).with_fetcher(move |id| {
            last_seen_clone.store(id, Ordering::Relaxed);
            Ok(SAMPLE_TLE_3LINE.to_string())
        });
        let _ = cache.tle_text(33_591).unwrap();
        assert_eq!(last_seen.load(Ordering::Relaxed), 33_591);
    }

    #[test]
    fn cache_returns_text_when_file_is_fresh() {
        let dir = unique_temp_dir("fresh");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{TEST_NORAD}.tle"));
        std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
        let cache = TleCache::with_dir(dir.clone());
        let text = cache.tle_text(TEST_NORAD).unwrap();
        assert!(text.contains("VANGUARD 1"));
    }

    #[test]
    fn cache_returns_tle_pair_when_file_is_fresh() {
        let dir = unique_temp_dir("pair");
        std::fs::create_dir_all(&dir).unwrap();
        // Per-NORAD cache: the fixture body needs to contain NORAD 5
        // and the cache file must be at the path keyed by 5.
        let path = dir.join("5.tle");
        std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
        let cache = TleCache::with_dir(dir);
        let (l1, l2) = cache.tle_for(5).unwrap();
        assert!(l1.starts_with("1 00005"));
        assert!(l2.starts_with("2 00005"));
    }

    #[test]
    fn cache_returns_not_found_when_norad_id_missing_from_fresh_file() {
        // Per-NORAD cache file at id 99999 that contains a different
        // satellite's entries — should surface as NotFound, not as
        // a successful match.
        let dir = unique_temp_dir("missing-id");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("99999.tle");
        std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
        let cache = TleCache::with_dir(dir);
        let err = cache.tle_for(99_999).unwrap_err();
        assert!(matches!(err, TleCacheError::NotFound { norad_id: 99_999 }));
    }

    #[test]
    fn has_any_tle_pair_accepts_real_tle_text() {
        assert!(has_any_tle_pair(SAMPLE_TLE_3LINE));
        assert!(has_any_tle_pair(SAMPLE_TLE_2LINE));
    }

    #[test]
    fn has_any_tle_pair_rejects_html_error_pages() {
        // What a captive portal / proxy 5xx looks like in practice.
        let html = "\
<html><head><title>503 Service Unavailable</title></head>
<body><h1>Service Unavailable</h1>
<p>The server is temporarily unable to service your request.</p>
</body></html>
";
        assert!(!has_any_tle_pair(html));
    }

    #[test]
    fn has_any_tle_pair_rejects_truncated_or_garbage_text() {
        assert!(!has_any_tle_pair(""));
        assert!(!has_any_tle_pair("just some random non-TLE text\n"));
        // Has a `1 NNNNN` line but no matching `2 NNNNN` line — half a
        // pair only, must not pass.
        assert!(!has_any_tle_pair(
            "1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753\n"
        ));
        // Mismatched-id pair — line1 NORAD 5, line2 NORAD 25544.
        assert!(!has_any_tle_pair(
            "1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753\n2 25544  51.6442 211.4001 0001234  92.7501 270.5089 15.49538275234276\n"
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
        let path = cache.cache_path(TEST_NORAD);
        cache.write_cache(&path, "hello cache").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "hello cache");
        // No leftover tempfiles in the cache directory.
        // Tempfiles look like `33591.tmp.12345.0` — `Path::extension()`
        // returns the *last* segment (`"0"`), not `"tmp"`, so we have
        // to scan the filename string for the `.tmp.` infix instead.
        let leftover_tmp_count = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.contains(".tmp."))
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
    /// Process-wide test-only counter so two parallel tests with the
    /// same `tag` can't land on the same scratch path. `SystemTime`
    /// alone isn't enough — `cargo test` defaults to running tests
    /// concurrently within a single process, and on coarse-resolution
    /// clocks (e.g. CI runners with a 1 ms tick) two threads can read
    /// the same `nanos` value.
    static NEXT_TEST_TMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let counter = NEXT_TEST_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("sdr-sat-test-{tag}-{pid}-{nanos}-{counter}"))
    }
}
