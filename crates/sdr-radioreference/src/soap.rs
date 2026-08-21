//! SOAP envelope builder, HTTP transport, and XML response parsers for the
//! `RadioReference` API.

use std::borrow::Cow;
use std::io::Cursor;

use quick_xml::Reader;
use quick_xml::Writer;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesDecl, BytesRef, BytesText, Event};
use reqwest::blocking::Client;

use crate::types::{CountyInfo, RrCategory, RrFrequency, RrSubcategory, RrTag, ZipInfo};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// `RadioReference` SOAP endpoint URL.
const SOAP_ENDPOINT: &str = "https://api.radioreference.com/soap2/index.php";

/// API version sent in every request.
const API_VERSION: &str = "18";

// XML namespace constants
const NS_SOAP_ENV: &str = "http://schemas.xmlsoap.org/soap/envelope/";
const NS_SOAP_ENC: &str = "http://schemas.xmlsoap.org/soap/encoding/";
const NS_XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
const NS_XSD: &str = "http://www.w3.org/2001/XMLSchema";
const NS_TNS: &str = "http://api.radioreference.com/soap2";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during SOAP communication.
#[derive(Debug, thiserror::Error)]
pub enum SoapError {
    /// HTTP transport error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// XML parsing error.
    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),

    /// I/O error during XML writing.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The server returned a SOAP fault.
    #[error("SOAP fault: {0}")]
    Fault(String),

    /// Unexpected response structure.
    #[error("unexpected response: {0}")]
    Unexpected(String),

    /// Authentication failed.
    #[error("authentication failed")]
    AuthFailed,
}

