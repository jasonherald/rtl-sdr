use super::*;

#[test]
fn parse_hz_plain() {
    assert_eq!(parse_hz("2048000").unwrap(), 2_048_000);
}

#[test]
fn parse_hz_kilo() {
    assert_eq!(parse_hz("2400k").unwrap(), 2_400_000);
    assert_eq!(parse_hz("2400K").unwrap(), 2_400_000);
}

#[test]
fn parse_hz_mega() {
    assert_eq!(parse_hz("100M").unwrap(), 100_000_000);
    assert_eq!(parse_hz("100.5M").unwrap(), 100_500_000);
}

#[test]
fn parse_hz_giga() {
    assert_eq!(parse_hz("1G").unwrap(), 1_000_000_000);
}

#[test]
fn parse_defaults_are_loopback_and_upstream_port() {
    let (cfg, _disc) = parse_args::<&str>(&[]).unwrap();
    assert_eq!(cfg.bind.ip().to_string(), "127.0.0.1");
    assert_eq!(cfg.bind.port(), DEFAULT_PORT);
}

#[test]
fn parse_auto_gain_is_none() {
    let args = vec!["-g".to_string(), "0".to_string()];
    let (cfg, _disc) = parse_args(&args).unwrap();
    assert!(cfg.initial.gain_tenths_db.is_none());
}

#[test]
fn parse_manual_gain_rounds_to_tenths() {
    let args = vec!["-g".to_string(), "28.2".to_string()];
    let (cfg, _disc) = parse_args(&args).unwrap();
    assert_eq!(cfg.initial.gain_tenths_db, Some(282));
}

#[test]
fn parse_direct_sampling_hardcodes_mode_2() {
    let args = vec!["-D".to_string()];
    let (cfg, _disc) = parse_args(&args).unwrap();
    assert_eq!(cfg.initial.direct_sampling, 2);
}

#[test]
fn parse_hz_rejects_garbage() {
    assert!(parse_hz("not-a-number").is_err());
    assert!(parse_hz("").is_err());
    assert!(parse_hz("5MHz").is_err()); // trailing "Hz" not valid suffix
    assert!(parse_hz("-100").is_err()); // caught by the `n < 0.0` guard before rounding
}

#[test]
fn parse_hz_rejects_nan_and_infinity() {
    // `f64::parse` accepts these; without the `is_finite` guard the
    // subsequent `as i64` cast silently converts NaN → 0 and ±Inf to
    // saturating bounds — producing plausible-looking output from
    // garbage. Verify both paths reject.
    assert!(parse_hz("NaN").is_err());
    assert!(parse_hz("nan").is_err());
    assert!(parse_hz("inf").is_err());
    assert!(parse_hz("Infinity").is_err());
    assert!(parse_hz("-inf").is_err());
    assert!(parse_hz("NaNk").is_err()); // with suffix too
}

#[test]
fn parse_gain_rejects_nan_and_infinity() {
    for v in ["NaN", "inf", "-inf", "Infinity"] {
        let args = vec!["-g".to_string(), v.to_string()];
        assert!(
            parse_args(&args).is_err(),
            "parse_args should reject -g {v}"
        );
    }
}

#[test]
fn parse_gain_rejects_oversized_finite_values() {
    // Finite f64s large enough that (db * 10.0) overflows i32 must
    // be rejected rather than saturating silently to i32::MAX.
    // Covers the gap that `is_finite()` alone doesn't catch.
    for v in ["1e100", "1e20", "-1e100", "1e10"] {
        let args = vec!["-g".to_string(), v.to_string()];
        assert!(
            parse_args(&args).is_err(),
            "parse_args should reject oversized -g {v}"
        );
    }
}

#[test]
fn parse_gain_accepts_realistic_range() {
    // Sanity check the valid gain range isn't accidentally rejected.
    // RTL-SDR tuner gain table goes ~0..49.6 dB; pick a few plausible values.
    for (v, want) in [
        ("0", None),
        ("14.4", Some(144)),
        ("49.6", Some(496)),
        ("-5", Some(-50)),
    ] {
        let args = vec!["-g".to_string(), v.to_string()];
        let (cfg, _disc) = parse_args(&args).unwrap();
        assert_eq!(cfg.initial.gain_tenths_db, want, "gain {v}");
    }
}

