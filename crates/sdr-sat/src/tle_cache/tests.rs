use super::*;

/// Three real TLE entries (Vanguard 1, ISS, NOAA 19) with valid
/// checksums, in 3-line Celestrak format, ready to be parsed.
const SAMPLE_TLE_3LINE: &str = "\
VANGUARD 1
1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753
2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667
ISS (ZARYA)
1 25544U 98067A   20194.88670927  .00002728  00000-0  61021-4 0  9996
2 25544  51.6442 211.4001 0001234  92.7501 270.5089 15.49538275234275
NOAA 19
1 33591U 09005A   24001.50000000  .00000050  00000-0  50000-4 0  9991
2 33591  99.0000 100.0000 0010000  90.0000 270.0000 14.10000000123452
";

/// Same three TLEs but in 2-line format (no name line) — Celestrak
/// also serves this variant from some endpoints. Parser must
/// handle both.
const SAMPLE_TLE_2LINE: &str = "\
1 00005U 58002B   00179.78495062  .00000023  00000-0  28098-4 0  4753
2 00005  34.2682 348.7242 1859667 331.7664  19.3264 10.82419157413667
1 25544U 98067A   20194.88670927  .00002728  00000-0  61021-4 0  9996
2 25544  51.6442 211.4001 0001234  92.7501 270.5089 15.49538275234275
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
1 11111U 99001A   24000.00000000  .00000000  00000-0  10000-3 0  9994
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
2 25544  51.6442 211.4001 0001234  92.7501 270.5089 15.49538275234275
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
1 11111U 99001A   24000.00000000  .00000000  00000-0  10000-3 0  9994
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
    // satellite's entries is corrupt for that id: the refresh path
    // must not serve it, and must refetch. Offline, the fetch
    // error is what surfaces — never a successful match (#720).
    let dir = unique_temp_dir("missing-id");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("99999.tle");
    std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
    let cache = TleCache::with_dir(dir).with_fetcher(always_fail_fetcher());
    let err = cache.tle_for(99_999).unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)), "got {err:?}");
}

#[test]
fn cached_tle_for_does_not_call_network_on_miss() {
    // The whole point of `cached_tle_for`: a UI-thread caller
    // can ask "what TLE do we have on disk?" without risking a
    // synchronous HTTP fetch. Drop a canned-fail fetcher in to
    // pin that — a network call here would surface as Fetch
    // error, but cache_only should never hit that path.
    let dir = unique_temp_dir("cached-only-miss");
    std::fs::create_dir_all(&dir).unwrap();
    let cache = TleCache::with_dir(dir).with_fetcher(always_fail_fetcher());
    // No file present.
    let err = cache.cached_tle_for(33_591).unwrap_err();
    assert!(
        matches!(err, TleCacheError::NotFound { norad_id: 33_591 }),
        "expected NotFound, got {err:?}"
    );
}

#[test]
fn cached_tle_for_returns_pair_when_file_is_present() {
    let dir = unique_temp_dir("cached-only-hit");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("5.tle");
    std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
    let cache = TleCache::with_dir(dir).with_fetcher(always_fail_fetcher());
    let (l1, l2) = cache.cached_tle_for(5).unwrap();
    assert!(l1.starts_with("1 00005"));
    assert!(l2.starts_with("2 00005"));
}

#[test]
fn cached_tle_for_never_invokes_the_fetcher_even_when_stale() {
    // The contract for `cached_tle_for`: zero network calls,
    // regardless of cache freshness. Pin it by counting fetcher
    // invocations — that's the actual UI-thread invariant we
    // care about. (`tle_text` falls back to stale cache on
    // fetch failure, so its return value can't differentiate
    // "fetched and failed" from "served stale", which is why we
    // count fetcher calls instead.)
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicU32, Ordering};
    let dir = unique_temp_dir("cached-only-stale");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("5.tle");
    std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
    // Sleep one tick + use a 1-ns refresh window so the file
    // looks "stale" to anyone who'd consult freshness.
    std::thread::sleep(std::time::Duration::from_millis(2));
    let calls = StdArc::new(AtomicU32::new(0));
    let calls_for_fetcher = StdArc::clone(&calls);
    let cache = TleCache::with_dir(dir)
        .with_refresh_max_age(std::time::Duration::from_nanos(1))
        .with_fetcher(move |_| {
            calls_for_fetcher.fetch_add(1, Ordering::Relaxed);
            Err(TleCacheError::Fetch("test: fetcher disabled".to_string()))
        });
    // Even with a stale cache file, `cached_tle_for` returns
    // the on-disk copy and never calls the fetcher.
    let (l1, _l2) = cache.cached_tle_for(5).unwrap();
    assert!(l1.starts_with("1 00005"));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "cached_tle_for invoked the fetcher",
    );
}

