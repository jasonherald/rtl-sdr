//
// SdrCoreRadioReference.swift — Swift wrapper for the FFI
// RadioReference surface (credentials + search, issue #241).
//
// All of these are handle-free — they talk to the keyring or the
// RadioReference.com SOAP API, never to the engine. The FFI
// contract is documented in `include/sdr_core.h`; this file
// translates between the caller-allocated-buffer C conventions
// and Swift-native types.
//
// Threading: the search call is synchronous blocking HTTP. Hosts
// MUST dispatch it off the main actor — `Task.detached` in a
// `@MainActor` context is the standard pattern.

import Foundation
@preconcurrency import sdr_core_c

// MARK: - Public types (Codable, mirror the FFI JSON schema)

extension SdrCore {
    /// One frequency row returned by `searchRadioReference(zip:)`.
    /// Mirrors the `WireFrequency` struct on the Rust side; Codable
    /// keys match the JSON schema documented in
    /// `include/sdr_core.h` → `sdr_core_radioreference_search_zip`.
    public struct RadioReferenceFrequency: Codable, Hashable, Identifiable, Sendable {
        /// Opaque RadioReference frequency ID — stable for this
        /// row but not human-meaningful. Use for dedup / import
        /// tracking.
        public let id: String

        /// Tuner frequency in Hz.
        public let freqHz: UInt64

        /// Raw RR mode string as returned by RadioReference
        /// ("FM", "FMN", "FMW", "AM", "USB", "LSB", "CW", …).
        /// Surfaced for display; bookmarks use `demodMode` instead.
        public let rrMode: String

        /// Engine demod mode name, already mapped via
        /// `sdr_radioreference::mode_map::map_rr_mode` on the Rust
        /// side ("NFM", "WFM", "AM", "USB", "LSB", "CW"). This is
        /// what a bookmark should carry.
        public let demodMode: String

        /// Channel bandwidth in Hz, also from the mode map.
        public let bandwidthHz: Double

        /// CTCSS / PL tone in Hz, or `nil` when absent. Stored
        /// in the bookmark for future restore; not applied to
        /// the DSP during import.
        public let toneHz: Float?

        /// Human-readable description.
        public let description: String

        /// Short label (e.g. "SJ RPTR"). Preferred over
        /// `description` for display when non-empty — matches
        /// the GTK list's title field.
        public let alphaTag: String

        /// First tag description, used as the "Category" filter
        /// key in the sidebar. Empty when the row has no tags.
        public let category: String

        /// All tag descriptions in order (usually just one).
        public let tags: [String]

        enum CodingKeys: String, CodingKey {
            case id
            case freqHz       = "freq_hz"
            case rrMode       = "rr_mode"
            case demodMode    = "demod_mode"
            case bandwidthHz  = "bandwidth_hz"
            case toneHz       = "tone_hz"
            case description
            case alphaTag     = "alpha_tag"
            case category
            case tags
        }
    }

    /// Result of a ZIP search: the resolved county plus the full
    /// frequency list. Mirrors `WireSearchResult` on the Rust side.
    public struct RadioReferenceSearchResult: Codable, Sendable {
        public let countyId: UInt32
        public let countyName: String
        public let stateId: UInt32
        public let city: String
        public let frequencies: [RadioReferenceFrequency]

        enum CodingKeys: String, CodingKey {
            case countyId    = "county_id"
            case countyName  = "county_name"
            case stateId     = "state_id"
            case city
            case frequencies
        }
    }

    // MARK: - Credentials

    /// Persist RadioReference credentials to the macOS Keychain
    /// (same keychain item the Linux GTK build uses — they stay
    /// in sync if both are installed).
    public static func saveRadioReferenceCredentials(user: String, password: String) throws {
        let rc = user.withCString { uPtr in
            password.withCString { pPtr in
                sdr_core_radioreference_save_credentials(uPtr, pPtr)
            }
        }
        try checkRc(rc)
    }

