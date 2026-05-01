//! Label name lookup. Each ACARS message carries a 2-byte
//! label code identifying its category (Q0 = link test, H1 =
//! crew message, B1 = weather, etc.).
//!
//! The public API ([`lookup`]) returns `Some(name)` for ~80 known
//! labels or `None` for unknown codes. Sources: sigidwiki ACARS
//! labels page, airframes.io public docs, `acarsdeco2` / `vdlm2dec`
//! name tables. ARINC 618 is paywalled; this is a best-effort
//! curated list rather than a verbatim port.

/// Look up the human-readable name for a 2-byte label code.
/// Returns `Some(name)` for known ACARS labels (~80 entries
/// covering OOOI events, position reports, weather, ATC, and
/// ACMS) or `None` for unknown codes. Names are sourced from
/// the sigidwiki ACARS labels page, airframes.io public docs,
/// and the open-source `acarsdeco2` / `vdlm2dec` projects'
/// name tables. ARINC 618 itself is paywalled; `acarsdec`
/// doesn't ship a name table either, so this is a curated
/// best-effort list rather than a verbatim port.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn lookup(code: [u8; 2]) -> Option<&'static str> {
    Some(match &code {
        // OOOI events (numeric pairs)
        b"10" | b"17" => "Arrival info",
        b"11" | b"QA" => "Out (gate)",
        b"12" | b"QB" => "Off (wheels)",
        b"13" | b"QC" => "On (wheels)",
        b"14" | b"QD" => "In (gate)",
        b"15" | b"80" => "Departure",
        b"16" | b"25" | b"32" | b"35" | b"40" | b"44" | b"45" | b"4M" | b"4N" | b"5U" | b"81"
        | b"A6" | b"A8" | b"QH" | b"QM" | b"QP" | b"QQ" | b"QR" => "Position",
        b"1F" | b"82" => "Free text",
        b"1G" => "Gate request",
        b"1H" => "Gate assignment",
        b"1L" | b"2C" | b"30" | b"57" | b"M1" | b"QG" => "Position report",
        b"1S" | b"26" | b"5Z" | b"8S" => "Schedule",
        // Departure clearance + datalink (2x)
        b"20" => "Departure clearance",
        b"21" => "Departure clearance reply",
        b"22" | b"83" => "Pre-departure clearance",
        b"23" => "Datalink expedite",
        b"27" => "Schedule (revision)",
        b"2N" => "Takeoff time",
        b"2Z" => "Destination update",
        // 3x — fuel + maintenance
        b"33" => "Fuel report",
        b"39" => "Maintenance ground report",
        // 5x — OOOI summary
        b"51" => "Ground service request",
        b"52" | b"8A" => "Engine maintenance",
        b"5Y" => "OOOI report",
        // 7x — voice + tests
        b"70" => "Voice contact request",
        b"7A" | b"7B" | b"7C" => "Test message",
        // 8x — dispatch + clearance
        b"8D" => "Dispatch reply",
        b"8E" => "ETA report",
        // A-family
        b"A0" => "Test",
        b"A7" => "Pre-departure clearance request",
        b"A9" => "Pre-departure clearance reply",
        b"AA" | b"Q5" => "Engine data",
        // B-family — weather
        b"B1" | b"BA" => "Weather request",
        b"B2" => "Weather information",
        b"B3" => "Weather (text)",
        b"B4" => "Weather (route)",
        b"B5" => "Weather (terminal)",
        b"B6" => "Weather (en-route)",
        b"B7" => "Weather (clearance)",
        b"B8" => "Weather (SIGMET)",
        b"B9" => "Weather (other)",
        // C-family — ATC
        b"C0" => "Uplink command",
        b"C1" => "ATC",
        b"C2" => "ATC clearance request",
        b"C3" => "ATC reply",
        // H-family — free text
        b"H1" => "Crew message",
        b"H2" => "Free text uplink",
        b"H3" => "Free text downlink",
        // Q-family — control + position + OOOI
        b"Q0" => "Link test",
        b"Q1" => "ATIS",
        b"Q2" => "ACARS network test",
        b"Q3" => "Voice circuit test",
        b"Q4" => "Navaids",
        b"Q6" => "Engine display data",
        b"Q7" => "Component maintenance",
        b"QE" => "OOOI summary",
        b"QF" => "OOOI (extended)",
        b"QK" => "Voice request",
        b"QL" => "ATIS (alt)",
        b"QN" | b"QS" => "Diversion",
        b"QT" => "ACARS request",
        // RB — alias dispatched the same as 26 in label_parsers
        b"RB" => "Schedule (alias for 26)",
        // Underscore prefix — generic up/down link
        b"_d" => "General downlink",
        b"_e" => "General uplink",
        // Unknown
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_labels() {
        // Spot-check 5 canonical labels across the major code
        // families (alphanumeric pair, numeric, underscore prefix,
        // alias). Adding all 80 here would just be a copy of the
        // table in `lookup` itself.
        assert_eq!(lookup(*b"H1"), Some("Crew message"));
        assert_eq!(lookup(*b"Q0"), Some("Link test"));
        assert_eq!(lookup(*b"M1"), Some("Position report"));
        assert_eq!(lookup(*b"_d"), Some("General downlink"));
        assert_eq!(lookup(*b"RB"), Some("Schedule (alias for 26)"));
    }

    #[test]
    fn lookup_numeric_labels() {
        // OOOI events (10-15) are particularly important — the
        // viewer's UI uses them to highlight gate/wheels events.
        assert_eq!(lookup(*b"10"), Some("Arrival info"));
        assert_eq!(lookup(*b"11"), Some("Out (gate)"));
        assert_eq!(lookup(*b"12"), Some("Off (wheels)"));
        assert_eq!(lookup(*b"13"), Some("On (wheels)"));
        assert_eq!(lookup(*b"14"), Some("In (gate)"));
    }

    #[test]
    fn lookup_unknown_returns_none() {
        // Bogus codes outside the known table should still
        // resolve to `None` so the viewer falls back to the bare
        // 2-char code in the column display.
        assert_eq!(lookup([0xFF, 0xFF]), None);
        assert_eq!(lookup(*b"ZZ"), None);
    }
}