#[test]
fn parse_hz_accepts_fractional_suffix_values() {
    assert_eq!(parse_hz("1.5k").unwrap(), 1_500);
    assert_eq!(parse_hz("0.1M").unwrap(), 100_000);
}

#[test]
fn parse_hz_rejects_negative_fractional_before_rounding() {
    // `parse_hz("-0.4")` would previously round to 0, pass the
    // u32 range check, and be accepted as a plausible 0 Hz
    // frequency. The pre-cast `n < 0.0` guard catches it as a
    // parse error instead.
    assert!(parse_hz("-0.4").is_err());
    assert!(parse_hz("-0.5").is_err());
    assert!(parse_hz("-0.4k").is_err());
    assert!(parse_hz("-1e-10").is_err());
}

#[test]
fn parse_hz_overflows_u32_rejected() {
    // 5 GHz > u32::MAX (~4.29 GHz)
    assert!(parse_hz("5G").is_err());
}

#[test]
fn parse_args_missing_value_rejected() {
    // -a requires an argument
    let args = vec!["-a".to_string()];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_unknown_flag_rejected() {
    let args = vec!["-X".to_string()];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_rejects_zero_sample_rate() {
    // `parse_hz` accepts "0" as a valid non-negative u32, but a
    // zero sample rate wedges the RTL-SDR USB controller. Reject
    // up-front.
    for v in ["0", "0k", "0.0", "0M"] {
        let args = vec!["-s".to_string(), v.to_string()];
        assert!(
            parse_args(&args).is_err(),
            "parse_args should reject -s {v}"
        );
    }
}

#[test]
fn parse_args_invalid_port_rejected() {
    let args = vec!["-p".to_string(), "not-a-port".to_string()];
    assert!(parse_args(&args).is_err());

    // Port > u16::MAX is rejected by the u16 parser.
    let args = vec!["-p".to_string(), "99999".to_string()];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_help_flag_exits() {
    // `-h` and `--help` both return Err so `main` calls `usage()`
    // which exits — don't call `main` in a test, just verify.
    assert!(parse_args(&["-h".to_string()]).is_err());
    assert!(parse_args(&["--help".to_string()]).is_err());
}

#[test]
fn parse_args_malformed_ip_rejected() {
    let args = vec!["-a".to_string(), "not.an.ip".to_string()];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_all_flags_together() {
    // Exercise the full parse path with every option set, so future
    // refactors of the match keep the option ordering flexibility.
    let args: Vec<String> = [
        "-a", "10.0.0.5", "-p", "12345", "-f", "433.92M", "-g", "19.7", "-s", "1800k", "-d", "1",
        "-P", "-5", "-n", "250", "-T", "-D",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let (cfg, _disc) = parse_args(&args).unwrap();
    assert_eq!(cfg.bind.ip().to_string(), "10.0.0.5");
    assert_eq!(cfg.bind.port(), 12_345);
    assert_eq!(cfg.initial.center_freq_hz, 433_920_000);
    assert_eq!(cfg.initial.gain_tenths_db, Some(197));
    assert_eq!(cfg.initial.sample_rate_hz, 1_800_000);
    assert_eq!(cfg.device_index, 1);
    assert_eq!(cfg.initial.ppm, -5);
    assert_eq!(cfg.buffer_capacity, 250);
    assert!(cfg.initial.bias_tee);
    assert_eq!(cfg.initial.direct_sampling, 2);
}

#[test]
fn parse_args_listener_cap_defaults_to_crate_default() {
    // No `--listener-cap` flag → `ServerConfig::default_loopback`
    // value, which is [`DEFAULT_LISTENER_CAP`]. Pins the contract
    // that omitting the flag doesn't silently zero the cap.
    let (cfg, _disc) = parse_args::<&str>(&[]).unwrap();
    assert_eq!(cfg.listener_cap, DEFAULT_LISTENER_CAP);
}

#[test]
fn parse_args_listener_cap_override() {
    // Explicit `--listener-cap N` overrides the default.
    let args = ["--listener-cap", "25"];
    let (cfg, _disc) = parse_args(&args).unwrap();
    assert_eq!(cfg.listener_cap, 25);
}

#[test]
fn parse_args_auth_key_defaults_to_none() {
    let (cfg, _disc) = parse_args::<&str>(&[]).unwrap();
    assert!(cfg.auth_key.is_none());
}

#[test]
fn parse_args_auth_key_decodes_valid_hex() {
    // 8-byte key in hex form → 16 hex chars.
    let args = ["--auth-key", "deadbeef01020304"];
    let (cfg, _disc) = parse_args(&args).unwrap();
    assert_eq!(
        cfg.auth_key,
        Some(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04])
    );
}

#[test]
fn parse_args_auth_key_decodes_32_byte_default() {
    // 32-byte key (the default `generate_random_auth_key`
    // shape) → 64 hex chars. Pins that the CLI parser
    // handles the canonical server-generated length, which
    // is what operators will paste from the UI (once #395
    // ships) or `openssl rand -hex 32`.
    let mut hex = String::with_capacity(64);
    {
        use std::fmt::Write;
        for i in 0..32u32 {
            write!(&mut hex, "{:02x}", (i * 7) % 256).unwrap();
        }
    }
    let args = ["--auth-key", hex.as_str()];
    let (cfg, _disc) = parse_args(&args).unwrap();
    let key = cfg.auth_key.unwrap();
    assert_eq!(key.len(), 32);
    assert_eq!(key[0], 0x00);
    assert_eq!(key[1], 0x07);
    assert_eq!(key[2], 0x0E);
}

#[test]
fn parse_args_auth_key_rejects_odd_length_hex() {
    // Odd-length hex can't round-trip to bytes — reject
    // at argv-parse time rather than truncating silently.
    let args = ["--auth-key", "abc"];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_auth_key_rejects_non_hex_chars() {
    // 'z' isn't a valid hex digit.
    let args = ["--auth-key", "zzzzzzzz"];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_auth_key_rejects_non_ascii() {
    // **Regression test for `CodeRabbit` round 1 on PR #405.**
    // Non-ASCII input must fail cleanly instead of panicking
    // on UTF-8 byte-boundary slicing. 4-byte emojis like
    // "💩" pass `len().is_multiple_of(2)` (`len() == 4`), so
    // the early-exit length check alone wouldn't catch it;
    // the `is_ascii()` guard in `parse_auth_key_hex` does.
    let args = ["--auth-key", "💩"];
    assert!(parse_args(&args).is_err());

    // Mix of ASCII hex + non-ASCII also rejected.
    let mixed_args = ["--auth-key", "ab💩cd"];
    assert!(parse_args(&mixed_args).is_err());
}

#[test]
fn parse_args_auth_key_rejects_empty() {
    let args = ["--auth-key", ""];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_auth_key_rejects_over_max_length() {
    // MAX_AUTH_KEY_LEN = 256 bytes → 512 hex chars. Anything
    // beyond that would fail at AuthKeyMessage serialization
    // downstream; catch here so the error surfaces as a
    // clean argv-parse fail.
    let hex: String = "ab".repeat(sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN + 1);
    let args = ["--auth-key", hex.as_str()];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_listener_cap_rejects_non_numeric() {
    // Non-u32 argument surfaces as `ParseError` — users get a
    // clean "bad args" exit code instead of a silent fallback
    // that would confuse "why is my cap still 10?"
    let args = ["--listener-cap", "lots"];
    assert!(parse_args(&args).is_err());
}

#[test]
fn parse_args_discovery_defaults_on() {
    let (_cfg, disc) = parse_args::<&str>(&[]).unwrap();
    assert!(disc.announce, "mDNS advertise should default to on");
    assert!(disc.nickname.is_none());
}

#[test]
fn parse_no_announce_flag_disables_mdns() {
    let args = vec!["--no-announce".to_string()];
    let (_cfg, disc) = parse_args(&args).unwrap();
    assert!(!disc.announce);
}

#[test]
fn parse_nickname_flag_captures_name() {
    let args = vec!["-N".to_string(), "attic-pi".to_string()];
    let (_cfg, disc) = parse_args(&args).unwrap();
    assert_eq!(disc.nickname.as_deref(), Some("attic-pi"));
}

#[test]
fn parse_bind_override() {
    let args = vec!["-a".to_string(), "0.0.0.0".to_string()];
    let (cfg, _disc) = parse_args(&args).unwrap();
    assert_eq!(cfg.bind.ip().to_string(), "0.0.0.0");
}
