# RadioReference Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow users to browse RadioReference.com frequencies by zip code and import them as bookmarks, with secure credential storage and a new Preferences window.

**Architecture:** New `sdr-radioreference` crate for SOAP client + types + mode mapping. `KeyringStore` in `sdr-config` for OS keyring access. `AdwPreferencesWindow` and `AdwDialog`-based browse UI in `sdr-ui`. SOAP calls run on GLib thread pool via `gio::spawn_blocking()`.

**Tech Stack:** `reqwest` (blocking), `quick-xml`, `keyring` (sync-secret-service), GTK4/libadwaita 1.5

**Design Spec:** `docs/superpowers/specs/2026-04-08-radioreference-integration-design.md`

---

## File Structure

### New Files

```
crates/sdr-radioreference/
  Cargo.toml
  src/
    lib.rs              -- RrClient public API
    soap.rs             -- SOAP envelope construction + HTTP transport + XML parsing
    types.rs            -- ZipInfo, RrFrequency, RrTag response structs
    mode_map.rs         -- RR mode string -> demod mode + bandwidth

crates/sdr-ui/src/
  preferences/
    mod.rs              -- AdwPreferencesWindow construction
    general_page.rs     -- Recording/screenshot directory settings
    accounts_page.rs    -- RadioReference credential entry + test & save
  radioreference/
    mod.rs              -- AdwDialog browse UI, search flow, import
    frequency_list.rs   -- GtkListBox with check rows, filtering, dedup
```

### Modified Files

```
Cargo.toml                                      -- workspace members + deps
crates/sdr-config/Cargo.toml                    -- add keyring dep
crates/sdr-config/src/lib.rs                    -- add KeyringStore module
crates/sdr-ui/Cargo.toml                        -- add sdr-radioreference dep
crates/sdr-ui/src/lib.rs                        -- add preferences + radioreference modules
crates/sdr-ui/src/sidebar/navigation_panel.rs   -- add rr_category + rr_import_id to Bookmark
crates/sdr-ui/src/window.rs                     -- Preferences menu item, RR header button, wiring
```

---

## Task 1: sdr-radioreference Crate — Mode Mapping

**Files:**
- Create: `crates/sdr-radioreference/Cargo.toml`
- Create: `crates/sdr-radioreference/src/lib.rs`
- Create: `crates/sdr-radioreference/src/mode_map.rs`
- Modify: `Cargo.toml:34-50` (workspace members + deps)

- [ ] **Step 1: Add workspace dependencies and crate member**

In `Cargo.toml`, add to `[workspace]` members:

```toml
"crates/sdr-radioreference",
```

Add to `[workspace.dependencies]`:

```toml
# RadioReference SOAP client
sdr-radioreference = { path = "crates/sdr-radioreference" }
reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls"] }
quick-xml = "0.37"
```

- [ ] **Step 2: Create crate Cargo.toml**

Create `crates/sdr-radioreference/Cargo.toml`:

```toml
[package]
name = "sdr-radioreference"
version = "0.1.0"
description = "RadioReference.com SOAP API client"
edition.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
sdr-types.workspace = true
reqwest.workspace = true
quick-xml.workspace = true
thiserror.workspace = true
tracing.workspace = true
serde.workspace = true

[lints]
workspace = true
```

- [ ] **Step 3: Create lib.rs stub**

Create `crates/sdr-radioreference/src/lib.rs`:

```rust
pub mod mode_map;
```

- [ ] **Step 4: Write failing tests for mode mapping**

Create `crates/sdr-radioreference/src/mode_map.rs`:

```rust
//! Map RadioReference mode strings to SDR-RS demod modes and default bandwidths.

/// Result of mapping an RR mode string to our demod system.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedMode {
    /// Our demod mode name: "NFM", "WFM", "AM", "USB", "LSB", "CW", "RAW".
    pub demod_mode: &'static str,
    /// Default bandwidth in Hz for this mode.
    pub bandwidth: f64,
}

/// Map a RadioReference mode string to our demod mode and default bandwidth.
///
/// Unknown modes default to NFM 12,500 Hz since most public safety
/// traffic is narrowband FM.
pub fn map_rr_mode(rr_mode: &str) -> MappedMode {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fm_maps_to_nfm() {
        let m = map_rr_mode("FM");
        assert_eq!(m.demod_mode, "NFM");
        assert!((m.bandwidth - 12_500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fmn_maps_to_nfm() {
        let m = map_rr_mode("FMN");
        assert_eq!(m.demod_mode, "NFM");
        assert!((m.bandwidth - 12_500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn fmw_maps_to_wfm() {
        let m = map_rr_mode("FMW");
        assert_eq!(m.demod_mode, "WFM");
        assert!((m.bandwidth - 150_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn am_maps_to_am() {
        let m = map_rr_mode("AM");
        assert_eq!(m.demod_mode, "AM");
        assert!((m.bandwidth - 10_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usb_maps_to_usb() {
        let m = map_rr_mode("USB");
        assert_eq!(m.demod_mode, "USB");
        assert!((m.bandwidth - 2_800.0).abs() < f64::EPSILON);
    }

    #[test]
    fn lsb_maps_to_lsb() {
        let m = map_rr_mode("LSB");
        assert_eq!(m.demod_mode, "LSB");
        assert!((m.bandwidth - 2_800.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cw_maps_to_cw() {
        let m = map_rr_mode("CW");
        assert_eq!(m.demod_mode, "CW");
        assert!((m.bandwidth - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_defaults_to_nfm() {
        let m = map_rr_mode("P25");
        assert_eq!(m.demod_mode, "NFM");
        assert!((m.bandwidth - 12_500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn case_insensitive() {
        let m = map_rr_mode("fm");
        assert_eq!(m.demod_mode, "NFM");
    }
}
```

- [ ] **Step 5: Run tests to verify they fail**

Run: `cargo test -p sdr-radioreference`
Expected: FAIL — `todo!()` panics

- [ ] **Step 6: Implement mode mapping**

Replace `todo!()` in `map_rr_mode`:

```rust
pub fn map_rr_mode(rr_mode: &str) -> MappedMode {
    match rr_mode.to_uppercase().as_str() {
        "FM" | "FMN" => MappedMode {
            demod_mode: "NFM",
            bandwidth: 12_500.0,
        },
        "FMW" => MappedMode {
            demod_mode: "WFM",
            bandwidth: 150_000.0,
        },
        "AM" => MappedMode {
            demod_mode: "AM",
            bandwidth: 10_000.0,
        },
        "USB" => MappedMode {
            demod_mode: "USB",
            bandwidth: 2_800.0,
        },
        "LSB" => MappedMode {
            demod_mode: "LSB",
            bandwidth: 2_800.0,
        },
        "CW" => MappedMode {
            demod_mode: "CW",
            bandwidth: 500.0,
        },
        _ => MappedMode {
            demod_mode: "NFM",
            bandwidth: 12_500.0,
        },
    }
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p sdr-radioreference`
Expected: All 9 tests PASS

- [ ] **Step 8: Run clippy**

Run: `cargo clippy -p sdr-radioreference -- -D warnings`
Expected: Clean

- [ ] **Step 9: Commit**

```bash
git add crates/sdr-radioreference/ Cargo.toml Cargo.lock
git commit -m "add sdr-radioreference crate with mode mapping"
```

---

## Task 2: sdr-radioreference — SOAP Types and Transport

**Files:**
- Create: `crates/sdr-radioreference/src/types.rs`
- Create: `crates/sdr-radioreference/src/soap.rs`
- Modify: `crates/sdr-radioreference/src/lib.rs`

- [ ] **Step 1: Create response types**

Create `crates/sdr-radioreference/src/types.rs`:

```rust
//! RadioReference API response types.

/// Zip code lookup result from `getZipcodeInfo`.
#[derive(Debug, Clone)]
pub struct ZipInfo {
    /// County ID for use in subsequent queries.
    pub county_id: u32,
    /// State ID.
    pub state_id: u32,
    /// City name.
    pub city: String,
    /// County name.
    pub county_name: String,
    /// State name.
    pub state_name: String,
}

/// A tag/category associated with a frequency.
#[derive(Debug, Clone)]
pub struct RrTag {
    /// Tag ID.
    pub id: u32,
    /// Tag description (e.g., "Law Dispatch", "Fire Dispatch").
    pub description: String,
}

/// A frequency entry from RadioReference.
#[derive(Debug, Clone)]
pub struct RrFrequency {
    /// RadioReference unique frequency ID.
    pub id: String,
    /// Output/downlink frequency in Hz.
    pub freq_hz: u64,
    /// Raw RR mode string (e.g., "FM", "FMN", "AM").
    pub mode: String,
    /// CTCSS/PL tone in Hz, if any.
    pub tone: Option<f32>,
    /// Description text.
    pub description: String,
    /// Alpha tag / display label.
    pub alpha_tag: String,
    /// Category tags associated with this frequency.
    pub tags: Vec<RrTag>,
}
```

