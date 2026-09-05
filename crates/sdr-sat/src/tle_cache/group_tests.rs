use super::*;

#[test]
fn parse_group_tles_extracts_all_named_entries() {
    let body = "\
ORBCOMM FM06
1 25118U 97084G   26248.14598269  .00000608  00000+0  20228-3 0  9991
2 25118  45.0146  27.7326 0001119 213.3371 317.0700 14.47727064505974
ORBCOMM FM04
1 25159U 98007C   26248.16811710  .00000396  00000+0  18072-3 0  9991
2 25159 107.9604 334.0349 0041099 188.9146 171.1268 14.35849962488071
";
    let got = parse_group_tles(body);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, "ORBCOMM FM06");
    assert!(got[0].1.starts_with("1 25118"));
    assert!(got[0].2.starts_with("2 25118"));
    assert_eq!(got[1].0, "ORBCOMM FM04");
}

/// `CodeRabbit` review on #903 (Major): a `1 …`/`2 …` pair that merely
/// matches the prefix shape but fails SGP4 parsing (truncated numeric
/// fields here) must be dropped, not returned — otherwise it could
/// slip past `force_refresh_group`'s `is_empty()` guard and poison the
/// cache. Only the well-formed, SGP4-valid entry survives.
#[test]
fn parse_group_tles_rejects_sgp4_invalid_pairs() {
    // A stray line with no following element lines is ignored, and the
    // truncated/malformed numeric fields on the first TLE-shaped pair
    // fail SGP4 validation and are dropped.
    let body = "GARBAGE\nnot a tle line\nORBCOMM FM06\n1 25118U 97084G   26248.1 .0 0 0 0 9991\n2 25118  45.0 27.7 0001 213 317 14.47\n";
    let got = parse_group_tles(body);
    assert!(
        got.is_empty(),
        "SGP4-invalid TLE lines must be dropped, got {got:?}"
    );
}

#[test]
fn group_cache_is_fresh_true_for_freshly_written_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TleCache::with_dir(dir.path().to_path_buf());
    let path = cache.group_cache_path("ORBCOMM");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "fresh").unwrap();
    assert!(cache.group_cache_is_fresh("ORBCOMM"));
}

#[test]
fn group_cache_is_fresh_false_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TleCache::with_dir(dir.path().to_path_buf());
    assert!(!cache.group_cache_is_fresh("ORBCOMM"));
}

/// `CodeRabbit` review on #903: back-to-back nameless 2LE entries (no
/// name line between them at all) must never let one entry's `2 …`
/// data line get mistaken for the *next* entry's name — it must fall
/// back to `"UNKNOWN"` instead.
#[test]
fn parse_group_tles_rejects_data_line_names() {
    let body = "\
1 25118U 97084G   26248.14598269  .00000608  00000+0  20228-3 0  9991
2 25118  45.0146  27.7326 0001119 213.3371 317.0700 14.47727064505974
1 25159U 98007C   26248.16811710  .00000396  00000+0  18072-3 0  9991
2 25159 107.9604 334.0349 0041099 188.9146 171.1268 14.35849962488071
";
    let got = parse_group_tles(body);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, "UNKNOWN");
    assert_eq!(got[1].0, "UNKNOWN");
    assert!(
        !got[1].0.starts_with("1 ") && !got[1].0.starts_with("2 "),
        "second entry's name must not be the first entry's data line: {:?}",
        got[1].0
    );
}

/// Real, SGP4-valid ORBCOMM TLE fixture (two satellites) shared by the
/// `force_refresh_group` tests below.
const VALID_GROUP_BODY: &str = "\
ORBCOMM FM06
1 25118U 97084G   26248.14598269  .00000608  00000+0  20228-3 0  9991
2 25118  45.0146  27.7326 0001119 213.3371 317.0700 14.47727064505974
ORBCOMM FM04
1 25159U 98007C   26248.16811710  .00000396  00000+0  18072-3 0  9991
2 25159 107.9604 334.0349 0041099 188.9146 171.1268 14.35849962488071
";

#[test]
fn force_refresh_group_writes_and_reads_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TleCache::with_dir(dir.path().to_path_buf()).with_group_fetcher(
        std::sync::Arc::new(|_slug: &str| Ok(VALID_GROUP_BODY.to_string())),
    );
    let fetched = cache.force_refresh_group("ORBCOMM").unwrap();
    assert_eq!(fetched.len(), 2);
    // Cache-only read now returns the same without a fetcher hit.
    let cached = cache.cached_group_tles("ORBCOMM").unwrap();
    assert_eq!(cached[0].0, "ORBCOMM FM06");
}

/// `CodeRabbit` review on #903 (Major): unlike the per-NORAD
/// `force_refresh` (which routes through `fetch_validated`),
/// `force_refresh_group` used to write whatever the fetch returned —
/// a captive-portal/HTML 200 response would overwrite a good cache,
/// parse to empty, AND reset the 24h freshness clock. Guard must
/// reject a body with no valid TLE pair BEFORE writing, and must
/// leave a pre-existing good cache file untouched.
#[test]
fn force_refresh_group_rejects_non_tle_body() {
    let dir = tempfile::tempdir().unwrap();

    // Seed a good group cache first via a good fetcher.
    let cache = TleCache::with_dir(dir.path().to_path_buf()).with_group_fetcher(
        std::sync::Arc::new(|_slug: &str| Ok(VALID_GROUP_BODY.to_string())),
    );
    let fetched = cache.force_refresh_group("ORBCOMM").unwrap();
    assert_eq!(fetched.len(), 2);
    assert_eq!(fetched[0].0, "ORBCOMM FM06");

    // Swap to a garbage (HTML/captive-portal) fetcher and try again.
    let cache = TleCache::with_dir(dir.path().to_path_buf()).with_group_fetcher(
        std::sync::Arc::new(|_slug: &str| Ok("<html>portal</html>".to_string())),
    );
    let err = cache.force_refresh_group("ORBCOMM").unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)), "got {err:?}");

    // The pre-existing good cache file must NOT have been overwritten.
    let cached = cache.cached_group_tles("ORBCOMM").unwrap();
    assert_eq!(cached.len(), 2);
    assert_eq!(cached[0].0, "ORBCOMM FM06");

    // A body whose lines are prefix-shaped like a TLE pair (`1 `/`2 `)
    // but fail SGP4 validation must also be rejected — this is the
    // CodeRabbit #903 regression: prefix-matching-but-invalid entries
    // used to slide past `parse_group_tles(&body).is_empty()` and
    // overwrite a good cache.
    let cache = TleCache::with_dir(dir.path().to_path_buf()).with_group_fetcher(
        std::sync::Arc::new(|_slug: &str| {
            Ok("ORBCOMM BOGUS\n1 25118U 97084G   26248.1 .0 0 0 0 9991\n2 25118  45.0 27.7 0001 213 317 14.47\n".to_string())
        }),
    );
    let err = cache.force_refresh_group("ORBCOMM").unwrap_err();
    assert!(matches!(err, TleCacheError::Fetch(_)), "got {err:?}");
    let cached = cache.cached_group_tles("ORBCOMM").unwrap();
    assert_eq!(cached.len(), 2);
    assert_eq!(cached[0].0, "ORBCOMM FM06");
}