    /// Load the stored credentials. Returns `nil` when no
    /// credentials are stored, throws on a genuine keyring
    /// backend failure. Distinguishes the two cases via the
    /// FFI contract:
    ///   - rc == 0 and both buffers non-empty → `(user, pass)`
    ///   - rc == 0 and either buffer empty → `nil` ("not stored")
    ///   - rc != 0 → throws with the FFI error code + message
    public static func loadRadioReferenceCredentials() throws -> (user: String, password: String)? {
        var userBuf = [CChar](repeating: 0, count: Self.credentialBufferSize)
        var passBuf = [CChar](repeating: 0, count: Self.credentialBufferSize)
        let rc = userBuf.withUnsafeMutableBufferPointer { uBuf -> Int32 in
            passBuf.withUnsafeMutableBufferPointer { pBuf -> Int32 in
                guard let uBase = uBuf.baseAddress, let pBase = pBuf.baseAddress else {
                    return -1
                }
                return sdr_core_radioreference_load_credentials(
                    uBase, uBuf.count,
                    pBase, pBuf.count
                )
            }
        }
        try checkRc(rc)
        let user = cStringToSwiftString(userBuf)
        let pass = cStringToSwiftString(passBuf)
        // FFI returns OK with empty buffers for the "not stored"
        // case (see `sdr_core_radioreference_load_credentials`
        // in `include/sdr_core.h`). Distinct from `.io` which is
        // now reserved for real backend failures — the rabbit
        // caught this on round 1 of PR #346: the earlier shape
        // lumped both into the same error code, so a broken
        // keychain looked identical to "no creds saved."
        if user.isEmpty || pass.isEmpty {
            return nil
        }
        return (user, pass)
    }

    /// Remove any stored credentials. Idempotent — calling this
    /// when no credentials are saved is a no-op.
    public static func deleteRadioReferenceCredentials() throws {
        try checkRc(sdr_core_radioreference_delete_credentials())
    }

    /// True when a non-empty username AND password are stored.
    /// Doesn't load the values — use this to gate "show
    /// RadioReference panel" without surfacing the password.
    public static var hasRadioReferenceCredentials: Bool {
        sdr_core_radioreference_has_credentials()
    }

    // MARK: - Test + search

    /// Outcome of `testRadioReferenceCredentials`. Split from
    /// plain throw/no-throw so the Settings UI can render each
    /// actionable case distinctly:
    ///   - `.valid` — credentials work
    ///   - `.invalidCredentials` — RadioReference rejected
    ///     the login (user should fix their password)
    ///   - `.invalidInput` — local validation failed before
    ///     any network call (e.g. empty user / password). Keeps
    ///     these out of the "network error" bucket so users
    ///     don't think the API is down when they just forgot
    ///     to fill in a field. Per CodeRabbit round 4.
    ///   - `.networkError` — everything else (HTTP, SOAP,
    ///     SSL, …). Retry might fix.
    public enum RadioReferenceTestResult: Sendable {
        case valid
        case invalidCredentials(String)
        case invalidInput(String)
        case networkError(String)
    }

    /// Probe RadioReference with the given credentials (does a
    /// `getZipcodeInfo("90210")` under the hood — the same check
    /// the Linux "Test & Save" button uses). Safe to call from
    /// any thread; callers should still dispatch off the main
    /// actor because the underlying HTTP is blocking.
    public static func testRadioReferenceCredentials(
        user: String,
        password: String
    ) -> RadioReferenceTestResult {
        let rc = user.withCString { uPtr in
            password.withCString { pPtr in
                sdr_core_radioreference_test_credentials(uPtr, pPtr)
            }
        }
        if rc == 0 {
            return .valid
        }
        let err = SdrCoreError.fromCurrentError(rawCode: rc)
        switch err.code {
        case .auth:
            return .invalidCredentials(err.message)
        case .invalidArg:
            return .invalidInput(err.message)
        default:
            return .networkError(err.message)
        }
    }