#[test]
fn cached_tle_for_returns_not_found_for_corrupted_body() {
    // Validation gate applies on the read path too — a stray
    // HTML response (or a manual `echo` into the cache) must
    // surface as NotFound rather than be served as garbage.
    let dir = unique_temp_dir("cached-only-corrupt");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("5.tle");
    std::fs::write(&path, "<html>not a TLE</html>\n").unwrap();
    let cache = TleCache::with_dir(dir).with_fetcher(always_fail_fetcher());
    let err = cache.cached_tle_for(5).unwrap_err();
    assert!(matches!(err, TleCacheError::NotFound { norad_id: 5 }));
}

#[test]
fn force_refresh_always_calls_fetcher_even_when_cache_is_fresh() {
    // The whole point of `force_refresh`: a manual Refresh click
    // must trigger a real network call so the timestamp is
    // meaningful, even if the cache happens to be fresh.
    // Inject a fetcher that records call counts and verify
    // it fires.
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicU32, Ordering};
    let dir = unique_temp_dir("force-refresh");
    std::fs::create_dir_all(&dir).unwrap();
    // Pre-populate the cache with a fresh, valid file.
    let path = dir.join("5.tle");
    std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
    let calls = StdArc::new(AtomicU32::new(0));
    let calls_for_fetcher = StdArc::clone(&calls);
    let cache = TleCache::with_dir(dir).with_fetcher(move |_| {
        calls_for_fetcher.fetch_add(1, Ordering::Relaxed);
        Ok(SAMPLE_TLE_3LINE.to_string())
    });
    // Sanity: tle_text would NOT call the fetcher because the
    // cache is fresh.
    let _ = cache.tle_text(5).unwrap();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "tle_text spuriously fetched"
    );
    // force_refresh must always fetch.
    let _ = cache.force_refresh(5).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    // And again — every call hits the network.
    let _ = cache.force_refresh(5).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[test]
fn force_refresh_does_not_fall_back_to_stale_cache_on_failure() {
    // The "Last refreshed" timestamp must mean a real refresh
    // succeeded — so a network failure with a stale-but-valid
    // cache file present must still surface as Err. Otherwise
    // a click on Refresh while offline could march the
    // timestamp forward without any new data.
    let dir = unique_temp_dir("force-refresh-no-fallback");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("5.tle");
    std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
    let cache = TleCache::with_dir(dir).with_fetcher(always_fail_fetcher());
    let err = cache.force_refresh(5).unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)));
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

// --- #720 (Aug 2026 deep review) ---

/// Celestrak's Alpha-5 encoding for catalog numbers ≥ 100 000: the
/// first column is a letter (I and O excluded) worth
/// `(index + 10) · 10 000`.
#[test]
fn norad_id_parses_alpha5_catalog_numbers() {
    assert_eq!(
        norad_id_from_tle_line("1 A0000U 24001A   24001.5"),
        Some(100_000)
    );
    assert_eq!(norad_id_from_tle_line("2 B1234  99.0"), Some(111_234));
    assert_eq!(norad_id_from_tle_line("1 Z9999U"), Some(339_999));
    assert_eq!(norad_id_from_tle_line("1 I0000U"), None, "I is excluded");
    assert_eq!(norad_id_from_tle_line("1 O0000U"), None, "O is excluded");
    assert_eq!(
        norad_id_from_tle_line("1 a0000U"),
        None,
        "lower case is not Alpha-5"
    );
    assert_eq!(norad_id_from_tle_line("1 25544U"), Some(25_544));
}

