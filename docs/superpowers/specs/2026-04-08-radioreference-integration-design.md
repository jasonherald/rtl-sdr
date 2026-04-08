# RadioReference Integration — Design Spec

## Overview

Integrate RadioReference.com's frequency database into SDR-RS, allowing users to browse frequencies by location and import them as bookmarks. Users authenticate with their own RadioReference premium credentials. Credentials are stored securely in the OS keyring.

This also introduces the app's first `AdwPreferencesWindow` and a general-purpose credential storage layer in `sdr-config`.

## Architecture

```
sdr-config          — KeyringStore: get/set/delete secrets via OS keyring
sdr-radioreference  — NEW crate: SOAP client, RR data types, mode mapping
sdr-ui              — AdwPreferencesWindow, RadioReference browse dialog
```

### Data Flow

1. User opens Preferences > Accounts > enters RR username + password
2. "Test & Save" fires a lightweight SOAP call to validate credentials
3. Green checkmark on success > credentials saved to OS keyring
4. RadioReference icon appears in header bar (only when credentials are stored)
5. User clicks icon > browse dialog opens
6. Zip code entered > SOAP calls: `getZipCodeInfo` > county picker (if multiple) > `getCountyFreqsByTag`
7. Cascading filters: Category > Agency > Frequencies
8. User checkboxes desired frequencies > Import > auto-mapped Bookmark entries added

### Threading

SOAP calls are blocking HTTP via `reqwest::blocking::Client`. They run on GLib's thread pool via `gio::spawn_blocking()` and the result is awaited on the main thread via `glib::spawn_future_local()`. No tokio runtime needed.

## Credential Storage (`sdr-config`)

### KeyringStore

Thin wrapper around the `keyring` crate (v2):

```rust
KeyringStore::new("sdr-rs")
  .set("radioreference-username", "jason") -> Result<()>
  .get("radioreference-username") -> Result<Option<String>>
  .delete("radioreference-username") -> Result<()>
```

Three keyring entries for RadioReference:
- `radioreference-username`
- `radioreference-password`

The application API key is a compile-time constant embedded in the binary. It identifies the app, not the user, and is not secret.

### Backend

- **Linux**: Secret Service D-Bus API (GNOME Keyring, KeePassXC)
- **macOS**: Keychain

If no keyring backend is available, the `keyring` crate returns an error. The UI surfaces this as "No secure storage available — install GNOME Keyring or KeePassXC."

### Dependencies

Add `keyring = "3"` to `sdr-config/Cargo.toml`.

## SOAP Client (`sdr-radioreference`)

### Crate Structure

```
crates/sdr-radioreference/src/
  lib.rs          — pub API: RrClient, query functions
  soap.rs         — SOAP envelope construction, HTTP transport
  types.rs        — Response structs (ZipInfo, County, Category, Agency, Frequency)
  mode_map.rs     — RR mode string -> DemodMode + bandwidth mapping
```

### RrClient API

```rust
RrClient::new(username: &str, password: &str, app_key: &str) -> Self

// Validates credentials by calling getZipCodeInfo("90210") — lightweight, deterministic
fn test_connection(&self) -> Result<()>

// Returns county/state info for a US zip code
fn get_zip_info(&self, zip: &str) -> Result<ZipInfo>

// Returns all frequencies for a county, grouped by category/agency
fn get_county_frequencies(&self, county_id: u32) -> Result<Vec<RrFrequency>>
```

### SOAP Transport

Each method:
1. Builds a SOAP XML envelope with `quick-xml::Writer`
2. POSTs to `https://api.radioreference.com/soap2/` via `reqwest::blocking::Client`
3. Parses the XML response with `quick-xml::Reader`

No SOAP framework needed — the API surface is small enough for hand-crafted envelopes.

### Response Types

```rust
pub struct ZipInfo {
    pub county_id: u32,
    pub state_id: u32,
    pub city: String,
    pub county_name: String,
    pub state_name: String,
}

pub struct RrFrequency {
    pub id: String,            // RR's unique frequency ID
    pub freq_hz: u64,
    pub mode: String,          // raw RR mode string
    pub tone: Option<f32>,     // PL/CTCSS tone Hz
    pub tone_type: Option<String>,
    pub description: String,
    pub agency: String,
    pub category: String,
    pub subcategory: Option<String>,
}
```

### Mode Mapping (`mode_map.rs`)

| RR Mode | DemodMode | Default Bandwidth |
|---------|-----------|-------------------|
| FM, FMN | NFM | 12,500 Hz |
| FMW | WFM | 150,000 Hz |
| AM | AM | 10,000 Hz |
| USB | USB | 2,800 Hz |
| LSB | LSB | 2,800 Hz |
| CW | CW | 500 Hz |
| (unknown) | NFM | 12,500 Hz |

Default to NFM for unknown modes — most public safety traffic is narrowband FM.

```rust
pub struct MappedMode {
    pub demod_mode: String,    // "NFM", "WFM", "AM", etc.
    pub bandwidth: f64,        // Hz
}

pub fn map_rr_mode(rr_mode: &str) -> MappedMode
```