- [ ] **Step 2: Create SOAP envelope builder and XML parser**

Create `crates/sdr-radioreference/src/soap.rs`:

```rust
//! SOAP envelope construction and XML response parsing for RadioReference API.

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};

use crate::types::{RrFrequency, RrTag, ZipInfo};

/// RadioReference SOAP API endpoint.
const SOAP_ENDPOINT: &str = "https://api.radioreference.com/soap2/index.php";

/// API version.
const API_VERSION: &str = "18";

/// SOAP XML namespace constants.
const NS_SOAP: &str = "http://schemas.xmlsoap.org/soap/envelope/";
const NS_XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
const NS_XSD: &str = "http://www.w3.org/2001/XMLSchema";
const NS_TNS: &str = "http://api.radioreference.com/soap2";
const NS_ENC: &str = "http://schemas.xmlsoap.org/soap/encoding/";

/// Error type for SOAP operations.
#[derive(Debug, thiserror::Error)]
pub enum SoapError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("XML parse error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("SOAP fault: {0}")]
    Fault(String),
    #[error("unexpected response: {0}")]
    Unexpected(String),
    #[error("authentication failed")]
    AuthFailed,
}

/// Credentials for SOAP requests.
pub struct SoapAuth {
    pub username: String,
    pub password: String,
    pub app_key: String,
}

/// Build a complete SOAP envelope for a method call.
///
/// The `body_fn` closure writes method-specific parameters into the
/// method element using the provided `Writer`.
fn build_envelope<F>(method: &str, auth: &SoapAuth, body_fn: F) -> Result<Vec<u8>, SoapError>
where
    F: FnOnce(&mut Writer<Vec<u8>>) -> Result<(), quick_xml::Error>,
{
    let mut writer = Writer::new(Vec::new());

    // XML declaration
    writer.write_event(Event::Decl(quick_xml::events::BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    // SOAP Envelope
    let mut envelope = BytesStart::new("SOAP-ENV:Envelope");
    envelope.push_attribute(("xmlns:SOAP-ENV", NS_SOAP));
    envelope.push_attribute(("xmlns:SOAP-ENC", NS_ENC));
    envelope.push_attribute(("xmlns:xsi", NS_XSI));
    envelope.push_attribute(("xmlns:xsd", NS_XSD));
    envelope.push_attribute(("xmlns:tns", NS_TNS));
    writer.write_event(Event::Start(envelope))?;

    // Empty header
    writer.write_event(Event::Empty(BytesStart::new("SOAP-ENV:Header")))?;

    // Body
    writer.write_event(Event::Start(BytesStart::new("SOAP-ENV:Body")))?;

    // Method element
    let method_tag = format!("tns:{method}");
    writer.write_event(Event::Start(BytesStart::new(&method_tag)))?;

    // Method-specific parameters
    body_fn(&mut writer)?;

    // authInfo block
    write_auth_info(&mut writer, auth)?;

    // Close method, body, envelope
    writer.write_event(Event::End(BytesEnd::new(&method_tag)))?;
    writer.write_event(Event::End(BytesEnd::new("SOAP-ENV:Body")))?;
    writer.write_event(Event::End(BytesEnd::new("SOAP-ENV:Envelope")))?;

    Ok(writer.into_inner())
}

/// Write the authInfo element into the SOAP body.
fn write_auth_info(writer: &mut Writer<Vec<u8>>, auth: &SoapAuth) -> Result<(), quick_xml::Error> {
    let mut ai = BytesStart::new("authInfo");
    ai.push_attribute(("xsi:type", "tns:authInfo"));
    writer.write_event(Event::Start(ai))?;

    write_typed_element(writer, "appKey", "xsd:string", &auth.app_key)?;
    write_typed_element(writer, "username", "xsd:string", &auth.username)?;
    write_typed_element(writer, "password", "xsd:string", &auth.password)?;
    write_typed_element(writer, "version", "xsd:string", API_VERSION)?;
    write_typed_element(writer, "style", "xsd:string", "rpc")?;

    writer.write_event(Event::End(BytesEnd::new("authInfo")))?;
    Ok(())
}

/// Write a single typed element: `<name xsi:type="type">value</name>`.
fn write_typed_element(
    writer: &mut Writer<Vec<u8>>,
    name: &str,
    xsi_type: &str,
    value: &str,
) -> Result<(), quick_xml::Error> {
    let mut elem = BytesStart::new(name);
    elem.push_attribute(("xsi:type", xsi_type));
    writer.write_event(Event::Start(elem))?;
    writer.write_event(Event::Text(BytesText::new(value)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))?;
    Ok(())
}

/// Send a SOAP request and return the raw XML response body.
fn send_request(client: &reqwest::blocking::Client, envelope: &[u8]) -> Result<String, SoapError> {
    let response = client
        .post(SOAP_ENDPOINT)
        .header("Content-Type", "text/xml;charset=UTF-8")
        .body(envelope.to_vec())
        .send()?;

    let status = response.status();
    let body = response.text()?;

    if !status.is_success() {
        // Check for SOAP fault in error response
        if let Some(fault) = extract_soap_fault(&body) {
            if fault.contains("authentication") || fault.contains("login") {
                return Err(SoapError::AuthFailed);
            }
            return Err(SoapError::Fault(fault));
        }
        return Err(SoapError::Unexpected(format!("HTTP {status}")));
    }

    // Check for SOAP fault in success response too
    if let Some(fault) = extract_soap_fault(&body) {
        return Err(SoapError::Fault(fault));
    }

    Ok(body)
}

/// Extract SOAP fault string from response XML, if present.
fn extract_soap_fault(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut in_fault_string = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"faultstring" => {
                in_fault_string = true;
            }
            Ok(Event::Text(e)) if in_fault_string => {
                return e.unescape().ok().map(|s| s.to_string());
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    None
}

// ── Public SOAP operations ──────────────────────────────────────────

/// Build and send a `getZipcodeInfo` SOAP request.
pub fn get_zipcode_info(
    client: &reqwest::blocking::Client,
    auth: &SoapAuth,
    zipcode: &str,
) -> Result<ZipInfo, SoapError> {
    let envelope = build_envelope("getZipcodeInfo", auth, |w| {
        write_typed_element(w, "zipcode", "xsd:int", zipcode)
    })?;

    let response = send_request(client, &envelope)?;
    parse_zip_info(&response)
}

/// Build and send a `getCountyFreqsByTag` SOAP request.
///
/// Pass `tag = 0` to attempt fetching all frequencies.
pub fn get_county_freqs_by_tag(
    client: &reqwest::blocking::Client,
    auth: &SoapAuth,
    county_id: u32,
    tag: u32,
) -> Result<Vec<RrFrequency>, SoapError> {
    let envelope = build_envelope("getCountyFreqsByTag", auth, |w| {
        write_typed_element(w, "ctid", "xsd:int", &county_id.to_string())?;
        write_typed_element(w, "tag", "xsd:int", &tag.to_string())
    })?;

    let response = send_request(client, &envelope)?;
    parse_frequencies(&response)
}

// ── XML response parsers ────────────────────────────────────────────

/// Parse `getZipcodeInfo` response XML into a `ZipInfo`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn parse_zip_info(xml: &str) -> Result<ZipInfo, SoapError> {
    let mut reader = Reader::from_str(xml);
    let mut current_tag = String::new();
    let mut zip = ZipInfo {
        county_id: 0,
        state_id: 0,
        city: String::new(),
        county_name: String::new(),
        state_name: String::new(),
    };

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current_tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "ctid" => zip.county_id = text.parse().unwrap_or(0),
                    "stid" => zip.state_id = text.parse().unwrap_or(0),
                    "city" => zip.city = text,
                    "countyName" => zip.county_name = text,
                    "stateName" => zip.state_name = text,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(SoapError::Xml(e)),
            _ => {}
        }
    }

    if zip.county_id == 0 {
        return Err(SoapError::Unexpected("no county ID in response".to_string()));
    }

    Ok(zip)
}

/// Parse `getCountyFreqsByTag` response XML into a list of `RrFrequency`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn parse_frequencies(xml: &str) -> Result<Vec<RrFrequency>, SoapError> {
    let mut reader = Reader::from_str(xml);
    let mut frequencies = Vec::new();
    let mut current_freq: Option<RrFrequency> = None;
    let mut current_tag_elem: Option<RrTag> = None;
    let mut current_xml_tag = String::new();
    let mut in_tag_item = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match name.as_str() {
                    "item" if current_freq.is_none() => {
                        current_freq = Some(RrFrequency {
                            id: String::new(),
                            freq_hz: 0,
                            mode: String::new(),
                            tone: None,
                            description: String::new(),
                            alpha_tag: String::new(),
                            tags: Vec::new(),
                        });
                    }
                    "item" if current_freq.is_some() => {
                        // Nested item = tag entry
                        in_tag_item = true;
                        current_tag_elem = Some(RrTag {
                            id: 0,
                            description: String::new(),
                        });
                    }
                    _ => {
                        current_xml_tag = name;
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                if in_tag_item {
                    if let Some(ref mut tag) = current_tag_elem {
                        match current_xml_tag.as_str() {
                            "tagId" => tag.id = text.parse().unwrap_or(0),
                            "tagDescr" => tag.description = text,
                            _ => {}
                        }
                    }
                } else if let Some(ref mut freq) = current_freq {
                    match current_xml_tag.as_str() {
                        "fid" => freq.id = text,
                        "out" => {
                            // RR returns frequency in MHz; convert to Hz
                            if let Ok(mhz) = text.parse::<f64>() {
                                freq.freq_hz = (mhz * 1_000_000.0).round() as u64;
                            }
                        }
                        "mode" => freq.mode = text,
                        "tone" => {
                            if let Ok(t) = text.parse::<f32>() {
                                if t > 0.0 {
                                    freq.tone = Some(t);
                                }
                            }
                        }
                        "descr" => freq.description = text,
                        "alpha" => freq.alpha_tag = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if name == "item" {
                    if in_tag_item {
                        // Close a tag item
                        if let (Some(ref mut freq), Some(tag)) =
                            (&mut current_freq, current_tag_elem.take())
                        {
                            freq.tags.push(tag);
                        }
                        in_tag_item = false;
                    } else if let Some(freq) = current_freq.take() {
                        // Close a frequency item
                        if freq.freq_hz > 0 {
                            frequencies.push(freq);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(SoapError::Xml(e)),
            _ => {}
        }
    }

    Ok(frequencies)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZIP_RESPONSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/">
<SOAP-ENV:Body>
<ns1:getZipcodeInfoResponse xmlns:ns1="http://api.radioreference.com/soap2">
<return>
<zipCode>90210</zipCode>
<city>Beverly Hills</city>
<stid>6</stid>
<ctid>277</ctid>
<lat>34.0901</lat>
<lon>-118.4065</lon>
<countyName>Los Angeles</countyName>
<stateName>California</stateName>
</return>
</ns1:getZipcodeInfoResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    const FREQ_RESPONSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/">
<SOAP-ENV:Body>
<ns1:getCountyFreqsByTagResponse xmlns:ns1="http://api.radioreference.com/soap2">
<return>
<item>
<fid>12345</fid>
<out>155.37000</out>
<mode>FMN</mode>
<tone>110.9</tone>
<descr>Police Dispatch</descr>
<alpha>PD Disp</alpha>
<tags>
<item><tagId>32</tagId><tagDescr>Law Dispatch</tagDescr></item>
</tags>
</item>
<item>
<fid>12346</fid>
<out>154.43000</out>
<mode>FM</mode>
<tone>0</tone>
<descr>Fire Dispatch</descr>
<alpha>FD Disp</alpha>
<tags>
<item><tagId>33</tagId><tagDescr>Fire Dispatch</tagDescr></item>
</tags>
</item>
</return>
</ns1:getCountyFreqsByTagResponse>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    const FAULT_RESPONSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/">
<SOAP-ENV:Body>
<SOAP-ENV:Fault>
<faultcode>SOAP-ENV:Server</faultcode>
<faultstring>authentication failed</faultstring>
</SOAP-ENV:Fault>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    #[test]
    fn parse_zip_info_response() {
        let zip = parse_zip_info(ZIP_RESPONSE).expect("should parse");
        assert_eq!(zip.county_id, 277);
        assert_eq!(zip.state_id, 6);
        assert_eq!(zip.city, "Beverly Hills");
        assert_eq!(zip.county_name, "Los Angeles");
        assert_eq!(zip.state_name, "California");
    }

    #[test]
    fn parse_frequencies_response() {
        let freqs = parse_frequencies(FREQ_RESPONSE).expect("should parse");
        assert_eq!(freqs.len(), 2);

        assert_eq!(freqs[0].id, "12345");
        assert_eq!(freqs[0].freq_hz, 155_370_000);
        assert_eq!(freqs[0].mode, "FMN");
        assert!((freqs[0].tone.expect("should have tone") - 110.9).abs() < f32::EPSILON);
        assert_eq!(freqs[0].description, "Police Dispatch");
        assert_eq!(freqs[0].alpha_tag, "PD Disp");
        assert_eq!(freqs[0].tags.len(), 1);
        assert_eq!(freqs[0].tags[0].id, 32);
        assert_eq!(freqs[0].tags[0].description, "Law Dispatch");

        assert_eq!(freqs[1].id, "12346");
        assert_eq!(freqs[1].freq_hz, 154_430_000);
        assert!(freqs[1].tone.is_none()); // tone=0 means no tone
    }

    #[test]
    fn extract_fault_from_response() {
        let fault = extract_soap_fault(FAULT_RESPONSE);
        assert_eq!(fault, Some("authentication failed".to_string()));
    }

    #[test]
    fn no_fault_in_success_response() {
        let fault = extract_soap_fault(ZIP_RESPONSE);
        assert!(fault.is_none());
    }

    #[test]
    fn envelope_contains_auth_info() {
        let auth = SoapAuth {
            username: "testuser".to_string(),
            password: "testpass".to_string(),
            app_key: "testkey".to_string(),
        };
        let envelope = build_envelope("getZipcodeInfo", &auth, |w| {
            write_typed_element(w, "zipcode", "xsd:int", "90210")
        })
        .expect("should build");
        let xml = String::from_utf8(envelope).expect("valid utf8");

        assert!(xml.contains("getZipcodeInfo"));
        assert!(xml.contains("<username"));
        assert!(xml.contains("testuser"));
        assert!(xml.contains("<password"));
        assert!(xml.contains("testpass"));
        assert!(xml.contains("<appKey"));
        assert!(xml.contains("testkey"));
        assert!(xml.contains("<zipcode"));
        assert!(xml.contains("90210"));
    }
}
```