/// An unreadable cache file (permissions) must not abort the fetch
/// path, and when offline the fetch error — not the read error —
/// is what the caller sees.
#[cfg(target_os = "linux")]
#[test]
fn unreadable_cache_file_neither_blocks_fetch_nor_masks_fetch_errors() {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::metadata("/proc/self").is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid() == 0
    }) {
        return; // root ignores file permissions
    }
    let dir = unique_temp_dir("unreadable-cache");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("5.tle");
    std::fs::write(&path, SAMPLE_TLE_3LINE).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let online = TleCache::with_dir(dir.clone()).with_fetcher(|_| Ok(SAMPLE_TLE_3LINE.to_string()));
    let text = online.tle_text(5).unwrap();
    assert_eq!(text, SAMPLE_TLE_3LINE);

    // The successful refresh rewrote the file readable; make it
    // unreadable again for the offline half.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let offline = TleCache::with_dir(dir.clone()).with_fetcher(always_fail_fetcher());
    let err = offline.tle_text(5).unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)), "got {err:?}");

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A body that is valid TLE text but for a different satellite is
/// a fetch failure: nothing is written under `{id}.tle` and the
/// "last refreshed" clock does not move.
#[test]
fn refresh_rejects_a_body_for_a_different_satellite() {
    let dir = unique_temp_dir("wrong-sat-body");
    std::fs::create_dir_all(&dir).unwrap();
    let cache = TleCache::with_dir(dir.clone()).with_fetcher(|_| Ok(SAMPLE_TLE_3LINE.to_string()));
    let err = cache.force_refresh(99_999).unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)), "got {err:?}");
    assert!(!dir.join("99999.tle").exists(), "nothing may be cached");
    let err = cache.tle_text(99_999).unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)), "got {err:?}");
    assert!(!dir.join("99999.tle").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// CR round 1 on PR #799 — a pair with the right id but fields the
/// SGP4 parser rejects is a fetch failure and is never cached;
/// otherwise `Satellite::from_tle` would refuse it from the cache
/// and the satellite would vanish until the file aged out.
#[test]
fn refresh_rejects_a_pair_the_sgp4_parser_cannot_parse() {
    const MALFORMED: &str = "\
1 99999U 09005A   24001.50000000  .00000050  00000-0  50000-4 0  9994
2 99999  XX.0000 100.0000 0010000  90.0000 270.0000 14.10000000123456
";
    let dir = unique_temp_dir("malformed-pair");
    std::fs::create_dir_all(&dir).unwrap();
    let cache = TleCache::with_dir(dir.clone()).with_fetcher(|_| Ok(MALFORMED.to_string()));
    let err = cache.force_refresh(99_999).unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)), "got {err:?}");
    assert!(!dir.join("99999.tle").exists(), "nothing may be cached");

    // A malformed body already on disk is corrupt for the refresh
    // path too: the fetch error surfaces, not the bad lines.
    std::fs::write(dir.join("99999.tle"), MALFORMED).unwrap();
    let offline = TleCache::with_dir(dir.clone()).with_fetcher(always_fail_fetcher());
    let err = offline.tle_text(99_999).unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)), "got {err:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// CR round 2 on PR #799 — the cache-only path applies the same
/// SGP4-backed validation: a malformed pair for the requested id is
/// `NotFound`, not an `Ok` that `Satellite::from_tle` then rejects.
#[test]
fn cached_tle_for_rejects_a_malformed_requested_pair() {
    const MALFORMED: &str = "\
1 99999U 09005A   24001.50000000  .00000050  00000-0  50000-4 0  9994
2 99999  XX.0000 100.0000 0010000  90.0000 270.0000 14.10000000123456
";
    let dir = unique_temp_dir("cached-malformed");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("99999.tle"), MALFORMED).unwrap();
    let cache = TleCache::with_dir(dir.clone()).with_fetcher(always_fail_fetcher());
    let err = cache.cached_tle_for(99_999).unwrap_err();
    assert!(
        matches!(err, TleCacheError::NotFound { norad_id: 99_999 }),
        "got {err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
