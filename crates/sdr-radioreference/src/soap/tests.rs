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
fn cdata_text_is_preserved_verbatim() {
    let xml = COUNTY_ENTITY_RESPONSE_XML.replace(
        "Fire &amp; EMS &lt;all&gt;",
        "<![CDATA[Fire & EMS <all> & more]]> plus &amp; text",
    );
    let info = parse_county_info(&xml, 1).expect("parses");
    assert_eq!(
        info.categories[0].name,
        "Fire & EMS <all> & more plus & text"
    );
}

#[test]
fn fault_string_with_entities_is_fully_decoded() {
    let xml = FAULT_RESPONSE_XML.replace("Invalid API key", "Invalid &quot;key&quot; &amp; user");
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