- [ ] **Step 3: Update lib.rs**

```rust
pub mod mode_map;
pub mod soap;
pub mod types;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p sdr-radioreference`
Expected: All tests PASS (mode_map + soap)

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p sdr-radioreference -- -D warnings`
Expected: Clean

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-radioreference/
git commit -m "add SOAP types, envelope builder, XML parsers with tests"
```

---

## Task 3: sdr-radioreference — RrClient Public API

**Files:**
- Modify: `crates/sdr-radioreference/src/lib.rs`

- [ ] **Step 1: Implement RrClient**

Replace `crates/sdr-radioreference/src/lib.rs`:

```rust
//! RadioReference.com SOAP API client.
//!
//! Provides `RrClient` for querying the RadioReference frequency database.
//! All methods are blocking — call from a background thread when used with GTK.

pub mod mode_map;
pub mod soap;
pub mod types;

pub use soap::SoapError;
pub use types::{RrFrequency, RrTag, ZipInfo};

/// Application API key — identifies the SDR-RS app to RadioReference.
/// This is not a secret; it's distributed with the application.
const APP_KEY: &str = "PENDING_API_KEY";

/// RadioReference SOAP API client.
///
/// Holds user credentials and an HTTP client. All methods are blocking.
pub struct RrClient {
    auth: soap::SoapAuth,
    http: reqwest::blocking::Client,
}

impl RrClient {
    /// Create a new client with the given RadioReference credentials.
    pub fn new(username: &str, password: &str) -> Self {
        Self {
            auth: soap::SoapAuth {
                username: username.to_string(),
                password: password.to_string(),
                app_key: APP_KEY.to_string(),
            },
            http: reqwest::blocking::Client::new(),
        }
    }

    /// Test the connection by querying zip code 90210.
    ///
    /// Returns `Ok(())` if credentials are valid, `Err` otherwise.
    pub fn test_connection(&self) -> Result<(), SoapError> {
        soap::get_zipcode_info(&self.http, &self.auth, "90210")?;
        Ok(())
    }

    /// Look up county/state info for a US zip code.
    pub fn get_zip_info(&self, zipcode: &str) -> Result<ZipInfo, SoapError> {
        tracing::debug!(zipcode, "querying RadioReference zip info");
        soap::get_zipcode_info(&self.http, &self.auth, zipcode)
    }

    /// Get all frequencies for a county.
    ///
    /// Calls `getCountyFreqsByTag` with `tag=0` to request all categories.
    pub fn get_county_frequencies(&self, county_id: u32) -> Result<Vec<RrFrequency>, SoapError> {
        tracing::debug!(county_id, "querying RadioReference county frequencies");
        soap::get_county_freqs_by_tag(&self.http, &self.auth, county_id, 0)
    }
}
```

