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
    /// **GTK callers — beware:** the freshness check can trigger a
    /// blocking HTTP fetch. For UI-thread paths (e.g. recomputing the
    /// pass list after a lat/lon edit) prefer
    /// [`TleCache::cached_tle_for`], which is read-only and never
    /// touches the network.
    ///
    /// # Errors
    ///
    /// Surfaces [`TleCacheError`] for network, I/O, or lookup failures.
    pub fn tle_for(&self, norad_id: u32) -> Result<(String, String), TleCacheError> {
        let text = self.tle_text(norad_id)?;
        parse_tle_text(&text, norad_id).ok_or(TleCacheError::NotFound { norad_id })
    }

    /// Cache-only lookup. Reads whatever's on disk, validates the
    /// content, and returns the matching TLE pair. **Never** makes a
    /// network call, regardless of cache freshness — staleness is the
    /// caller's problem (the satellites panel reschedules an explicit
    /// refresh button click for that).
    ///
    /// Used by the UI's pass-recompute path so a lat/lon/alt edit
    /// can't accidentally block the GTK main loop on synchronous HTTP
    /// + cache writes.
    ///
    /// # Errors
    ///
    /// * [`TleCacheError::NotFound`] — cache file missing, unreadable,
    ///   structurally invalid, or doesn't contain `norad_id`.
    /// * [`TleCacheError::Io`] — file system read failure (other than
    ///   not-found / non-UTF-8, both of which are demoted to `NotFound`
    ///   so the caller doesn't see a fatal Io for a routine miss).
    pub fn cached_tle_for(&self, norad_id: u32) -> Result<(String, String), TleCacheError> {
        let path = self.cache_path(norad_id);
        let cached = read_file(&path)?.ok_or(TleCacheError::NotFound { norad_id })?;
        // Same SGP4-backed validation as the refresh path: a pair the
        // parser rejects is a miss here, not an `Ok` that
        // `Satellite::from_tle` then refuses (CR on PR #799).
        if !has_valid_tle_for(&cached, norad_id) {
            return Err(TleCacheError::NotFound { norad_id });
        }
        parse_tle_text(&cached, norad_id).ok_or(TleCacheError::NotFound { norad_id })
    }

    /// Forced network fetch. Bypasses the freshness check and the
    /// stale-cache fallback — used by the manual "Refresh" button so
    /// a click always means "do the network round-trip" and the
    /// timestamp can only advance on a real success.
    ///
    /// On success, the response is validated and written to the cache
    /// just like the slow path of [`TleCache::tle_text`].
    ///
    /// # Errors
    ///
    /// * [`TleCacheError::Fetch`] — network failure, non-2xx status,
    ///   or upstream returned a body that isn't a valid TLE (HTML
    ///   error page, captive portal, etc.). The cache is **not**
    ///   updated and the on-disk copy (stale or otherwise) is left
    ///   untouched.
    /// * [`TleCacheError::Io`] — only if writing the freshly-fetched
    ///   body to disk failed; the in-memory body is still returned.
    pub fn force_refresh(&self, norad_id: u32) -> Result<String, TleCacheError> {
        let text = self.fetch_validated(norad_id)?;
        let path = self.cache_path(norad_id);
        if let Err(e) = self.write_cache(&path, &text) {
            tracing::warn!(
                "TLE cache write for NORAD {norad_id} failed ({e}); returning fresh in-memory copy",
            );
        }
        Ok(text)
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
        // A cache file we cannot read (permissions, mid-read failure)
        // is treated like a miss on this path: the network is the
        // authority here, and an I/O error must neither block the
        // fetch nor replace a real fetch error (#720).
        if !is_stale(&path, self.refresh_max_age)
            && let Some(cached) = read_file_or_miss(&path, norad_id)
        {
            if has_valid_tle_for(&cached, norad_id) {
                return Ok(cached);
            }
            tracing::warn!("ignoring corrupted fresh TLE cache for NORAD {norad_id}; refetching");
        }

        // Slow path: fetch from upstream, validate, write to disk.
        // If the fetch (or validation) fails, fall back to whatever
        // stale copy still happens to exist *and validates*. If even
        // that's gone or corrupted, surface the original fetch error.
        match self.fetch_validated(norad_id) {
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
                if let Some(cached) = read_file_or_miss(&path, norad_id)
                    && has_valid_tle_for(&cached, norad_id)
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
    /// Fetch and require a TLE pair **for `norad_id`**. A structurally
    /// valid body for a different satellite used to pass the id-agnostic
    /// check, get written under `{id}.tle` with "last refreshed: now",
    /// and then make the satellite vanish from the list for a day when
    /// `cached_tle_for` found no matching pair (#720).
    fn fetch_validated(&self, norad_id: u32) -> Result<String, TleCacheError> {
        let text = self.fetch(norad_id)?;
        if has_valid_tle_for(&text, norad_id) {
            Ok(text)
        } else if has_any_tle_pair(&text) {
            Err(TleCacheError::Fetch(format!(
                "response body did not contain a TLE pair for NORAD {norad_id}"
            )))
        } else {
            Err(TleCacheError::Fetch(
                "response body did not contain any valid TLE pair (captive portal? HTML error page?)"
                    .to_string(),
            ))
        }
    }

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
        crate::ensure_tls_provider();
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

/// [`read_file`] for the refresh path: any read error is logged and
/// treated as a miss so it cannot block or mask the network path.
fn read_file_or_miss(path: &Path, norad_id: u32) -> Option<String> {
    match read_file(path) {
        Ok(cached) => cached,
        Err(e) => {
            tracing::warn!(
                "TLE cache for NORAD {norad_id} is unreadable ({e}); treating as a miss"
            );
            None
        }
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

/// Does `text` hold a pair for `norad_id` that the SGP4 parser accepts?
/// The structural check in [`parse_tle_text`] only looks at prefixes and
/// catalog numbers; a body with the right id but a bad checksum or a
/// malformed orbital field passed it, got cached, and was then refused
/// by `Satellite::from_tle` until the file aged out (CR on PR #799).
fn has_valid_tle_for(text: &str, norad_id: u32) -> bool {
    parse_tle_text(text, norad_id).is_some_and(|(line1, line2)| {
        crate::sgp4_core::Satellite::from_tle("validation", &line1, &line2).is_ok()
    })
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
    let field = field.trim();
    if let Ok(id) = field.parse::<u32>() {
        return Some(id);
    }
    alpha5_to_norad_id(field)
}

/// Decode an Alpha-5 catalog number (`A0000`–`Z9999`): the first
/// character is a letter standing in for the ten-thousands, `A` = 10
/// through `Z` = 34 with `I` and `O` skipped, followed by four digits.
/// Celestrak mandates it for ids ≥ 100 000; plain digits never parse
/// those, so every such satellite used to be "not a valid TLE" and
/// refetched on every call (#720).
fn alpha5_to_norad_id(field: &str) -> Option<u32> {
    const ALPHA5_LETTERS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const ALPHA5_FIRST_VALUE: u32 = 10;
    const ALPHA5_DIGITS: usize = 4;
    let (letter, digits) = field.split_at_checked(1)?;
    let letter = letter.as_bytes().first()?;
    let index = ALPHA5_LETTERS.iter().position(|l| l == letter)?;
    if digits.len() != ALPHA5_DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let low = digits.parse::<u32>().ok()?;
    let index = u32::try_from(index).ok()?;
    Some((index + ALPHA5_FIRST_VALUE) * 10_000 + low)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