### Dependencies

```toml
[dependencies]
sdr-types.workspace = true
reqwest = { version = "0.12", features = ["blocking"] }
quick-xml = "0.37"
thiserror.workspace = true
tracing.workspace = true
serde.workspace = true
```

## Preferences Window (`sdr-ui`)

### File Structure

```
crates/sdr-ui/src/
  preferences/
    mod.rs              — AdwPreferencesWindow construction
    general_page.rs     — Recording/screenshot directory settings
    accounts_page.rs    — RadioReference credential entry + test
```

### Pages

**General page (`AdwPreferencesPage`):**
- `AdwActionRow` — Recording directory with folder picker button (default: `~/sdr-recordings`)
- `AdwActionRow` — Screenshot directory with folder picker button (default: `~/Pictures`)

**Accounts page (`AdwPreferencesPage`):**
- RadioReference group (`AdwPreferencesGroup`):
  - `AdwEntryRow` — Username
  - `AdwPasswordEntryRow` — Password (masked input)
  - Test & Save button:
    - Spinner while testing
    - Green checkmark + "Connected" on success
    - Red error label on failure (bad creds, network error, no keyring)
  - Remove credentials button (visible only when credentials exist)

### Access

Opened from the app menu (`GtkMenuButton` at top right) > "Preferences" menu item.

## RadioReference Browse Dialog (`sdr-ui`)

### File Structure

```
crates/sdr-ui/src/
  radioreference/
    mod.rs              — AdwDialog construction, search flow orchestration
    frequency_list.rs   — GtkListBox with check rows, filtering, duplicate detection
```

### Trigger

New icon button in the header bar (right side, near screenshot button). Uses a radio/antenna icon. Only visible when RR credentials are stored in the keyring.

### Dialog Layout

```
+---------------------------------------------+
|  RadioReference                        [X]   |
+----------------------------------------------+
|  Zip Code: [_____] [Search]                  |
|                                              |
|  County: [v dropdown]  (if multiple)         |
|                                              |
|  Category: [v All / Law Enforcement / ...]   |
|  Agency:   [v All / Springfield PD / ...]    |
|                                              |
|  +------------------------------------------+|
|  | [ ] 155.370 MHz  NFM  SpringfieldPD Disp ||
|  | [ ] 155.730 MHz  NFM  SpringfieldPD Tac1 ||
|  | [ ] 154.430 MHz  NFM  Springfield FD Disp||
|  |  *  460.025 MHz  NFM  County EMS (saved) ||
|  +------------------------------------------+|
|                                              |
|  [Import Selected (2)]                       |
+----------------------------------------------+
```

### Behavior

- **Search** fires zip lookup > populates county dropdown (auto-selects if only one) > loads all frequencies for that county
- **Category/Agency dropdowns** filter the frequency list client-side (all data fetched once per county)
- **Frequency rows** show: checkbox, formatted frequency, mode, description
- **Already-bookmarked** frequencies show a checkmark icon instead of a checkbox, dimmed row, not selectable
- **Duplicate detection** uses `rr_import_id` first (exact match on RR frequency ID), falls back to frequency Hz match
- **Import button** label shows count: "Import Selected (3)"
- **Loading states**: spinner overlay during SOAP calls

### On Import

Each selected frequency becomes a `Bookmark`:
- `name`: `"{agency} - {description}"`
- `frequency`: from RR data (Hz)
- `demod_mode`: auto-mapped via `mode_map::map_rr_mode()`
- `bandwidth`: inferred from mapped mode
- `rr_category`: RR category string (stored for future tree rework)
- `rr_import_id`: RR frequency ID (for dedup)
- All other profile fields: `None` (defaults)

## Bookmark Struct Changes

Two new optional fields on `Bookmark`:

```rust
#[serde(default)]
pub rr_category: Option<String>,    // "Law Enforcement", "Fire", "EMS", etc.

#[serde(default)]
pub rr_import_id: Option<String>,   // RR frequency ID for dedup and future sync
```

Both `#[serde(default)]` for backward compatibility with existing `bookmarks.json` files.

These fields are metadata for future use (bookmark category tree). They do not affect current bookmark display or behavior.

## New Dependencies (Workspace)

| Crate | Version | Used By | Purpose |
|-------|---------|---------|---------|
| reqwest | 0.12 (blocking feature) | sdr-radioreference | SOAP HTTP client |
| quick-xml | 0.37 | sdr-radioreference | Parse SOAP XML |
| keyring | 3 | sdr-config | OS keyring access |

## Issue Mapping

| Issue | Scope |
|-------|-------|
| #152 | KeyringStore in sdr-config |
| #153 | sdr-radioreference crate (SOAP client, types, mode map) |
| #154 | RadioReference browse dialog (zip search, county picker) |
| #155 | Frequency list with cascading category/agency filters |
| #156 | Import selected frequencies as bookmarks |
| #151 | Epic — all of the above |

Additional work not in original issues:
- AdwPreferencesWindow (General + Accounts pages)
- Header bar RadioReference button (conditional visibility)