- [ ] **Step 2: Build and clippy**

Run: `cargo build -p sdr-radioreference && cargo clippy -p sdr-radioreference -- -D warnings`
Expected: Clean

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-radioreference/src/lib.rs
git commit -m "add RrClient public API"
```

---

## Task 4: sdr-config — KeyringStore

**Files:**
- Modify: `Cargo.toml` (workspace deps)
- Modify: `crates/sdr-config/Cargo.toml`
- Create: `crates/sdr-config/src/keyring_store.rs`
- Modify: `crates/sdr-config/src/lib.rs`

- [ ] **Step 1: Add keyring dependency**

Add to `Cargo.toml` workspace dependencies:

```toml
keyring = { version = "3", features = ["sync-secret-service"] }
```

Add to `crates/sdr-config/Cargo.toml` dependencies:

```toml
keyring.workspace = true
```

- [ ] **Step 2: Create KeyringStore**

Create `crates/sdr-config/src/keyring_store.rs`:

```rust
//! Secure credential storage via the OS keyring.
//!
//! Uses `keyring` crate which delegates to:
//! - **Linux**: Secret Service D-Bus API (GNOME Keyring, KeePassXC)
//! - **macOS**: Keychain

/// Error type for keyring operations.
#[derive(Debug, thiserror::Error)]
pub enum KeyringError {
    /// No keyring backend available on this system.
    #[error("no secure storage available — install GNOME Keyring or KeePassXC")]
    NoBackend,
    /// The requested credential was not found.
    #[error("credential not found")]
    NotFound,
    /// Platform-specific keyring error.
    #[error("keyring error: {0}")]
    Platform(String),
}

/// Thin wrapper around the OS keyring for storing secrets.
///
/// Each instance is scoped to a service name (e.g., `"sdr-rs"`).
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    /// Create a new store scoped to the given service name.
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    /// Store a secret value for the given key.
    pub fn set(&self, key: &str, value: &str) -> Result<(), KeyringError> {
        let entry = self.entry(key)?;
        entry
            .set_password(value)
            .map_err(|e| KeyringError::Platform(e.to_string()))
    }

    /// Retrieve a secret value for the given key.
    ///
    /// Returns `Ok(None)` if the key does not exist.
    pub fn get(&self, key: &str) -> Result<Option<String>, KeyringError> {
        let entry = self.entry(key)?;
        match entry.get_password() {
            Ok(val) => Ok(Some(val)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(KeyringError::Platform(e.to_string())),
        }
    }

    /// Delete a stored secret.
    ///
    /// Returns `Ok(())` even if the key did not exist.
    pub fn delete(&self, key: &str) -> Result<(), KeyringError> {
        let entry = self.entry(key)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(KeyringError::Platform(e.to_string())),
        }
    }

    /// Check whether a credential exists for the given key.
    pub fn has(&self, key: &str) -> bool {
        self.get(key).ok().flatten().is_some()
    }

    /// Create a keyring entry for the given key.
    fn entry(&self, key: &str) -> Result<keyring::Entry, KeyringError> {
        keyring::Entry::new(&self.service, key).map_err(|e| {
            if e.to_string().contains("no default") || e.to_string().contains("platform") {
                KeyringError::NoBackend
            } else {
                KeyringError::Platform(e.to_string())
            }
        })
    }
}
```

- [ ] **Step 3: Add module to sdr-config lib.rs**

Add to `crates/sdr-config/src/lib.rs` (at the top, with other module declarations):

```rust
pub mod keyring_store;
pub use keyring_store::KeyringStore;
```

- [ ] **Step 4: Build and clippy**

Run: `cargo build -p sdr-config && cargo clippy -p sdr-config -- -D warnings`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/sdr-config/
git commit -m "add KeyringStore for secure credential storage via OS keyring"
```

---

## Task 5: Bookmark Struct — Add RR Metadata Fields

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/navigation_panel.rs:114-143`

- [ ] **Step 1: Add fields to Bookmark struct**

In `crates/sdr-ui/src/sidebar/navigation_panel.rs`, add two new fields to the `Bookmark` struct after the `high_pass` field (around line 141):

```rust
    #[serde(default)]
    pub high_pass: Option<bool>,
    /// RadioReference category (e.g., "Law Dispatch"). Metadata for future
    /// bookmark tree organization.
    #[serde(default)]
    pub rr_category: Option<String>,
    /// RadioReference frequency ID for duplicate detection and future sync.
    #[serde(default)]
    pub rr_import_id: Option<String>,
```

- [ ] **Step 2: Update Bookmark::new to include new fields**

In the `Bookmark::new()` method (around line 147), add the new fields with `None` values:

```rust
            high_pass: None,
            rr_category: None,
            rr_import_id: None,
```

- [ ] **Step 3: Update Bookmark::with_profile to include new fields**

In the `Bookmark::with_profile()` method (around line 168), add the new fields with `None` values:

```rust
            high_pass: profile.high_pass,
            rr_category: None,
            rr_import_id: None,
```

- [ ] **Step 4: Build and test**

Run: `cargo build --workspace && cargo test --workspace`
Expected: Clean build, all tests pass (serde(default) ensures backward compat)

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/sidebar/navigation_panel.rs
git commit -m "add rr_category and rr_import_id fields to Bookmark"
```

---

## Task 6: Preferences Window — Scaffold + General Page

**Files:**
- Create: `crates/sdr-ui/src/preferences/mod.rs`
- Create: `crates/sdr-ui/src/preferences/general_page.rs`
- Modify: `crates/sdr-ui/src/lib.rs` (add module)
- Modify: `crates/sdr-ui/src/window.rs` (add Preferences menu item + action)

- [ ] **Step 1: Create general_page.rs**

Create `crates/sdr-ui/src/preferences/general_page.rs`:

```rust
//! General preferences page — default directories for recordings and screenshots.

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_config::ConfigManager;
use std::sync::Arc;

/// Default recording directory.
const DEFAULT_RECORDING_DIR: &str = "sdr-recordings";
/// Default screenshot directory.
const DEFAULT_SCREENSHOT_DIR: &str = "Pictures";

/// Build the General preferences page.
pub fn build_general_page(config: &Arc<ConfigManager>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("preferences-system-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Directories")
        .description("Default save locations for recordings and screenshots")
        .build();

    // Recording directory
    let recording_dir = get_config_dir(config, "recording_dir", DEFAULT_RECORDING_DIR);
    let recording_row = adw::ActionRow::builder()
        .title("Recording Directory")
        .subtitle(&recording_dir)
        .build();

    let recording_btn = gtk4::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Choose directory")
        .build();
    recording_row.add_suffix(&recording_btn);

    let config_clone = Arc::clone(config);
    let row_clone = recording_row.clone();
    recording_btn.connect_clicked(move |btn| {
        let dialog = gtk4::FileDialog::builder()
            .title("Select Recording Directory")
            .build();
        let config_inner = Arc::clone(&config_clone);
        let row_inner = row_clone.clone();
        let window = btn.root().and_downcast::<gtk4::Window>();
        dialog.select_folder(window.as_ref(), gtk4::gio::Cancellable::NONE, move |result| {
            if let Ok(folder) = result {
                if let Some(path) = folder.path() {
                    let path_str = path.to_string_lossy().to_string();
                    row_inner.set_subtitle(&path_str);
                    config_inner.write(|v| {
                        v["recording_dir"] = serde_json::Value::String(path_str);
                    });
                }
            }
        });
    });

    // Screenshot directory
    let screenshot_dir = get_config_dir(config, "screenshot_dir", DEFAULT_SCREENSHOT_DIR);
    let screenshot_row = adw::ActionRow::builder()
        .title("Screenshot Directory")
        .subtitle(&screenshot_dir)
        .build();

    let screenshot_btn = gtk4::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text("Choose directory")
        .build();
    screenshot_row.add_suffix(&screenshot_btn);

    let config_clone = Arc::clone(config);
    let row_clone = screenshot_row.clone();
    screenshot_btn.connect_clicked(move |btn| {
        let dialog = gtk4::FileDialog::builder()
            .title("Select Screenshot Directory")
            .build();
        let config_inner = Arc::clone(&config_clone);
        let row_inner = row_clone.clone();
        let window = btn.root().and_downcast::<gtk4::Window>();
        dialog.select_folder(window.as_ref(), gtk4::gio::Cancellable::NONE, move |result| {
            if let Ok(folder) = result {
                if let Some(path) = folder.path() {
                    let path_str = path.to_string_lossy().to_string();
                    row_inner.set_subtitle(&path_str);
                    config_inner.write(|v| {
                        v["screenshot_dir"] = serde_json::Value::String(path_str);
                    });
                }
            }
        });
    });

    group.add(&recording_row);
    group.add(&screenshot_row);
    page.add(&group);
    page
}

/// Read a directory config value, falling back to `$HOME/default`.
fn get_config_dir(config: &Arc<ConfigManager>, key: &str, default_subdir: &str) -> String {
    config.read(|v| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| {
                let home = gtk4::glib::home_dir();
                home.join(default_subdir).to_string_lossy().to_string()
            })
    })
}
```