// quick-xml attribute errors can surface during parsing.
impl From<quick_xml::events::attributes::AttrError> for SoapError {
    fn from(e: quick_xml::events::attributes::AttrError) -> Self {
        Self::Xml(quick_xml::Error::InvalidAttr(e))
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Credentials used for every `RadioReference` API call.
#[derive(Clone)]
pub struct SoapAuth {
    /// `RadioReference` username.
    pub username: String,
    /// `RadioReference` password.
    pub password: String,
    /// Application key issued by `RadioReference`.
    pub app_key: String,
}

impl std::fmt::Debug for SoapAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoapAuth")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("app_key", &"[REDACTED]")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Envelope builder helpers
// ---------------------------------------------------------------------------

/// Builds a complete SOAP XML envelope for the given `method`.
///
/// `body_fn` is a closure that writes method-specific parameter elements into
/// the method body (the writer is positioned inside the `<tns:method>` element).
/// Auth info is appended automatically after `body_fn` returns.
pub fn build_envelope<F>(method: &str, auth: &SoapAuth, body_fn: F) -> Result<String, SoapError>
where
    F: FnOnce(&mut Writer<Cursor<Vec<u8>>>) -> Result<(), SoapError>,
{
    let mut writer = Writer::new(Cursor::new(Vec::new()));

    // XML declaration
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    // Build the body XML with the method call
    let method_tag = format!("tns:{method}");
    writer
        .create_element("SOAP-ENV:Envelope")
        .with_attribute(("xmlns:SOAP-ENV", NS_SOAP_ENV))
        .with_attribute(("xmlns:SOAP-ENC", NS_SOAP_ENC))
        .with_attribute(("xmlns:xsi", NS_XSI))
        .with_attribute(("xmlns:xsd", NS_XSD))
        .with_attribute(("xmlns:tns", NS_TNS))
        .write_inner_content(|w| {
            w.create_element("SOAP-ENV:Body").write_inner_content(|w| {
                w.create_element(&*method_tag).write_inner_content(|w| {
                    body_fn(w).map_err(|e| std::io::Error::other(e.to_string()))?;
                    write_auth_info(w, auth)?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })?;

    let buf = writer.into_inner().into_inner();
    String::from_utf8(buf).map_err(|e| SoapError::Unexpected(e.to_string()))
}

/// Writes the `<authInfo>` block with `appKey`, `username`, `password`,
/// `version`, and `style` elements.
fn write_auth_info(writer: &mut Writer<Cursor<Vec<u8>>>, auth: &SoapAuth) -> std::io::Result<()> {
    writer
        .create_element("authInfo")
        .with_attribute(("xsi:type", "tns:authInfo"))
        .write_inner_content(|w| {
            write_typed_element(w, "appKey", "xsd:string", &auth.app_key)?;
            write_typed_element(w, "username", "xsd:string", &auth.username)?;
            write_typed_element(w, "password", "xsd:string", &auth.password)?;
            write_typed_element(w, "version", "xsd:string", API_VERSION)?;
            write_typed_element(w, "style", "xsd:string", "rpc")?;
            Ok(())
        })?;
    Ok(())
}

/// Writes `<name xsi:type="type">value</name>`.
fn write_typed_element(
    writer: &mut Writer<Cursor<Vec<u8>>>,
    name: &str,
    xsi_type: &str,
    value: &str,
) -> std::io::Result<()> {
    writer
        .create_element(name)
        .with_attribute(("xsi:type", xsi_type))
        .write_text_content(BytesText::new(value))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP transport
// ---------------------------------------------------------------------------

/// Posts the SOAP envelope to the `RadioReference` endpoint and returns the
/// response body as a string.  Checks for SOAP faults before returning.
pub fn send_request(client: &Client, envelope: &str) -> Result<String, SoapError> {
    let resp = client
        .post(SOAP_ENDPOINT)
        .header("Content-Type", "text/xml; charset=utf-8")
        .body(envelope.to_owned())
        .send()?;

    let status = resp.status();
    let body = resp.text()?;

    // Check for SOAP faults first — the server may return a fault inside
    // either a 200 or a 500 response.
    if let Some(fault) = extract_soap_fault(&body) {
        if fault.contains("Authentication") || fault.contains("auth") || fault.contains("login") {
            return Err(SoapError::AuthFailed);
        }
        return Err(SoapError::Fault(fault));
    }

    // Reject non-success HTTP responses that weren't SOAP faults (e.g.,
    // HTML error pages, 503s).
    if !status.is_success() {
        return Err(SoapError::Unexpected(format!("HTTP {status}")));
    }

    Ok(body)
}

/// Extracts the `<faultstring>` text from a SOAP fault response, if present.
pub fn extract_soap_fault(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                if local == b"faultstring" {
                    // `read_text_content` accumulates the split
                    // Text / GeneralRef events up to `</faultstring>`.
                    // An empty element yields `""`, which is treated
                    // as "no fault text" like the pre-quick-xml-0.38
                    // single-Text-event path did.
                    return read_text_content(&mut reader)
                        .ok()
                        .filter(|t| !t.is_empty())
                        .map(Cow::into_owned);
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Public operations
// ---------------------------------------------------------------------------

/// Looks up ZIP code information (county, state, city) via the
/// `getZipcodeInfo` SOAP method.
pub fn get_zipcode_info(
    client: &Client,
    auth: &SoapAuth,
    zipcode: &str,
) -> Result<ZipInfo, SoapError> {
    let envelope = build_envelope("getZipcodeInfo", auth, |w| {
        write_typed_element(w, "zipcode", "xsd:int", zipcode)?;
        Ok(())
    })?;

    let body = send_request(client, &envelope)?;
    parse_zip_info(&body)
}

/// Fetches frequencies for a county filtered by tag via the
/// `getCountyFreqsByTag` SOAP method.
pub fn get_county_freqs_by_tag(
    client: &Client,
    auth: &SoapAuth,
    county_id: u32,
    tag: u32,
) -> Result<Vec<RrFrequency>, SoapError> {
    let county_str = county_id.to_string();
    let tag_str = tag.to_string();

    let envelope = build_envelope("getCountyFreqsByTag", auth, |w| {
        write_typed_element(w, "ctid", "xsd:int", &county_str)?;
        write_typed_element(w, "tag", "xsd:int", &tag_str)?;
        Ok(())
    })?;

    let body = send_request(client, &envelope)?;
    parse_frequencies(&body)
}

/// Fetches detailed county information including categories and subcategories
/// via the `getCountyInfo` SOAP method.
pub fn get_county_info(
    client: &Client,
    auth: &SoapAuth,
    county_id: u32,
) -> Result<CountyInfo, SoapError> {
    let county_str = county_id.to_string();

    let envelope = build_envelope("getCountyInfo", auth, |w| {
        write_typed_element(w, "ctid", "xsd:int", &county_str)?;
        Ok(())
    })?;

    let body = send_request(client, &envelope)?;
    parse_county_info(&body, county_id)
}

/// Fetches frequencies for a subcategory via the `getSubcatFreqs` SOAP method.
pub fn get_subcat_freqs(
    client: &Client,
    auth: &SoapAuth,
    scid: u32,
) -> Result<Vec<RrFrequency>, SoapError> {
    let scid_str = scid.to_string();

    let envelope = build_envelope("getSubcatFreqs", auth, |w| {
        write_typed_element(w, "scid", "xsd:int", &scid_str)?;
        Ok(())
    })?;

    let body = send_request(client, &envelope)?;
    parse_frequencies(&body)
}

// ---------------------------------------------------------------------------
// XML parsers
// ---------------------------------------------------------------------------

/// Extracts the local name from a potentially namespace-prefixed element name.
/// For example, `ns1:faultstring` becomes `faultstring`.
fn local_name(full: &[u8]) -> &[u8] {
    full.iter()
        .position(|&b| b == b':')
        .map_or(full, |pos| &full[pos + 1..])
}

/// Reads the text content of the current element (caller has just seen
/// `Event::Start` for this element).
///
/// Since quick-xml 0.38 the reader no longer unescapes text itself:
/// `Prince George&apos;s` arrives as `Text("Prince George")`,
/// `GeneralRef("apos")`, `Text("s")`. This accumulates all of those
/// fragments (resolving character references and the five
/// predefined XML entities) until the element's `End` event, so
/// callers see one contiguous string exactly as the old
/// `BytesText::unescape` path produced.
///
/// Whitespace is trimmed from the *assembled* string, never per
/// fragment: the reader's `trim_text(true)` would strip the spaces
/// on both sides of an entity (`Fire &amp; EMS` → `Fire&EMS`), so
/// no parser in this module enables it. Whitespace-only `Text`
/// events between elements are simply ignored by the `Start` /
/// `End`-driven parsers.
fn read_text_content<'a>(reader: &mut Reader<&'a [u8]>) -> Result<Cow<'a, str>, SoapError> {
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(e)) => {
                let decoded = e.xml10_content().map_err(quick_xml::Error::from)?;
                text.push_str(&decoded);
            }
            Ok(Event::GeneralRef(e)) => text.push_str(&resolve_general_ref(&e)?),
            Ok(Event::End(_)) => break,
            Ok(Event::Eof) => {
                return Err(SoapError::Unexpected(
                    "unexpected EOF while reading element text".into(),
                ));
            }
            Err(e) => return Err(SoapError::Xml(e)),
            _ => {}
        }
    }
    Ok(Cow::Owned(text.trim().to_owned()))
}

/// Resolves a `&...;` reference inside element text: numeric character
/// references (`&#39;`, `&#x27;`) and the predefined XML entities
/// (`amp`, `lt`, `gt`, `quot`, `apos`). `RadioReference` does not ship
/// a DTD, so any other entity name is an error — matching what
/// `BytesText::unescape` rejected before quick-xml 0.38.
fn resolve_general_ref(r: &BytesRef<'_>) -> Result<Cow<'static, str>, SoapError> {
    if let Some(c) = r.resolve_char_ref()? {
        return Ok(Cow::Owned(c.to_string()));
    }
    let name = r.decode().map_err(quick_xml::Error::from)?;
    resolve_predefined_entity(&name)
        .map(Cow::Borrowed)
        .ok_or_else(|| SoapError::Unexpected(format!("unknown entity reference: &{name};")))
}

/// Parses a `getZipcodeInfo` response and returns the extracted `ZipInfo`.
pub fn parse_zip_info(xml: &str) -> Result<ZipInfo, SoapError> {
    let mut reader = Reader::from_str(xml);

    let mut county_id: Option<u32> = None;
    let mut state_id: Option<u32> = None;
    let mut city: Option<String> = None;
    let mut lat: Option<String> = None;
    let mut lon: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                match local {
                    b"ctid" => {
                        let t = read_text_content(&mut reader)?;
                        county_id = Some(
                            t.parse::<u32>()
                                .map_err(|e| SoapError::Unexpected(format!("bad ctid: {e}")))?,
                        );
                    }
                    b"stid" => {
                        let t = read_text_content(&mut reader)?;
                        state_id = Some(
                            t.parse::<u32>()
                                .map_err(|e| SoapError::Unexpected(format!("bad stid: {e}")))?,
                        );
                    }
                    b"city" => {
                        city = Some(read_text_content(&mut reader)?.into_owned());
                    }
                    b"lat" => {
                        lat = Some(read_text_content(&mut reader)?.into_owned());
                    }
                    b"lon" => {
                        lon = Some(read_text_content(&mut reader)?.into_owned());
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(SoapError::Xml(e)),
            _ => {}
        }
    }

    Ok(ZipInfo {
        county_id: county_id.ok_or_else(|| SoapError::Unexpected("missing ctid".into()))?,
        state_id: state_id.ok_or_else(|| SoapError::Unexpected("missing stid".into()))?,
        city: city.ok_or_else(|| SoapError::Unexpected("missing city".into()))?,
        lat: lat.unwrap_or_default(),
        lon: lon.unwrap_or_default(),
    })
}

/// Nesting state for `parse_county_info`.
#[derive(PartialEq, Eq)]
enum CountyParseState {
    Top,
    InCats,
    InCat,
    InSubcats,
    InSubcat,
}

/// Parses a `getCountyInfo` response, extracting county name and
/// category/subcategory hierarchy.
#[allow(clippy::too_many_lines)]
pub fn parse_county_info(xml: &str, county_id: u32) -> Result<CountyInfo, SoapError> {
    use CountyParseState::{InCat, InCats, InSubcat, InSubcats, Top};

    let mut reader = Reader::from_str(xml);

    let mut county_name: Option<String> = None;
    let mut state_id: Option<u32> = None;
    let mut categories: Vec<RrCategory> = Vec::new();

    let mut state = Top;
    let mut current_cat_id: u32 = 0;
    let mut current_cat_name = String::new();
    let mut current_subcats: Vec<RrSubcategory> = Vec::new();
    let mut current_scid: u32 = 0;
    let mut current_sc_name = String::new();

    // Nesting: return > cats > item(cat) > subcats > item(subcat)
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let name_ref = name.as_ref();
                let local = local_name(name_ref);
                match (&state, local) {
                    // Leaf text fields are consumed through their
                    // `End` right here so entity-split text (quick-xml
                    // ≥ 0.38 emits `GeneralRef` events between `Text`
                    // fragments) is reassembled before use.
                    (Top, b"countyName") => {
                        county_name = Some(read_text_content(&mut reader)?.into_owned());
                    }
                    (Top, b"stid") => {
                        state_id = Some(
                            read_text_content(&mut reader)?
                                .parse()
                                .map_err(|e| SoapError::Unexpected(format!("bad stid: {e}")))?,
                        );
                    }
                    (InCat, b"cid") => {
                        current_cat_id = read_text_content(&mut reader)?
                            .parse()
                            .map_err(|e| SoapError::Unexpected(format!("bad cid: {e}")))?;
                    }
                    (InCat, b"cName") => {
                        current_cat_name = read_text_content(&mut reader)?.into_owned();
                    }
                    (InSubcat, b"scid") => {
                        current_scid = read_text_content(&mut reader)?
                            .parse()
                            .map_err(|e| SoapError::Unexpected(format!("bad scid: {e}")))?;
                    }
                    (InSubcat, b"scName") => {
                        current_sc_name = read_text_content(&mut reader)?.into_owned();
                    }
                    (Top, b"cats") => state = InCats,
                    (InCats, b"item") => {
                        state = InCat;
                        current_cat_id = 0;
                        current_cat_name.clear();
                        current_subcats.clear();
                    }
                    (InCat, b"subcats") => state = InSubcats,
                    (InSubcats, b"item") => {
                        state = InSubcat;
                        current_scid = 0;
                        current_sc_name.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                let name_ref = name.as_ref();
                let local = local_name(name_ref);
                match (&state, local) {
                    (InSubcat, b"item") => {
                        if current_scid > 0 {
                            current_subcats.push(RrSubcategory {
                                scid: current_scid,
                                name: std::mem::take(&mut current_sc_name),
                            });
                        }
                        state = InSubcats;
                    }
                    (InSubcats, b"subcats") => state = InCat,
                    (InCat, b"item") => {
                        if current_cat_id > 0 {
                            categories.push(RrCategory {
                                id: current_cat_id,
                                name: std::mem::take(&mut current_cat_name),
                                subcategories: std::mem::take(&mut current_subcats),
                            });
                        } else {
                            tracing::warn!("skipping category with missing/zero cid");
                        }
                        state = InCats;
                    }
                    (InCats, b"cats") => state = Top,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(SoapError::Xml(e)),
            _ => {}
        }
    }

    tracing::debug!(
        county_id,
        county_name = ?county_name,
        categories = categories.len(),
        subcategories = categories.iter().map(|c| c.subcategories.len()).sum::<usize>(),
        "parsed county info"
    );

    Ok(CountyInfo {
        county_id,
        county_name: county_name
            .ok_or_else(|| SoapError::Unexpected("missing countyName".into()))?,
        state_id: state_id.ok_or_else(|| SoapError::Unexpected("missing stid".into()))?,
        categories,
    })
}

/// Tracks nesting depth when parsing frequency responses, distinguishing
/// top-level `<item>` (frequency) from nested `<item>` (tag inside `<tags>`).
#[derive(Debug, PartialEq, Eq)]
enum FreqParseState {
    TopLevel,
    InFreqItem,
    InTags,
    InTagItem,
}

/// Accumulator for building a single frequency from streamed XML events.
#[derive(Default)]
struct FreqBuilder {
    fid: Option<String>,
    freq_mhz: Option<f64>,
    mode: Option<String>,
    tone_val: Option<f32>,
    description: Option<String>,
    alpha_tag: Option<String>,
    tags: Vec<RrTag>,
    tag_id: Option<u32>,
    tag_descr: Option<String>,
}

impl FreqBuilder {
    /// Resets all fields for a new frequency `<item>`.
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// Consumes accumulated fields and returns a finished `RrFrequency`.
    ///
    /// Returns `None` if required fields (`fid`, `out`, `mode`) are missing,
    /// logging a warning rather than producing a bogus entry.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn finish(&mut self) -> Option<RrFrequency> {
        let fid = self.fid.take()?;
        let freq_mhz = self.freq_mhz?;
        let mode = self.mode.take()?;

        let hz = (freq_mhz * 1_000_000.0).round() as u64;

        // `RadioReference` encodes "no tone" as an exact 0.0 literal.
        #[allow(clippy::float_cmp, reason = "0.0 is a sentinel, not a computed value")]
        let tone = self.tone_val.filter(|&t| t != 0.0);

        Some(RrFrequency {
            id: fid,
            freq_hz: hz,
            mode,
            tone,
            description: self.description.take().unwrap_or_default(),
            alpha_tag: self.alpha_tag.take().unwrap_or_default(),
            tags: std::mem::take(&mut self.tags),
        })
    }

    /// Pushes the current tag accumulator into the tags list.
    fn push_tag(&mut self) {
        self.tags.push(RrTag {
            id: self.tag_id.unwrap_or(0),
            description: self.tag_descr.take().unwrap_or_default(),
        });
    }

    /// Handles a `Start` event while inside a frequency `<item>`.
    fn handle_freq_field(
        &mut self,
        local: &[u8],
        reader: &mut Reader<&[u8]>,
    ) -> Result<Option<FreqParseState>, SoapError> {
        match local {
            b"fid" => self.fid = Some(read_text_content(reader)?.into_owned()),
            b"out" => {
                let t = read_text_content(reader)?;
                let mhz = t
                    .parse::<f64>()
                    .map_err(|e| SoapError::Unexpected(format!("bad frequency: {e}")))?;
                if !mhz.is_finite() || mhz <= 0.0 {
                    return Err(SoapError::Unexpected(format!("invalid frequency: {mhz}")));
                }
                self.freq_mhz = Some(mhz);
            }
            b"mode" => self.mode = Some(read_text_content(reader)?.into_owned()),
            b"tone" => {
                let t = read_text_content(reader)?;
                // Tone field is xsd:string — may be a float ("110.9"), empty,
                // or text like "CSQ". Parse as float, ignore failures.
                if let Ok(val) = t.parse::<f32>() {
                    self.tone_val = Some(val);
                }
            }
            b"descr" => self.description = Some(read_text_content(reader)?.into_owned()),
            b"alpha" => self.alpha_tag = Some(read_text_content(reader)?.into_owned()),
            b"tags" => return Ok(Some(FreqParseState::InTags)),
            _ => {}
        }
        Ok(None)
    }

    /// Handles a `Start` event while inside a tag `<item>`.
    fn handle_tag_field(
        &mut self,
        local: &[u8],
        reader: &mut Reader<&[u8]>,
    ) -> Result<(), SoapError> {
        match local {
            b"tagId" => {
                let t = read_text_content(reader)?;
                self.tag_id = Some(
                    t.parse::<u32>()
                        .map_err(|e| SoapError::Unexpected(format!("bad tagId: {e}")))?,
                );
            }
            b"tagDescr" => self.tag_descr = Some(read_text_content(reader)?.into_owned()),
            _ => {}
        }
        Ok(())
    }
}

/// Parses a `getCountyFreqsByTag` (or similar) response containing a list of
/// frequency `<item>` elements.
///
/// The `out` field in the response is in MHz and is converted to Hz via
/// `(mhz * 1_000_000.0).round() as u64`.  A `tone` value of `0` is mapped to
/// `None`.
pub fn parse_frequencies(xml: &str) -> Result<Vec<RrFrequency>, SoapError> {
    let mut reader = Reader::from_str(xml);

    let mut frequencies: Vec<RrFrequency> = Vec::new();
    let mut state = FreqParseState::TopLevel;
    let mut builder = FreqBuilder::default();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                match state {
                    FreqParseState::TopLevel if local == b"item" => {
                        state = FreqParseState::InFreqItem;
                        builder.reset();
                    }
                    FreqParseState::InFreqItem => {
                        if let Some(new) = builder.handle_freq_field(local, &mut reader)? {
                            state = new;
                        }
                    }
                    FreqParseState::InTags if local == b"item" => {
                        state = FreqParseState::InTagItem;
                        builder.tag_id = None;
                        builder.tag_descr = None;
                    }
                    FreqParseState::InTagItem => {
                        builder.handle_tag_field(local, &mut reader)?;
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                match state {
                    FreqParseState::InTagItem if local == b"item" => {
                        builder.push_tag();
                        state = FreqParseState::InTags;
                    }
                    FreqParseState::InTags if local == b"tags" => {
                        state = FreqParseState::InFreqItem;
                    }
                    FreqParseState::InFreqItem if local == b"item" => {
                        // Log before finish() consumes fields
                        let fid_dbg = builder.fid.clone();
                        let has_freq = builder.freq_mhz.is_some();
                        let has_mode = builder.mode.is_some();
                        if let Some(freq) = builder.finish() {
                            frequencies.push(freq);
                        } else {
                            tracing::warn!(
                                fid = ?fid_dbg,
                                has_freq,
                                has_mode,
                                "skipping frequency item with missing required fields"
                            );
                        }
                        state = FreqParseState::TopLevel;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(SoapError::Xml(e)),
            _ => {}
        }
    }

    Ok(frequencies)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Canned SOAP response for `getZipcodeInfo` -- ZIP 90210 (Beverly Hills).
    const ZIP_RESPONSE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"
    xmlns:ns1="http://api.radioreference.com/soap2"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
    xmlns:xsd="http://www.w3.org/2001/XMLSchema"
    xmlns:SOAP-ENC="http://schemas.xmlsoap.org/soap/encoding/">
  <SOAP-ENV:Body>
    <ns1:getZipcodeInfoResponse>
      <return xsi:type="ns1:ZipcodeInfo">
        <zipCode xsi:type="xsd:int">90210</zipCode>
        <lat xsi:type="xsd:string">34.0901</lat>
        <lon xsi:type="xsd:string">-118.4065</lon>
        <city xsi:type="xsd:string">Beverly Hills</city>
        <stid xsi:type="xsd:int">6</stid>
        <ctid xsi:type="xsd:int">277</ctid>
      </return>
    </ns1:getZipcodeInfoResponse>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    /// Canned SOAP response for `getCountyFreqsByTag` with two frequency items.
    const FREQS_RESPONSE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"
    xmlns:ns1="http://api.radioreference.com/soap2"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
    xmlns:xsd="http://www.w3.org/2001/XMLSchema"
    xmlns:SOAP-ENC="http://schemas.xmlsoap.org/soap/encoding/">
  <SOAP-ENV:Body>
    <ns1:getCountyFreqsByTagResponse>
      <return SOAP-ENC:arrayType="ns1:CountyFreq[2]" xsi:type="SOAP-ENC:Array">
        <item xsi:type="ns1:CountyFreq">
          <fid xsi:type="xsd:string">12345</fid>
          <out xsi:type="xsd:float">155.475</out>
          <mode xsi:type="xsd:string">FM</mode>
          <tone xsi:type="xsd:float">110.9</tone>
          <descr xsi:type="xsd:string">City Police Dispatch</descr>
          <alpha xsi:type="xsd:string">PD Disp</alpha>
          <tags SOAP-ENC:arrayType="ns1:TagInfo[1]" xsi:type="SOAP-ENC:Array">
            <item xsi:type="ns1:TagInfo">
              <tagId xsi:type="xsd:int">1</tagId>
              <tagDescr xsi:type="xsd:string">Law Dispatch</tagDescr>
            </item>
          </tags>
        </item>
        <item xsi:type="ns1:CountyFreq">
          <fid xsi:type="xsd:string">67890</fid>
          <out xsi:type="xsd:float">154.28</out>
          <mode xsi:type="xsd:string">FMN</mode>
          <tone xsi:type="xsd:float">0</tone>
          <descr xsi:type="xsd:string">County Fire Tac</descr>
          <alpha xsi:type="xsd:string">FD Tac</alpha>
          <tags SOAP-ENC:arrayType="ns1:TagInfo[1]" xsi:type="SOAP-ENC:Array">
            <item xsi:type="ns1:TagInfo">
              <tagId xsi:type="xsd:int">2</tagId>
              <tagDescr xsi:type="xsd:string">Fire Tac</tagDescr>
            </item>
          </tags>
        </item>
      </return>
    </ns1:getCountyFreqsByTagResponse>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    /// Canned SOAP fault response.
    const FAULT_RESPONSE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/">
  <SOAP-ENV:Body>
    <SOAP-ENV:Fault>
      <faultcode>SOAP-ENV:Server</faultcode>
      <faultstring>Invalid API key</faultstring>
    </SOAP-ENV:Fault>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    /// Canned success response (no fault) -- reuse the ZIP response.
    const SUCCESS_RESPONSE_XML: &str = ZIP_RESPONSE_XML;

    #[test]
    fn parse_zip_info_response() {
        let info = parse_zip_info(ZIP_RESPONSE_XML).expect("should parse zip info");
        assert_eq!(info.county_id, 277);
        assert_eq!(info.state_id, 6);
        assert_eq!(info.city, "Beverly Hills");
        assert_eq!(info.lat, "34.0901");
        assert_eq!(info.lon, "-118.4065");
    }

    #[test]
    fn parse_frequencies_response() {
        let freqs = parse_frequencies(FREQS_RESPONSE_XML).expect("should parse frequencies");
        assert_eq!(freqs.len(), 2);

        // First frequency -- 155.475 MHz with tone 110.9
        let f1 = &freqs[0];
        assert_eq!(f1.id, "12345");
        assert_eq!(f1.freq_hz, 155_475_000);
        assert_eq!(f1.mode, "FM");
        assert_eq!(f1.tone, Some(110.9));
        assert_eq!(f1.description, "City Police Dispatch");
        assert_eq!(f1.alpha_tag, "PD Disp");
        assert_eq!(f1.tags.len(), 1);
        assert_eq!(f1.tags[0].id, 1);
        assert_eq!(f1.tags[0].description, "Law Dispatch");

        // Second frequency -- 154.28 MHz with tone 0 (should be None)
        let f2 = &freqs[1];
        assert_eq!(f2.id, "67890");
        assert_eq!(f2.freq_hz, 154_280_000);
        assert_eq!(f2.mode, "FMN");
        assert_eq!(f2.tone, None);
        assert_eq!(f2.description, "County Fire Tac");
        assert_eq!(f2.alpha_tag, "FD Tac");
        assert_eq!(f2.tags.len(), 1);
        assert_eq!(f2.tags[0].id, 2);
        assert_eq!(f2.tags[0].description, "Fire Tac");
    }

    #[test]
    fn extract_fault_from_response() {
        let fault = extract_soap_fault(FAULT_RESPONSE_XML);
        assert_eq!(fault.as_deref(), Some("Invalid API key"));
    }

    #[test]
    fn no_fault_in_success_response() {
        let fault = extract_soap_fault(SUCCESS_RESPONSE_XML);
        assert!(fault.is_none());
    }

    #[test]
    fn envelope_contains_auth_info() {
        let auth = SoapAuth {
            username: "testuser".into(),
            password: "testpass".into(),
            app_key: "testkey123".into(),
        };
        let envelope =
            build_envelope("getZipcodeInfo", &auth, |_w| Ok(())).expect("should build envelope");

        assert!(envelope.contains("testuser"), "missing username");
        assert!(envelope.contains("testpass"), "missing password");
        assert!(envelope.contains("testkey123"), "missing appKey");
        assert!(envelope.contains("tns:getZipcodeInfo"), "missing method");
        assert!(envelope.contains("authInfo"), "missing authInfo element");
        assert!(envelope.contains(API_VERSION), "missing version");
    }
    /// `getCountyInfo` response carrying every kind of `&...;` reference
    /// the quick-xml ≥ 0.38 reader now surfaces as separate
    /// `Event::GeneralRef` events between `Text` fragments.
    const COUNTY_ENTITY_RESPONSE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/">
  <SOAP-ENV:Body>
    <ns1:getCountyInfoResponse>
      <return>
        <countyName>Prince George&apos;s</countyName>
        <stid>24</stid>
        <cats>
          <item>
            <cid>7</cid>
            <cName>Fire &amp; EMS &lt;all&gt;</cName>
            <subcats>
              <item>
                <scid>42</scid>
                <scName>Dispatch &#40;North&#x29; &quot;Ops&quot;</scName>
              </item>
            </subcats>
          </item>
        </cats>
      </return>
    </ns1:getCountyInfoResponse>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    #[test]
    fn county_info_reassembles_entity_split_text() {
        let info = parse_county_info(COUNTY_ENTITY_RESPONSE_XML, 1234).expect("parses");
        assert_eq!(info.county_id, 1234);
        assert_eq!(info.county_name, "Prince George's");
        assert_eq!(info.state_id, 24);
        assert_eq!(info.categories.len(), 1);
        let cat = &info.categories[0];
        assert_eq!(cat.id, 7);
        assert_eq!(cat.name, "Fire & EMS <all>");
        assert_eq!(cat.subcategories.len(), 1);
        assert_eq!(cat.subcategories[0].scid, 42);
        assert_eq!(cat.subcategories[0].name, "Dispatch (North) \"Ops\"");
    }

    #[test]
    fn fault_string_with_entities_is_fully_decoded() {
        let xml =
            FAULT_RESPONSE_XML.replace("Invalid API key", "Invalid &quot;key&quot; &amp; user");
        assert_eq!(
            extract_soap_fault(&xml).as_deref(),
            Some("Invalid \"key\" & user")
        );
    }

    #[test]
    fn empty_fault_string_is_treated_as_no_fault() {
        let xml = FAULT_RESPONSE_XML.replace("Invalid API key", "");
        assert!(extract_soap_fault(&xml).is_none());
    }

    #[test]
    fn unknown_entity_is_an_error_not_silent_truncation() {
        let xml = COUNTY_ENTITY_RESPONSE_XML.replace("&apos;", "&nbsp;");
        let err = parse_county_info(&xml, 1).expect_err("undefined entity must fail");
        assert!(err.to_string().contains("nbsp"), "{err}");
    }
}
