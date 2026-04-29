//! End-to-end compatibility test: run sdr-acars-cli on the
//! shipped acarsdec test.wav, strip volatile fields, diff
//! against the committed acarsdec snapshot. This is the
//! correctness oracle for the entire DSP + parser stack.
//!
//! The snapshot is regenerated manually — see
//! `tests/fixtures/REGENERATE.md`. Running this test never
//! invokes the C acarsdec; that's intentional (deterministic
//! CI, no external tool dependency).

use std::{path::PathBuf, process::Command};

/// Strip volatile fields from each line of acarsdec-format output.
///
/// Matches the sed regex used in REGENERATE.md so committed snapshot
/// and fresh CLI output normalize identically:
///
/// ```text
/// ^\[#[0-9]+ \(L:[^)]+\)[ 0-9.]*--   →   [#X (L:N E:N) --
/// ```
///
/// Volatile fields covered: `#<seq>`, `L:<level>`, `E:<errors>`,
/// `<timestamp>`. Hand-rolled (rather than pulling in a regex crate)
/// because the pattern is anchored at the start of the line and uses
/// simple character classes — a `find`/`char` walk is sufficient.
fn strip_volatile(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        if let Some(stripped) = strip_volatile_line(line) {
            out.push_str(&stripped);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// If `line` begins with the volatile header pattern, return the line
/// with that prefix replaced by `[#X (L:N E:N) --`. Otherwise return
/// `None` so the caller can pass the line through unchanged.
fn strip_volatile_line(line: &str) -> Option<String> {
    // 1. Must start with `[#`.
    let rest = line.strip_prefix("[#")?;

    // 2. One-or-more digits.
    let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    let rest = &rest[digits_end..];

    // 3. ` (L:`.
    let rest = rest.strip_prefix(" (L:")?;

    // 4. Up through the closing `)` (must be at least one char inside).
    let close_idx = rest.find(')')?;
    if close_idx == 0 {
        return None;
    }
    let rest = &rest[close_idx + 1..];

    // 5. Zero-or-more of ` 0-9.`, then literal `--`.
    let trail_end = rest
        .find(|c: char| !(c == ' ' || c == '.' || c.is_ascii_digit()))
        .unwrap_or(rest.len());
    let trailing = &rest[trail_end..];
    let after_dashes = trailing.strip_prefix("--")?;

    let mut result = String::with_capacity(line.len());
    result.push_str("[#X (L:N E:N) --");
    result.push_str(after_dashes);
    Some(result)
}

#[test]
#[allow(clippy::panic)]
fn sdr_acars_cli_matches_acarsdec_on_test_wav() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let test_wav = crate_root.join("tests/data/acars_test.wav");
    assert!(
        test_wav.exists(),
        "test.wav missing at {test_wav:?} — vendored at \
         crates/sdr-acars/tests/data/acars_test.wav"
    );

    let cli_bin = env!("CARGO_BIN_EXE_sdr-acars-cli");
    let output = Command::new(cli_bin)
        .arg(&test_wav)
        .output()
        .expect("running sdr-acars-cli");

    assert!(
        output.status.success(),
        "sdr-acars-cli failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let actual = strip_volatile(&String::from_utf8_lossy(&output.stdout));
    let expected_path = crate_root.join("tests/fixtures/acarsdec_test_wav_expected.txt");
    let expected = strip_volatile(
        &std::fs::read_to_string(&expected_path).expect("snapshot fixture readable"),
    );

    if actual != expected {
        // On mismatch, dump the actual side-by-side for diagnosis.
        let actual_dump = std::env::temp_dir().join("sdr-acars-actual.txt");
        let _ = std::fs::write(&actual_dump, &actual);
        panic!(
            "sdr-acars-cli output differs from acarsdec snapshot.\n  \
             Snapshot: {}\n  \
             Actual (stripped): {}\n  \
             Run: diff {} {}",
            expected_path.display(),
            actual_dump.display(),
            expected_path.display(),
            actual_dump.display(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::strip_volatile;

    #[test]
    fn strips_acarsdec_format_with_two_trailing_spaces() {
        let input = "[#3 (L: -4.6 E:0)  --------------------------------\nMode : E\n";
        let expected = "[#X (L:N E:N) --------------------------------\nMode : E\n";
        assert_eq!(strip_volatile(input), expected);
    }

    #[test]
    fn strips_our_format_with_unix_timestamp() {
        let input = "[#3 (L: +0.0 E:0) 1777469730.394 --------------------------------\nbody\n";
        let expected = "[#X (L:N E:N) --------------------------------\nbody\n";
        assert_eq!(strip_volatile(input), expected);
    }

    #[test]
    fn passes_non_header_lines_through_unchanged() {
        let input = "Mode : E Label : 5V Id : 4 Nak\nNo: S53A\n";
        assert_eq!(strip_volatile(input), input);
    }

    #[test]
    fn leaves_non_matching_brackets_alone() {
        // Looks similar but missing the (L:…) middle.
        let input = "[#5 something else\n";
        assert_eq!(strip_volatile(input), input);
    }
}