- [ ] **Step 2: Create preferences/mod.rs**

Create `crates/sdr-ui/src/preferences/mod.rs`:

```rust
//! Application preferences window.

pub mod general_page;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_config::ConfigManager;
use std::sync::Arc;

/// Build and return the preferences window.
pub fn build_preferences_window(
    parent: &impl IsA<gtk4::Window>,
    config: &Arc<ConfigManager>,
) -> adw::PreferencesWindow {
    let window = adw::PreferencesWindow::builder()
        .title("Preferences")
        .transient_for(parent)
        .modal(true)
        .default_width(600)
        .default_height(500)
        .build();

    // General page
    let general = general_page::build_general_page(config);
    window.add(&general);

    window
}
```

- [ ] **Step 3: Add module to lib.rs**

In `crates/sdr-ui/src/lib.rs`, add after the `notify` module:

```rust
pub mod preferences;
```

- [ ] **Step 4: Add Preferences menu item and action to window.rs**

In `crates/sdr-ui/src/window.rs`, in `build_menu_button()` (around line 495), add a "Preferences" menu item before the "Keyboard Shortcuts" item:

```rust
menu.append(Some("_Preferences"), Some("app.preferences"));
```

In `setup_app_actions()` (around line 1278), add the preferences action. This function receives the `app` and `window`. Add a new action that opens the preferences window:

```rust
let preferences_action = gio::SimpleAction::new("preferences", None);
let window_clone = window.clone();
preferences_action.connect_activate(move |_, _| {
    // ConfigManager needs to be available here — read from the
    // function parameter or state. We'll pass it through.
    let prefs = crate::preferences::build_preferences_window(
        &window_clone,
        // config ref — see Step 5 for how this is threaded through
    );
    prefs.present();
});
app.add_action(&preferences_action);
```

Note: The `ConfigManager` needs to be threaded into `setup_app_actions`. This requires adding a `config: &Arc<ConfigManager>` parameter to `setup_app_actions` and to `build_window`. The config is already created in `dsp_controller.rs` — read the current `build_window` call site to understand how to pass it through.

- [ ] **Step 5: Thread ConfigManager through to preferences**

The `ConfigManager` is created in `dsp_controller.rs`. It needs to reach the preferences window. Add `config: Arc<ConfigManager>` as a parameter to `build_window()` and `setup_app_actions()`, then pass it from the call site.

In `app.rs`, the `connect_activate` closure calls `window::build_window(app)`. Update this to also create/load the config and pass it:

```rust
app.connect_activate(|app| {
    let config_path = gtk4::glib::user_config_dir().join("sdr-rs").join("config.json");
    let defaults = serde_json::json!({});
    let config = std::sync::Arc::new(
        sdr_config::ConfigManager::load(&config_path, &defaults)
            .unwrap_or_else(|e| {
                tracing::warn!("config load failed, using defaults: {e}");
                sdr_config::ConfigManager::load(&config_path, &defaults)
                    .expect("default config creation should not fail")
            })
    );
    window::build_window(app, &config);
});
```

Update `build_window` signature to accept `config: &Arc<ConfigManager>` and thread it to `setup_app_actions`.

- [ ] **Step 6: Build and test**

Run: `cargo build --workspace && cargo clippy --all-targets --workspace -- -D warnings`
Expected: Clean

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-ui/src/preferences/ crates/sdr-ui/src/lib.rs crates/sdr-ui/src/window.rs crates/sdr-ui/src/app.rs
git commit -m "add AdwPreferencesWindow with General page for directory settings"
```

---

## Task 7: Accounts Page — Credential Entry + Test & Save

**Files:**
- Create: `crates/sdr-ui/src/preferences/accounts_page.rs`
- Modify: `crates/sdr-ui/src/preferences/mod.rs`
- Modify: `crates/sdr-ui/Cargo.toml` (add sdr-radioreference dep)

- [ ] **Step 1: Add sdr-radioreference to sdr-ui dependencies**

In `crates/sdr-ui/Cargo.toml`, add:

```toml
sdr-radioreference.workspace = true
```

- [ ] **Step 2: Create accounts_page.rs**

Create `crates/sdr-ui/src/preferences/accounts_page.rs`:

```rust
//! Accounts preferences page — RadioReference credential management.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_config::KeyringStore;

/// Keyring service name.
const KEYRING_SERVICE: &str = "sdr-rs";
/// Keyring key for RR username.
const RR_USERNAME_KEY: &str = "radioreference-username";
/// Keyring key for RR password.
const RR_PASSWORD_KEY: &str = "radioreference-password";