    /// Search RadioReference for all frequencies covering `zip`
    /// (a 5-digit US ZIP code). The FFI does `getZipInfo(zip)` +
    /// `getCountyFrequencies(county)` in one round-trip and
    /// returns a JSON blob that Codable decodes into
    /// `RadioReferenceSearchResult`.
    ///
    /// **Blocking.** Callers MUST dispatch off the main actor.
    /// Typical pattern:
    ///
    ///     Task.detached(priority: .userInitiated) {
    ///         let result = try SdrCore.searchRadioReference(
    ///             user: u, password: p, zip: "90210"
    ///         )
    ///         await MainActor.run { ... }
    ///     }
    ///
    /// Uses a grow-if-needed buffer: starts at 64 KB (typical
    /// counties return 30-300 frequencies, ~100-300 bytes each,
    /// so 64 KB fits most) and resizes once if the FFI reports
    /// a larger payload. Two calls in the worst case; one in
    /// the typical case.
    public static func searchRadioReference(
        user: String,
        password: String,
        zip: String
    ) throws -> RadioReferenceSearchResult {
        let json = try callSearchZip(user: user, password: password, zip: zip)
        let decoder = JSONDecoder()
        do {
            return try decoder.decode(RadioReferenceSearchResult.self, from: json)
        } catch {
            throw SdrCoreError(
                code: .internal,
                message: "JSON decode failed: \(error.localizedDescription)"
            )
        }
    }

    // MARK: - Private plumbing

    /// Buffer size used for credential load. 512 bytes fits any
    /// realistic username / password; truncation is not an error
    /// per the FFI contract.
    private static let credentialBufferSize = 512

    /// Initial buffer size for the search JSON — large enough
    /// that most counties fit in one round-trip. See
    /// `searchRadioReference(user:password:zip:)`.
    private static let initialSearchBufferSize = 64 * 1024

    /// Invoke the FFI search, retrying once with a
    /// larger buffer when the first call reports the payload
    /// won't fit. Returns the JSON bytes ready for `Codable`
    /// decoding.
    ///
    /// Retry contract: as of PR #346 round 3, the FFI returns
    /// `INVALID_ARG` when `out_buf` is too small (previously it
    /// returned OK and silently truncated the JSON). On that
    /// specific error, `required` is set to the exact
    /// NUL-inclusive size and we reallocate + retry once. Any
    /// other error propagates as-is.
    private static func callSearchZip(
        user: String,
        password: String,
        zip: String
    ) throws -> Data {
        var buffer = [CChar](repeating: 0, count: initialSearchBufferSize)
        var required: Int = 0

        var rc = runSearch(
            user: user, password: password, zip: zip,
            buffer: &buffer, required: &required
        )

        // Buffer too small → FFI returns InvalidArg with
        // `required` filled in. Distinct from a genuine
        // malformed-input InvalidArg because `required` is the
        // signal: non-zero AND greater than the buffer we just
        // passed means "grow and retry."
        if rc == SdrCoreError.Code.invalidArg.rawValue
            && required > 0
            && required > buffer.count
        {
            buffer = [CChar](repeating: 0, count: required)
            rc = runSearch(
                user: user, password: password, zip: zip,
                buffer: &buffer, required: &required
            )
        }
        try checkRc(rc)

        // Convert to Data by reading up to the first NUL. The
        // FFI guarantees NUL termination when rc == 0.
        return buffer.withUnsafeBufferPointer { buf -> Data in
            guard let base = buf.baseAddress else { return Data() }
            let len = strnlen(base, buf.count)
            let raw = UnsafeRawPointer(base)
            return Data(bytes: raw, count: len)
        }
    }

    private static func runSearch(
        user: String,
        password: String,
        zip: String,
        buffer: inout [CChar],
        required: inout Int
    ) -> Int32 {
        user.withCString { uPtr in
            password.withCString { pPtr in
                zip.withCString { zPtr in
                    buffer.withUnsafeMutableBufferPointer { buf -> Int32 in
                        guard let base = buf.baseAddress else { return -1 }
                        return sdr_core_radioreference_search_zip(
                            uPtr, pPtr, zPtr,
                            base, buf.count,
                            &required
                        )
                    }
                }
            }
        }
    }
}