/// Build the Accounts preferences page.
///
/// Returns `(page, has_credentials_flag)`. The flag is `true` if valid
/// credentials are currently stored and can be used by other UI components
/// to show/hide the RadioReference browse button.
pub fn build_accounts_page() -> (adw::PreferencesPage, Rc<Cell<bool>>) {
    let page = adw::PreferencesPage::builder()
        .title("Accounts")
        .icon_name("system-users-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("RadioReference")
        .description("Premium account credentials for frequency database access")
        .build();

    let username_row = adw::EntryRow::builder()
        .title("Username")
        .build();

    let password_row = adw::PasswordEntryRow::builder()
        .title("Password")
        .build();

    // Status label (hidden initially)
    let status_label = gtk4::Label::builder()
        .css_classes(["dim-label"])
        .visible(false)
        .build();

    // Test & Save button
    let test_button = gtk4::Button::builder()
        .label("Test & Save")
        .css_classes(["suggested-action"])
        .build();

    // Spinner (hidden initially)
    let spinner = gtk4::Spinner::builder()
        .visible(false)
        .build();

    // Remove credentials button (visible only when creds exist)
    let remove_button = gtk4::Button::builder()
        .label("Remove Credentials")
        .css_classes(["destructive-action"])
        .build();

    // Track whether credentials are stored
    let has_credentials = Rc::new(Cell::new(false));

    // Pre-fill if credentials exist
    let store = KeyringStore::new(KEYRING_SERVICE);
    if let Ok(Some(user)) = store.get(RR_USERNAME_KEY) {
        username_row.set_text(&user);
        has_credentials.set(true);
        remove_button.set_visible(true);
        status_label.set_text("Credentials stored");
        status_label.set_css_classes(&["success"]);
        status_label.set_visible(true);
    } else {
        remove_button.set_visible(false);
    }

    // Test & Save handler
    let username_clone = username_row.clone();
    let password_clone = password_row.clone();
    let status_clone = status_label.clone();
    let spinner_clone = spinner.clone();
    let test_btn_clone = test_button.clone();
    let remove_btn_clone = remove_button.clone();
    let has_creds_clone = Rc::clone(&has_credentials);

    test_button.connect_clicked(move |_| {
        let username = username_clone.text().to_string();
        let password = password_clone.text().to_string();

        if username.is_empty() || password.is_empty() {
            status_clone.set_text("Enter username and password");
            status_clone.set_css_classes(&["error"]);
            status_clone.set_visible(true);
            return;
        }

        // Show spinner, disable button
        spinner_clone.set_visible(true);
        spinner_clone.start();
        test_btn_clone.set_sensitive(false);
        status_clone.set_visible(false);

        let status = status_clone.clone();
        let spinner = spinner_clone.clone();
        let test_btn = test_btn_clone.clone();
        let remove_btn = remove_btn_clone.clone();
        let has_creds = Rc::clone(&has_creds_clone);

        // Run SOAP test on background thread
        let (sender, receiver) = glib::MainContext::channel::<Result<(), String>>(glib::Priority::DEFAULT);

        gtk4::gio::spawn_blocking(move || {
            let client = sdr_radioreference::RrClient::new(&username, &password);
            let result = client
                .test_connection()
                .map_err(|e| e.to_string());

            if result.is_ok() {
                // Save credentials on success
                let store = KeyringStore::new(KEYRING_SERVICE);
                if let Err(e) = store.set(RR_USERNAME_KEY, &username) {
                    let _ = sender.send(Err(format!("keyring: {e}")));
                    return;
                }
                if let Err(e) = store.set(RR_PASSWORD_KEY, &password) {
                    let _ = sender.send(Err(format!("keyring: {e}")));
                    return;
                }
            }

            let _ = sender.send(result);
        });

        receiver.attach(None, move |result| {
            spinner.stop();
            spinner.set_visible(false);
            test_btn.set_sensitive(true);

            match result {
                Ok(()) => {
                    status.set_text("Connected — credentials saved");
                    status.set_css_classes(&["success"]);
                    has_creds.set(true);
                    remove_btn.set_visible(true);
                }
                Err(e) => {
                    status.set_text(&format!("Failed: {e}"));
                    status.set_css_classes(&["error"]);
                }
            }
            status.set_visible(true);
            glib::ControlFlow::Continue
        });
    });

    // Remove credentials handler
    let status_clone = status_label.clone();
    let username_clone = username_row.clone();
    let password_clone = password_row.clone();
    let has_creds_clone = Rc::clone(&has_credentials);
    let remove_btn_clone = remove_button.clone();

    remove_button.connect_clicked(move |_| {
        let store = KeyringStore::new(KEYRING_SERVICE);
        let _ = store.delete(RR_USERNAME_KEY);
        let _ = store.delete(RR_PASSWORD_KEY);

        username_clone.set_text("");
        password_clone.set_text("");
        has_creds_clone.set(false);
        remove_btn_clone.set_visible(false);
        status_clone.set_text("Credentials removed");
        status_clone.set_css_classes(&["dim-label"]);
        status_clone.set_visible(true);
    });

    // Layout: rows in a vertical box within the group
    let button_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(12)
        .margin_top(8)
        .build();
    button_box.append(&test_button);
    button_box.append(&spinner);
    button_box.append(&remove_button);

    group.add(&username_row);
    group.add(&password_row);

    let status_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .build();
    status_box.append(&button_box);
    status_box.append(&status_label);
    group.add(&status_box);

    page.add(&group);

    (page, has_credentials)
}

/// Check if RadioReference credentials are stored in the keyring.
pub fn has_rr_credentials() -> bool {
    let store = KeyringStore::new(KEYRING_SERVICE);
    store.has(RR_USERNAME_KEY) && store.has(RR_PASSWORD_KEY)
}

/// Load RadioReference credentials from the keyring.
///
/// Returns `(username, password)` or `None` if not stored.
pub fn load_rr_credentials() -> Option<(String, String)> {
    let store = KeyringStore::new(KEYRING_SERVICE);
    let username = store.get(RR_USERNAME_KEY).ok()??;
    let password = store.get(RR_PASSWORD_KEY).ok()??;
    Some((username, password))
}
```

- [ ] **Step 3: Add accounts page to preferences mod.rs**

In `crates/sdr-ui/src/preferences/mod.rs`, add:

```rust
pub mod accounts_page;
```

And in `build_preferences_window`, add the accounts page after the general page:

```rust
    // Accounts page
    let (accounts, _has_credentials) = accounts_page::build_accounts_page();
    window.add(&accounts);
```

- [ ] **Step 4: Build and clippy**

Run: `cargo build --workspace && cargo clippy --all-targets --workspace -- -D warnings`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/Cargo.toml crates/sdr-ui/src/preferences/
git commit -m "add Accounts preferences page with RR credential test & save"
```

---

## Task 8: RadioReference Browse Dialog — Search + County Picker

**Files:**
- Create: `crates/sdr-ui/src/radioreference/mod.rs`
- Create: `crates/sdr-ui/src/radioreference/frequency_list.rs`
- Modify: `crates/sdr-ui/src/lib.rs`
- Modify: `crates/sdr-ui/src/window.rs` (header button + wiring)

- [ ] **Step 1: Create frequency_list.rs**

Create `crates/sdr-ui/src/radioreference/frequency_list.rs`:

```rust
//! Frequency list widget with checkboxes, filtering, and duplicate detection.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;

use sdr_radioreference::RrFrequency;
use crate::sidebar::navigation_panel::{format_frequency, Bookmark};

/// A frequency row in the list — wraps an `RrFrequency` with selection state.
#[derive(Debug, Clone)]
pub struct FrequencyRow {
    pub frequency: RrFrequency,
    pub selected: bool,
    pub already_bookmarked: bool,
}

/// Build the frequency list box and associated filter dropdowns.
///
/// Returns `(container, frequency_rows, import_button)` where:
/// - `container` is the full widget with filters + list + import button
/// - `frequency_rows` is the shared mutable list of rows for reading selection state
/// - `import_button` is the import button for connecting the import action externally
#[allow(clippy::too_many_lines)]
pub fn build_frequency_list(
    existing_bookmarks: &[Bookmark],
) -> (gtk4::Box, Rc<RefCell<Vec<FrequencyRow>>>, gtk4::Button) {
    let container = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(8)
        .build();

    // Category filter
    let category_model = gtk4::StringList::new(&["All"]);
    let category_dropdown = gtk4::DropDown::builder()
        .model(&category_model)
        .build();
    let category_row = adw::ActionRow::builder()
        .title("Category")
        .build();
    category_row.add_suffix(&category_dropdown);

    // Agency filter
    let agency_model = gtk4::StringList::new(&["All"]);
    let agency_dropdown = gtk4::DropDown::builder()
        .model(&agency_model)
        .build();
    let agency_row = adw::ActionRow::builder()
        .title("Agency")
        .build();
    agency_row.add_suffix(&agency_dropdown);

    // Frequency list
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(300)
        .build();
    let list_box = gtk4::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    scroll.set_child(Some(&list_box));

    // Import button
    let import_button = gtk4::Button::builder()
        .label("Import Selected (0)")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();

    // Shared state
    let rows: Rc<RefCell<Vec<FrequencyRow>>> = Rc::new(RefCell::new(Vec::new()));

    // Build set of existing bookmark IDs and frequencies for dedup
    let existing_rr_ids: HashSet<String> = existing_bookmarks
        .iter()
        .filter_map(|b| b.rr_import_id.clone())
        .collect();
    let existing_freqs: HashSet<u64> = existing_bookmarks
        .iter()
        .map(|b| b.frequency)
        .collect();

    // Store dedup sets for use in populate
    let existing_rr_ids = Rc::new(existing_rr_ids);
    let existing_freqs = Rc::new(existing_freqs);

    container.append(&category_row);
    container.append(&agency_row);
    container.append(&scroll);
    container.append(&import_button);

    // Expose populate and filter functions via closures stored on widgets
    // The parent dialog will call populate_frequencies() after a successful search

    (container, rows, import_button)
}

/// Populate the frequency list with results from RadioReference.
///
/// Builds check rows, applies duplicate detection, and sets up filter dropdowns.
pub fn populate_frequencies(
    list_box: &gtk4::ListBox,
    rows: &Rc<RefCell<Vec<FrequencyRow>>>,
    frequencies: &[RrFrequency],
    existing_bookmarks: &[Bookmark],
    category_model: &gtk4::StringList,
    agency_model: &gtk4::StringList,
    import_button: &gtk4::Button,
) {
    // Clear existing
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    rows.borrow_mut().clear();

    // Build dedup sets
    let existing_rr_ids: HashSet<String> = existing_bookmarks
        .iter()
        .filter_map(|b| b.rr_import_id.clone())
        .collect();
    let existing_freqs: HashSet<u64> = existing_bookmarks
        .iter()
        .map(|b| b.frequency)
        .collect();

    // Collect unique categories and agencies for filter dropdowns
    let mut categories: Vec<String> = Vec::new();
    let mut agencies: Vec<String> = Vec::new();

    // Build rows
    let mut freq_rows = Vec::new();
    for freq in frequencies {
        let already_bookmarked = existing_rr_ids.contains(&freq.id)
            || existing_freqs.contains(&freq.freq_hz);

        // Derive category from first tag
        let category = freq
            .tags
            .first()
            .map(|t| t.description.clone())
            .unwrap_or_else(|| "Uncategorized".to_string());

        if !categories.contains(&category) {
            categories.push(category.clone());
        }

        let agency = freq.alpha_tag.clone();
        if !agency.is_empty() && !agencies.contains(&agency) {
            agencies.push(agency.clone());
        }

        freq_rows.push(FrequencyRow {
            frequency: freq.clone(),
            selected: false,
            already_bookmarked,
        });
    }

    // Update filter models
    category_model.splice(0, category_model.n_items(), &{
        let mut items = vec!["All".to_string()];
        categories.sort();
        items.extend(categories);
        items.iter().map(String::as_str).collect::<Vec<_>>()
    });

    agency_model.splice(0, agency_model.n_items(), &{
        let mut items = vec!["All".to_string()];
        agencies.sort();
        items.extend(agencies);
        items.iter().map(String::as_str).collect::<Vec<_>>()
    });

    // Build list rows
    let import_btn = import_button.clone();
    let rows_ref = Rc::clone(rows);

    for (i, row) in freq_rows.iter().enumerate() {
        let freq = &row.frequency;
        let freq_str = format_frequency(freq.freq_hz);
        let mapped = sdr_radioreference::mode_map::map_rr_mode(&freq.mode);

        let list_row = adw::ActionRow::builder()
            .title(&format!("{freq_str}  {}", mapped.demod_mode))
            .subtitle(&freq.description)
            .build();

        if row.already_bookmarked {
            // Show saved indicator — not selectable
            let icon = gtk4::Image::builder()
                .icon_name("emblem-ok-symbolic")
                .tooltip_text("Already bookmarked")
                .css_classes(["dim-label"])
                .build();
            list_row.add_prefix(&icon);
            list_row.set_sensitive(false);
        } else {
            // Checkbox for selection
            let check = gtk4::CheckButton::builder()
                .valign(gtk4::Align::Center)
                .build();
            list_row.add_prefix(&check);

            let rows_inner = Rc::clone(&rows_ref);
            let import_inner = import_btn.clone();
            check.connect_toggled(move |cb| {
                if let Some(r) = rows_inner.borrow_mut().get_mut(i) {
                    r.selected = cb.is_active();
                }
                update_import_count(&rows_inner, &import_inner);
            });
        }

        list_box.append(&list_row);
    }

    *rows.borrow_mut() = freq_rows;
    update_import_count(rows, import_button);
}

/// Update the import button label with the count of selected frequencies.
fn update_import_count(rows: &Rc<RefCell<Vec<FrequencyRow>>>, button: &gtk4::Button) {
    let count = rows
        .borrow()
        .iter()
        .filter(|r| r.selected && !r.already_bookmarked)
        .count();
    button.set_label(&format!("Import Selected ({count})"));
    button.set_sensitive(count > 0);
}
```

- [ ] **Step 2: Create radioreference/mod.rs — browse dialog**

Create `crates/sdr-ui/src/radioreference/mod.rs`:

```rust
//! RadioReference browse dialog — search by zip code, filter, and import frequencies.

pub mod frequency_list;

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_radioreference::{RrFrequency, ZipInfo};

use crate::sidebar::navigation_panel::{
    Bookmark, load_bookmarks, save_bookmarks,
};
use frequency_list::{FrequencyRow, populate_frequencies};

/// Open the RadioReference browse dialog.
#[allow(clippy::too_many_lines)]
pub fn show_browse_dialog(parent: &impl IsA<gtk4::Widget>) {
    let dialog = adw::Dialog::builder()
        .title("RadioReference")
        .content_width(700)
        .content_height(600)
        .build();

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();

    // ── Search section ──────────────────────────────────────────────

    let search_group = adw::PreferencesGroup::builder()
        .title("Search")
        .build();

    let zip_row = adw::EntryRow::builder()
        .title("Zip Code")
        .build();

    let search_button = gtk4::Button::builder()
        .label("Search")
        .css_classes(["suggested-action"])
        .valign(gtk4::Align::Center)
        .build();
    zip_row.add_suffix(&search_button);

    search_group.add(&zip_row);

    // County dropdown (hidden until search returns multiple)
    let county_model = gtk4::StringList::new(&[]);
    let county_dropdown = gtk4::DropDown::builder()
        .model(&county_model)
        .visible(false)
        .build();
    let county_row = adw::ActionRow::builder()
        .title("County")
        .visible(false)
        .build();
    county_row.add_suffix(&county_dropdown);
    search_group.add(&county_row);

    // Status / spinner
    let search_spinner = gtk4::Spinner::builder()
        .visible(false)
        .build();
    let search_status = gtk4::Label::builder()
        .css_classes(["dim-label"])
        .visible(false)
        .build();

    content.append(&search_group);
    content.append(&search_spinner);
    content.append(&search_status);

    // ── Results section ─────────────────────────────────────────────

    let results_group = adw::PreferencesGroup::builder()
        .title("Frequencies")
        .visible(false)
        .build();

    // Category filter
    let category_model = gtk4::StringList::new(&["All"]);
    let category_dropdown = gtk4::DropDown::builder()
        .model(&category_model)
        .build();
    let category_row = adw::ActionRow::builder()
        .title("Category")
        .build();
    category_row.add_suffix(&category_dropdown);
    results_group.add(&category_row);

    // Agency filter
    let agency_model = gtk4::StringList::new(&["All"]);
    let agency_dropdown = gtk4::DropDown::builder()
        .model(&agency_model)
        .build();
    let agency_row = adw::ActionRow::builder()
        .title("Agency")
        .build();
    agency_row.add_suffix(&agency_dropdown);
    results_group.add(&agency_row);

    // Frequency list
    let scroll = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(300)
        .build();
    let list_box = gtk4::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    scroll.set_child(Some(&list_box));
    results_group.add(&scroll);

    // Import button
    let import_button = gtk4::Button::builder()
        .label("Import Selected (0)")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .margin_top(8)
        .build();

    content.append(&results_group);
    content.append(&import_button);

    // ── Shared state ────────────────────────────────────────────────

    let rows: Rc<RefCell<Vec<FrequencyRow>>> = Rc::new(RefCell::new(Vec::new()));
    let fetched_frequencies: Rc<RefCell<Vec<RrFrequency>>> = Rc::new(RefCell::new(Vec::new()));
    let zip_infos: Rc<RefCell<Vec<ZipInfo>>> = Rc::new(RefCell::new(Vec::new()));

    // ── Search handler ──────────────────────────────────────────────

    let zip_clone = zip_row.clone();
    let spinner_clone = search_spinner.clone();
    let status_clone = search_status.clone();
    let county_row_clone = county_row.clone();
    let county_dropdown_clone = county_dropdown.clone();
    let county_model_clone = county_model.clone();
    let results_group_clone = results_group.clone();
    let list_box_clone = list_box.clone();
    let rows_clone = Rc::clone(&rows);
    let fetched_clone = Rc::clone(&fetched_frequencies);
    let zip_infos_clone = Rc::clone(&zip_infos);
    let category_model_clone = category_model.clone();
    let agency_model_clone = agency_model.clone();
    let import_btn_clone = import_button.clone();

    search_button.connect_clicked(move |_| {
        let zipcode = zip_clone.text().to_string().trim().to_string();
        if zipcode.is_empty() || zipcode.len() != 5 {
            status_clone.set_text("Enter a 5-digit zip code");
            status_clone.set_css_classes(&["error"]);
            status_clone.set_visible(true);
            return;
        }

        // Show spinner
        spinner_clone.set_visible(true);
        spinner_clone.start();
        status_clone.set_visible(false);

        let (sender, receiver) =
            glib::MainContext::channel::<Result<(ZipInfo, Vec<RrFrequency>), String>>(
                glib::Priority::DEFAULT,
            );

        // Load credentials and query on background thread
        let zip = zipcode.clone();
        gtk4::gio::spawn_blocking(move || {
            let Some((username, password)) =
                crate::preferences::accounts_page::load_rr_credentials()
            else {
                let _ = sender.send(Err("no credentials stored".to_string()));
                return;
            };

            let client = sdr_radioreference::RrClient::new(&username, &password);

            // Step 1: get zip info
            let zip_info = match client.get_zip_info(&zip) {
                Ok(info) => info,
                Err(e) => {
                    let _ = sender.send(Err(format!("zip lookup: {e}")));
                    return;
                }
            };

            // Step 2: get county frequencies
            let freqs = match client.get_county_frequencies(zip_info.county_id) {
                Ok(f) => f,
                Err(e) => {
                    let _ = sender.send(Err(format!("frequency query: {e}")));
                    return;
                }
            };

            let _ = sender.send(Ok((zip_info, freqs)));
        });

        let spinner = spinner_clone.clone();
        let status = status_clone.clone();
        let county_row = county_row_clone.clone();
        let county_dropdown = county_dropdown_clone.clone();
        let results_group = results_group_clone.clone();
        let list_box = list_box_clone.clone();
        let rows = Rc::clone(&rows_clone);
        let fetched = Rc::clone(&fetched_clone);
        let cat_model = category_model_clone.clone();
        let agency_model = agency_model_clone.clone();
        let import_btn = import_btn_clone.clone();

        receiver.attach(None, move |result| {
            spinner.stop();
            spinner.set_visible(false);

            match result {
                Ok((zip_info, frequencies)) => {
                    status.set_text(&format!(
                        "{}, {} — {} frequencies",
                        zip_info.county_name,
                        zip_info.state_name,
                        frequencies.len()
                    ));
                    status.set_css_classes(&["dim-label"]);
                    status.set_visible(true);

                    // Store fetched data
                    *fetched.borrow_mut() = frequencies.clone();

                    // Populate the frequency list
                    let bookmarks = load_bookmarks();
                    populate_frequencies(
                        &list_box,
                        &rows,
                        &frequencies,
                        &bookmarks,
                        &cat_model,
                        &agency_model,
                        &import_btn,
                    );

                    results_group.set_visible(true);
                }
                Err(e) => {
                    status.set_text(&e);
                    status.set_css_classes(&["error"]);
                    status.set_visible(true);
                    results_group.set_visible(false);
                }
            }

            glib::ControlFlow::Continue
        });
    });

    // ── Import handler ──────────────────────────────────────────────

    let rows_import = Rc::clone(&rows);
    let dialog_ref = dialog.clone();

    import_button.connect_clicked(move |_| {
        let selected: Vec<RrFrequency> = rows_import
            .borrow()
            .iter()
            .filter(|r| r.selected && !r.already_bookmarked)
            .map(|r| r.frequency.clone())
            .collect();

        if selected.is_empty() {
            return;
        }

        let mut bookmarks = load_bookmarks();

        for freq in &selected {
            let mapped = sdr_radioreference::mode_map::map_rr_mode(&freq.mode);
            let name = if freq.alpha_tag.is_empty() {
                freq.description.clone()
            } else {
                format!("{} - {}", freq.alpha_tag, freq.description)
            };

            let mut bookmark = Bookmark::new(
                &name,
                freq.freq_hz,
                mapped.demod_mode,
                mapped.bandwidth,
            );
            bookmark.rr_category = freq
                .tags
                .first()
                .map(|t| t.description.clone());
            bookmark.rr_import_id = Some(freq.id.clone());

            bookmarks.push(bookmark);
        }

        save_bookmarks(&bookmarks);
        tracing::info!(count = selected.len(), "imported RadioReference frequencies");

        dialog_ref.close();
    });

    // ── Present ─────────────────────────────────────────────────────

    let scroll_outer = gtk4::ScrolledWindow::builder()
        .child(&content)
        .build();
    dialog.set_child(Some(&scroll_outer));
    dialog.present(Some(parent));
}
```

- [ ] **Step 3: Add module to lib.rs**

In `crates/sdr-ui/src/lib.rs`, add:

```rust
pub mod radioreference;
```

- [ ] **Step 4: Add RadioReference button to header bar and wire it**

In `crates/sdr-ui/src/window.rs`:

1. In `build_header_bar()`, add a RadioReference button (near the screenshot button, packed to the end):

```rust
let rr_button = gtk4::Button::builder()
    .icon_name("network-wireless-symbolic")
    .tooltip_text("RadioReference Frequency Browser")
    .visible(crate::preferences::accounts_page::has_rr_credentials())
    .build();
```

Pack it: `header.pack_end(&rr_button);` (before the screenshot button pack_end call so it appears to the left of the screenshot button).

2. Return `rr_button` from `build_header_bar` (add it to the return tuple).

3. In `build_window()`, wire the RR button click:

```rust
rr_button.connect_clicked(move |btn| {
    crate::radioreference::show_browse_dialog(btn);
});
```

- [ ] **Step 5: Build and clippy**

Run: `cargo build --workspace && cargo clippy --all-targets --workspace -- -D warnings`
Expected: Clean

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/radioreference/ crates/sdr-ui/src/lib.rs crates/sdr-ui/src/window.rs
git commit -m "add RadioReference browse dialog with zip search and frequency import"
```

---

## Task 9: Cascading Filters + Final Wiring

**Files:**
- Modify: `crates/sdr-ui/src/radioreference/mod.rs`
- Modify: `crates/sdr-ui/src/radioreference/frequency_list.rs`

- [ ] **Step 1: Add filter-by-category logic**

In `crates/sdr-ui/src/radioreference/mod.rs`, after the frequency list is populated, wire the category and agency dropdowns to filter the list:

Connect the `category_dropdown` `notify::selected` signal to rebuild the visible list rows. When a category is selected:
- If "All" → show all rows
- Otherwise → show only rows whose first tag matches the selected category

Similarly for `agency_dropdown`:
- If "All" → show all (subject to category filter)
- Otherwise → show only rows whose `alpha_tag` matches

The filtering is client-side: just toggle `set_visible` on list rows based on the active filter values.

- [ ] **Step 2: Wire filter dropdowns in the dialog**

After the frequency list is populated (in the search handler's receiver callback), connect the filter signals:

```rust
// Category filter
let list_box_filter = list_box.clone();
let rows_filter = Rc::clone(&rows);
let cat_model_filter = cat_model.clone();
category_dropdown.connect_notify(Some("selected"), move |dd, _| {
    let idx = dd.selected();
    let filter_val = if idx == 0 {
        None // "All"
    } else {
        cat_model_filter
            .string(idx)
            .map(|s| s.to_string())
    };
    apply_filters(&list_box_filter, &rows_filter, &filter_val, &None);
});
```

Add the `apply_filters` function that iterates list box children and sets visibility based on category/agency match.

- [ ] **Step 3: Refresh bookmark list after import**

After `dialog_ref.close()` in the import handler, the main window's bookmark list needs to reload. Pass a callback or use the existing `NavigationPanel::bookmarks` `RefCell` to trigger a rebuild.

The simplest approach: since `save_bookmarks()` writes to disk, and the navigation panel's bookmark list is rebuilt from disk on demand, the next time the user interacts with bookmarks it will reload. For immediate feedback, call the existing `rebuild_bookmark_list` function after import.

Thread the bookmark reload callback into `show_browse_dialog`:

```rust
pub fn show_browse_dialog(parent: &impl IsA<gtk4::Widget>, on_import: impl Fn() + 'static)
```

In the import handler, call `on_import()` after saving. In `window.rs`, pass a closure that rebuilds the bookmark list.

- [ ] **Step 4: Build, clippy, test**

Run: `cargo build --workspace && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`
Expected: All clean

- [ ] **Step 5: Final commit**

```bash
git add crates/sdr-ui/
git commit -m "add cascading category/agency filters and bookmark refresh on import"
```

---

## Verification Checklist

After all tasks:

1. `cargo build --workspace` compiles
2. `cargo test --workspace` all tests pass
3. `cargo clippy --all-targets --workspace -- -D warnings` clean
4. Preferences window opens from app menu with General + Accounts pages
5. General page folder pickers work and persist to config
6. Accounts page: entering RR credentials + Test & Save validates via SOAP call
7. Credentials stored in OS keyring (verify via `secret-tool lookup service sdr-rs`)
8. RadioReference icon appears in header bar after credentials are saved
9. Browse dialog opens, zip code search returns frequencies
10. County picker appears when zip maps to multiple counties
11. Category/Agency filters narrow the frequency list
12. Already-bookmarked frequencies shown as saved (dimmed, checkmark icon)
13. Import adds selected frequencies as bookmarks with correct mode/bandwidth
14. Bookmark list refreshes after import
15. Existing `bookmarks.json` files still load (backward compat with new optional fields)
