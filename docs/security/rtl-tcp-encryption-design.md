# rtl_tcp Transport Encryption — Design

**Status:** Design-complete, implementation not started
**Ticket:** [#397](https://github.com/jasonherald/rtl-sdr/issues/397) (research sub-issue of epic #390)
**Epic for implementation:** filed separately, cross-linked from this doc
**Author:** jasonherald
**Last updated:** 2026-04-23

---

## 1. Summary

Add confidentiality-protected transport between `rtl_tcp` clients and servers using a custom handshake layered on the existing `"RTLX"` extended protocol, AEAD stream encryption via **AES-256-GCM**, session keys derived with **HKDF-SHA256**, and optional forward secrecy via **X25519 ECDHE**. Crypto primitives provided by [`aws-lc-rs`](https://crates.io/crates/aws-lc-rs), which ships from FIPS-validated source (AWS-LC-FIPS 3.x in CMVP review; 2.x line certified at FIPS 140-3 Level 1, cert [#4816](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4816)). Encryption is opt-in per server and advertised in mDNS TXT; legacy GQRX / SDR++ clients continue to connect unencrypted against a server configured `encrypt=optional`.

**Recommendation: Option B** (custom AEAD framing over the existing `RTLX` handshake), selected after the research pass in section 5 showed:

- **Option A (TLS 1.3 via rustls)** lost its clean composition with [#394](https://github.com/jasonherald/rtl-sdr/issues/394)'s PSK because rustls does not implement TLS 1.3 external PSK ([`rustls#174`](https://github.com/rustls/rustls/issues/174), [PR #2424 closed unmerged 2025-06-13](https://github.com/rustls/rustls/pull/2424)). Without external PSK we're back to cert distribution + fingerprint pinning, which is the UX cost the ticket flagged.
- **Option C (Noise)** has no shipped FIPS implementation — [`snow`](https://crates.io/crates/snow) uses RustCrypto which is not FIPS-validated. Building a Noise state machine over `aws-lc-rs` primitives is a project in itself.
- **Option B** is the smallest viable surface that (a) reuses the existing RTLX handshake byte for byte, (b) composes additively with PSK auth, and (c) uses only primitives that are in AWS-LC's FIPS service boundary.

### 1.1 What v1 ships

- AES-256-GCM AEAD framing on the data stream after a successful handshake.
- HKDF-SHA256 session-key derivation from an X25519 ECDHE shared secret (+ optional PSK as the HKDF salt).
- mDNS TXT advertises `encrypt=off|optional|required`; client UI shows an "🔒 Encrypted" indicator in the status bar on a successful encrypted connection.
- Server and client UI toggles to require encryption.
- Cargo feature `fips` that switches `aws-lc-rs` to `aws-lc-fips-sys`, with platform-caveat docs (section 12.2).
- Benchmarks (section 13) proving the crypto path doesn't bottleneck the 2.4 Msps IQ stream on a Raspberry Pi 4 class target.

### 1.2 What v1 does NOT ship

- Certificate-based auth / PKI. The handshake is either anonymous ECDHE (opportunistic) or PSK-authenticated ECDHE (when [#394](https://github.com/jasonherald/rtl-sdr/issues/394) is also configured).
- Post-quantum KEM (ML-KEM). Out of scope per ticket; mechanism is protocol-version-bumpable later.
- Windows FIPS builds. Not a target platform (nothing else in the workspace targets Windows).
- Formal FIPS 140-3 validated binary claims on user-facing platforms. Section 12.2 explains why.

### 1.3 What's left for the implementation epic

Implementation is estimated at **≥5 PRs** following the sub-issue pattern that worked for epic [#390](https://github.com/jasonherald/rtl-sdr/issues/390). Sub-tickets filed in section 15.

---

## 2. Threat Model

### 2.1 In scope

- **On-LAN packet sniffing.** A passive attacker on the same Wi-Fi / Ethernet segment sees only ciphertext + non-sensitive protocol framing after the handshake lands.
- **Port-forwarded home lab exposure.** Users who NAT-forward an `rtl_tcp` server to the internet for personal remote access get confidentiality that's at least as strong as their residential ISP + home router stack.
- **Opportunistic active attacker on the handshake.** Mitigated if the handshake integrates a pre-shared key ([#394](https://github.com/jasonherald/rtl-sdr/issues/394)) — without the PSK, the anonymous ECDHE path gives confidentiality against a passive sniffer but is vulnerable to an MITM that can impersonate the server to the client (no server identity binding).

### 2.2 Out of scope

- **Targeted state-level adversaries.** This is a hobbyist LAN radio stack. Users needing WAN-grade security should tunnel through [WireGuard](https://www.wireguard.com/) or [OpenSSH](https://www.openssh.com/) port-forwarding — both are mature, both work transparently on top of the cleartext `rtl_tcp` protocol today.
- **Side-channel attacks** on the host (timing, power, cache). AWS-LC has constant-time implementations for the primitives we use, but no system-level side-channel hardening is claimed.
- **Compliance certification of deployments.** See section 12.2. The code can be built from FIPS-validated source; the deployed binary on the user's laptop is not itself a validated CMVP module.
- **Replay of replayable commands.** The rtl_tcp command channel carries state-setting commands (tune, gain) that are inherently replayable at the application layer — an attacker who can inject a previously-captured `SetCenterFreq` command into a future session causes no real harm because the user would just retune. Per-session nonces still prevent cross-session replay of ciphertext; within a session the sequence counter prevents intra-session replay (section 9.4).

### 2.3 Security posture summary

| Attacker capability              | Cleartext today | v1 anonymous ECDHE | v1 PSK + ECDHE |
|----------------------------------|-----------------|---------------------|----------------|
| Passive LAN sniff                | broken          | protected           | protected      |
| Passive WAN sniff                | broken          | protected           | protected      |
| Active MITM (handshake-phase)    | broken          | broken              | protected      |
| Active MITM (after handshake)    | broken          | protected           | protected      |
| Compromise server host           | broken          | broken              | broken         |

"Broken" = attacker reads/modifies the stream. "Protected" = attacker sees only ciphertext / cannot forge frames.

---

## 3. Goals

1. **FIPS 140-3 Level 1 compatible primitives only.** Section 4 enumerates the choice set and section 5.1 discusses the gap between "compatible primitives" and "validated deployment."
2. **Opt-in per server.** Legacy clients (GQRX, SDR++, old `rtl_tcp` builds, [libvxi11](https://sourceforge.net/projects/libvxi11/) consumers) must continue to work against a server configured `encrypt=optional`.
3. **Zero ambiguity on the wire about whether encryption is active.** mDNS advertises the server's policy; the client's connection state carries an `is_encrypted: bool` so the UI can show the correct indicator; a downgrade attack that strips encryption is detectable by the user (the indicator doesn't light up).
4. **No performance bottleneck** on the 2.4 Msps IQ stream on a Raspberry Pi 4 class target. AES-NI on x86_64 and ARMv8-A crypto extensions on aarch64 make AES-256-GCM essentially free at ~4.8 MB/s; section 13 specifies the benchmark targets.
5. **Composes additively with existing epic #390 features.** Role system ([#392](https://github.com/jasonherald/rtl-sdr/issues/392)), takeover ([#393](https://github.com/jasonherald/rtl-sdr/issues/393)), pre-shared-key auth ([#394](https://github.com/jasonherald/rtl-sdr/issues/394)), listener cap ([#392](https://github.com/jasonherald/rtl-sdr/issues/392)) all work the same whether the transport is encrypted or not.
6. **Doesn't compromise the existing legacy-safety guarantee.** The `RTLX` handshake bytes only go on the wire when the client has out-of-band evidence the server speaks the extension (mDNS `codecs=3` or cached knowledge). Encryption opt-in follows the same gate.

---

## 4. Primitive Choices (FIPS-Compatible)

### 4.1 Symmetric AEAD

**AES-256-GCM.** Approved per [NIST SP 800-38D](https://csrc.nist.gov/pubs/sp/800/38/d/final). Native-instruction accelerated on x86_64 (AES-NI + CLMUL) and ARMv8-A (`aes`/`pmull` features). Throughput at 2.4 Msps ingress = 4.8 MB/s; typical AES-NI throughput is 1-5 GB/s per core. Overhead is lost in the noise.

**Why not ChaCha20-Poly1305.** ChaCha20-Poly1305 is NOT in FIPS 140-3 as of early 2026 (verified against [NIST CMVP Implementation Guidance](https://csrc.nist.gov/projects/cryptographic-module-validation-program/fips-140-3-standards), [SP 800-38D](https://csrc.nist.gov/pubs/sp/800/38/d/final)), and both `aws-lc-fips-sys` and OpenSSL-FIPS explicitly exclude it from their service boundaries. The initial ticket incorrectly claimed ChaCha20-Poly1305 was approved as of 140-3 Level 1 — that premise was wrong and is noted here so a future reader doesn't reintroduce it.

**IV construction.** AES-GCM IV reuse is catastrophic. v1 follows the deterministic construction in [SP 800-38D §8.2.1](https://csrc.nist.gov/pubs/sp/800/38/d/final): the 96-bit IV is split into a 32-bit fixed field (derived from the HKDF output and the direction label) and a 64-bit invocation counter. The counter increments monotonically per frame per direction. At the 2.4 Msps rate with, say, 16 KB ciphertext chunks (75 frames/s per direction), a 64-bit counter won't roll for ~7.8 billion years, so we just fail hard (close the connection) if it ever did.

### 4.2 Key derivation

**HKDF-SHA256.** Approved per [SP 800-56C Rev. 2](https://csrc.nist.gov/pubs/sp/800/56/c/r2/final). Used to:

1. Derive a root session secret from the ECDHE shared secret + (optional) PSK salt + per-session nonce (extract step).
2. Split the root secret into a client-to-server AEAD key + server-to-client AEAD key + per-direction IV fixed-field (expand step, `info` differentiates per sub-key).

HKDF-SHA384 would also be acceptable and slightly stronger against brute-force but adds no practical security against realistic attackers; SHA-256 matches AES-256 security at 128 bits and keeps the doc simpler.

### 4.3 Key agreement (forward secrecy)

**X25519.** Approved per [FIPS 186-5](https://csrc.nist.gov/pubs/fips/186-5/final) + [SP 800-186](https://csrc.nist.gov/pubs/sp/800/186/final), published 2023-02. `SP 800-56A Rev. 3` covers ECDH on these curves when combined with an approved KDF. X25519 is chosen over P-256 for three reasons:

1. Constant-time implementations are simpler + easier to audit — X25519 has a well-studied reference impl that avoids the branching hazards of general-purpose elliptic-curve arithmetic.
2. Public keys are 32 bytes vs. 65 bytes uncompressed (P-256), which makes the handshake payload smaller and easier to fit in a single TCP segment.
3. X25519 key generation is about 2× faster than P-256 in AWS-LC benchmarks. At the handshake rate this project sees (one per user-driven Connect click), this is cosmetic — but "slightly better everything, same FIPS status" is a clean pick.

**Note on the research correction.** The original ticket stated "NOT Curve25519 (not FIPS-approved as of this writing)." That was correct when the ticket was filed but is no longer correct: FIPS 186-5 added it in February 2023. This doc updates the recommendation.

### 4.4 RNG

AWS-LC's default DRBG (CTR-DRBG, NIST SP 800-90A). Seeded from `getrandom(2)` on Linux / `getentropy(3)` on macOS. No Rust-side RNG code is written — we use `aws_lc_rs::rand::SystemRandom` and let AWS-LC do the right thing.

### 4.5 Crypto provider

**`aws-lc-rs`** ([crates.io](https://crates.io/crates/aws-lc-rs), [docs](https://docs.rs/aws-lc-rs)) via its `fips` Cargo feature when that's enabled. Without the `fips` feature, the same crate uses `aws-lc-sys` (same AWS-LC codebase, non-FIPS branch) — so the non-FIPS build still runs AWS-LC, just not from the reviewed FIPS module subset. It does NOT fall back to `ring` or RustCrypto.

**Not evaluated further:**

- **`ring`** — not FIPS certified, author has explicitly declined to pursue it.
- **`RustCrypto`** (aes-gcm, hkdf, chacha20poly1305 crates) — no FIPS validation plan.
- **`openssl`** — routes through system-installed OpenSSL-FIPS, which shifts the validation burden onto the user's packaging (matching OpenSSL version, matching `fipsmodule.cnf`, provider loaded correctly). Workable but significantly more friction than `aws-lc-rs`'s one-feature-flag opt-in. See section 12.3 for a "why not OpenSSL" note.

---

## 5. Option Analysis (Post-Research)

### 5.1 FIPS baseline realities

Three facts from the research pass that reshape every option:

1. **AWS-LC-FIPS 2.x is the current certified module line.** Certs [#4816](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4816) (static, 2024-10-01) and [#4759](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4759) (dynamic, 2024-08-14), both FIPS 140-3 Level 1.
2. **AWS-LC-FIPS 3.x is in CMVP review** but not yet fully certified as of 2026-04-23 ([modules-in-process list](https://csrc.nist.gov/projects/cryptographic-module-validation-program/modules-in-process/modules-in-process-list)). `aws-lc-rs` currently ships from 3.x source.
3. **Cert #4759/#4816 Tested Operational Environments are narrow:** Amazon Linux 2, Amazon Linux 2023, Ubuntu 22.04, each on Graviton3 (aarch64) or Intel Xeon Platinum 8275CL (x86_64). No macOS. No Arch. No Raspberry Pi 4. No Windows.

**Implication for the claim model.** The design target is "built from FIPS-validated source code using a crate configured in FIPS mode" — NOT "FIPS-validated deployment" as a CMVP module. That's a meaningful carve-out: a user who needs compliance-grade FIPS (auditor, DoD, regulated industry) needs a deployment-specific validated build on a covered OS + CPU combination. Our binary running on Arch Linux on a Ryzen workstation is "FIPS-algorithmic" at best. Section 12.2 documents this explicitly in user-facing form.

### 5.2 Option A: TLS 1.3 via rustls + aws-lc-rs

**Pros:**
- IETF-standardized handshake, extensively audited.
- Rustls + aws-lc-rs provider is the FIPS-friendly default for new TLS-in-Rust work ([rustls 0.23 FIPS manual](https://docs.rs/rustls/0.23.39/rustls/manual/_06_fips/index.html)).
- Under `features = ["fips"]`, rustls restricts cipher suites to FIPS-approved AES-GCM only, automatically.
- Handshake cost < 1 ms on modern cores ([rustls 2026-03-07 perf report](https://rustls.dev/perf/2026-03-07-report/)).

**Cons (after research):**
- **No external PSK support in rustls.** [Issue #174](https://github.com/rustls/rustls/issues/174) open since 2018; [PR #2424](https://github.com/rustls/rustls/pull/2424) (the most complete implementation attempt) closed unmerged 2025-06-13 with "pending future work." Without external PSK, the "TLS 1.3 PSK-only (no cert)" sub-option in the original ticket is off the table.
- That leaves cert-based TLS, which means:
  - Users must generate a server cert (openssl CLI, `rcgen` crate, or a CLI wizard we'd need to build).
  - Clients must pin the cert's public-key fingerprint on first connect (TOFU model) because we don't run a CA.
  - mDNS needs to carry the fingerprint or a hash of it, so discovery can prompt "connect to this server? fingerprint SHA-256 {hex}".
- That UX is ~3-5 additional UI surfaces: fingerprint display in the server panel, fingerprint display + approve flow in the client discovery expander, "re-pin after cert change" path, "import fingerprint from QR code" nice-to-have.
- Larger dependency graph: `rustls` + `tokio-rustls` (if we go async) + `webpki` (for any cert validation we do beyond raw fingerprint) + `aws-lc-rs` + transitive deps. Today the SDR project has zero TLS deps.
- **Provider-selection churn in rustls 0.24.** The 0.23 → 0.24 transition makes the provider explicit on `ClientConfig` / `ServerConfig` ([rustls-aws-lc-rs 0.1.0-dev.0](https://crates.io/crates/rustls-aws-lc-rs)). Minor, but a version-bump treadmill to track.

**When this would win:** If `rustls#174` external-PSK lands in the next 6-12 months, Option A re-becomes competitive and we could migrate. The protocol surface we design in Option B is intentionally small and version-bumpable, so a future TLS migration would be a wire-format-parallel layer, not a from-scratch redesign.

### 5.3 Option B: Custom AEAD framing over the RTLX handshake (RECOMMENDED)

**Pros:**
- **Reuses the existing `RTLX` handshake surface** from [#307](https://github.com/jasonherald/rtl-sdr/issues/307) / [#391–#396](https://github.com/jasonherald/rtl-sdr/issues/390). New `FLAG_REQUEST_ENCRYPT` bit in the existing `ClientHello.flags` byte, new fields appended in the `ServerExtension` when both ends opt in. Version-bump from v2 (`RTLX` post-auth) to v3 (`RTLX` post-encryption) in the existing version byte.
- **Crypto dependency surface is just `aws-lc-rs`.** No TLS stack, no certificate toolchain, no `webpki`, no `tokio-rustls`. One workspace dep addition.
- **Clean composition with PSK.** If the user has `#394` auth configured, the PSK enters the HKDF salt alongside the ECDHE shared secret, binding session keys to both factors. If no PSK, the HKDF salt is a constant — anonymous ECDHE, confidentiality-only.
- **Small protocol surface = small review surface.** Section 6 specifies 3 new wire fields and one handshake sub-step. The whole protocol spec fits on a page.
- **Forward secrecy comes for free.** X25519 ECDHE in every handshake, ephemeral per connection. No long-term key material on disk other than the optional PSK.

**Cons:**
- **Custom protocol = custom review.** IETF-grade TLS has been pounded on for a decade; this handshake has been pounded on by one person over a weekend. Mitigated by (a) small size — the handshake is specified below at byte-exact level, (b) reusing well-studied primitive compositions (Noise-IK-equivalent pattern, KDF chaining as in TLS 1.3), (c) adding implementation-level tests that exercise tamper, replay, and downgrade cases.
- **Nonce reuse is catastrophic and has to be architecturally prevented.** The IV construction in section 4.1 combined with the per-direction key split (HKDF-derived, not shared between directions) prevents it structurally, but it's worth flagging explicitly that this is the single highest-risk invariant in the whole feature and deserves its own code review + test coverage.
- **No session resumption.** Every connection does a fresh ECDHE. Cheap (sub-ms) so it doesn't matter at the connection rate this project sees.

### 5.4 Option C: Noise with FIPS primitives

**Pros:**
- Noise is designed for exactly this problem. [Noise_XXpsk3_25519_AESGCM_SHA256](https://noiseprotocol.org/noise.html#handshake-patterns) is almost exactly what we want.
- `snow` crate is mature and well-tested.

**Cons:**
- **No shipped `snow`-over-`aws-lc-rs` resolver.** `snow` has a pluggable `CryptoResolver` trait ([docs](https://docs.rs/snow/latest/snow/resolvers/trait.CryptoResolver.html)) but every published resolver uses RustCrypto or `ring`, neither of which is FIPS certified.
- **Implementing a resolver is non-trivial.** `snow` expects specific trait surfaces for DH, Hash, and Cipher backends. Getting them FIPS-compliant requires a ~500-line new crate just for the wiring.
- **That implementation has zero independent crypto review.** We'd be shipping a custom Noise-aws-lc-rs binding — the "most-correct protocol design" ends up landing in the "custom crypto code, sleep with one eye open" bucket anyway.
- **Protocol-level benefit over Option B is modest for this use case.** Noise XX gives mutual authentication with static keys, which is overkill for a LAN stream-radio server. Noise IK needs the responder's static key known by the initiator — we don't have a cert / static-key distribution story, so we're back to Noise NK (server static key only) or NN (anonymous), which offer no more than Option B.

Option C is deferred to "revisit if we want mutual static-key auth" and would be a follow-up epic if that case ever matters.

### 5.5 Recommendation: Option B

Option B wins on:

- Minimal dependency growth (one crate: `aws-lc-rs`).
- Clean composition with PSK auth (HKDF salt = PSK).
- Small review surface (one page of protocol spec, section 6).
- FIPS-compatibility via already-research-validated primitive choices (section 4).
- Future-proofing: the version byte in the `RTLX` handshake lets a future v4 migration to TLS or Noise happen additively.

---

## 6. Wire Protocol

This section specifies byte-exact wire format. See [`sdr-server-rtltcp::extension`](https://github.com/jasonherald/rtl-sdr/blob/main/crates/sdr-server-rtltcp/src/extension.rs) for existing fields the new protocol extends.

### 6.1 Existing `ClientHello` (v2, as shipped by #394)

```
Offset  Bytes  Field           Values
  0     4      Magic           "RTLX"
  4     1      codec_mask      bit 0 = None-codec, bit 1 = LZ4
  5     1      role            0 = Control, 1 = Listen (#392)
  6     1      flags           bit 0 = FLAG_REQUEST_TAKEOVER (#393)
                               bit 1 = FLAG_HAS_AUTH (#394)
  7     1      version         v1 (no auth) or v2 (auth/takeover)
```

### 6.2 New `ClientHello` (v3, this feature)

```
Offset  Bytes  Field           Values
  0     4      Magic           "RTLX"
  4     1      codec_mask      (unchanged)
  5     1      role            (unchanged)
  6     1      flags           bit 0 = FLAG_REQUEST_TAKEOVER
                               bit 1 = FLAG_HAS_AUTH
                               bit 2 = FLAG_REQUEST_ENCRYPT       ← NEW
                               bit 3 = FLAG_ENCRYPT_REQUIRED      ← NEW
  7     1      version         v3 (encryption-capable hello)
  8     32     client_x25519_pk  Client's ephemeral X25519 public key ← NEW
                                  (all zeros when FLAG_REQUEST_ENCRYPT not set,
                                  so non-encrypted connects keep the v2 hello
                                  semantics byte-for-byte in the first 8 bytes)
```

**Wire compatibility with v1/v2 servers:** v1/v2 servers read the 8-byte hello and use the version byte to gate parsing. A v3 hello against a v1/v2 server fails the version check and the server closes the connection with `Status::Protocol` (the existing protocol-error path). The client then falls back to the same "try without hello" logic that gates `RTLX` today (mDNS `codecs=3`).

**FLAG_ENCRYPT_REQUIRED semantics:** If the client sets this bit, the server MUST respond with encryption enabled or reject the handshake with `Status::EncryptionRequired` (new status code, defined below). If cleared, the client will accept either encrypted or unencrypted (opportunistic mode). The server's own policy (`encrypt=required` in mDNS / config) takes precedence — a `required` server refuses any handshake where the client didn't set `FLAG_REQUEST_ENCRYPT`, even if the client sent `FLAG_ENCRYPT_REQUIRED=0`.

### 6.3 Existing `ServerExtension` (v2, pre-encrypt)

```
Offset  Bytes  Field            Values
  0     4      Magic            "RTLX"
  4     1      codec            negotiated codec (0 = None, 1 = LZ4)
  5     1      granted_role     0 = Control, 1 = Listen (per #392)
  6     1      status           Status byte (OK / ControllerBusy / AuthRequired / ...)
  7     1      version          v2
```

### 6.4 New `ServerExtension` (v3, this feature)

```
Offset  Bytes  Field               Values
  0     4      Magic               "RTLX"
  4     1      codec               (unchanged)
  5     1      granted_role        (unchanged)
  6     1      status              Status byte; new values:
                                      EncryptionRequired = 0x06  ← NEW
                                      EncryptionNegotiationFailed = 0x07  ← NEW
  7     1      version             v3 if server negotiated encryption; v2 if not
  8     32     server_x25519_pk    Server's ephemeral X25519 public key  ← NEW
                                    (all zeros when the server declined to
                                    enable encryption)
 40     32     hkdf_salt           Per-session random salt used alongside
                                   the PSK (if any) in the HKDF Extract
                                   step. Server-generated via SystemRandom.
```

**Why the salt is server-chosen:** The server generating the salt means client-side bugs in the RNG can't bias the key derivation alone. AWS-LC's DRBG is reseeded from `getrandom(2)` so both sides contribute entropy, but routing through the server's AEAD-quality RNG adds defense-in-depth for the "client has a weak PRNG in some embedded scenario" edge.

### 6.5 Handshake sequence (encryption-enabled path)

```
    Client                                        Server
    ─────────                                     ─────────
    Open TCP
    Gen ephemeral X25519 keypair (c_sk, c_pk)
    Send ClientHello(v3)                ──────→   Receive ClientHello
                                                  Validate hello (magic, version,
                                                    flag consistency, PSK if
                                                    present per #394)
                                                  Gen ephemeral X25519 keypair
                                                    (s_sk, s_pk)
                                                  Gen hkdf_salt (32 random bytes)
                                                  If auth required: check PSK bytes
                                                    arriving after the hello
                                                    (existing #394 path)
                                                  Compute dhe = X25519(s_sk, c_pk)
                                                  Compute session keys via HKDF
                                                    (see 6.6)
                                                  Send ServerExtension(v3)
                                                    including s_pk, hkdf_salt, and
                                                    the existing DongleInfo        ←────
    Receive ServerExtension(v3)
    Compute dhe = X25519(c_sk, s_pk)
    Compute session keys via HKDF
    Send AEAD-encrypted Status::Ok acknowledgement      ──────→
                                                  Verify AEAD acknowledgement
                                                    (proves client actually holds
                                                    the DHE shared secret and
                                                    derived the same keys)
                                                  All subsequent command + IQ traffic
                                                    is AEAD-framed
                                                                                     ←──→
    Steady state:
      Client → Server: 5-byte commands, each wrapped in one AEAD frame
      Server → Client: IQ bytes, framed in ~16 KB AEAD frames
```

The **AEAD-acknowledgement** step is the key addition: without it, the server doesn't know the client successfully derived matching session keys, and a malformed client could send plaintext on the "encrypted" socket for a few frames before the server's AEAD-decrypt fails. The ack is a single encrypted frame with the 1-byte `Status::Ok` payload; if AEAD verify fails the server closes the connection immediately with no further bytes.

### 6.6 Key derivation (HKDF inputs)

```
IKM    = ECDHE shared secret (32 bytes, output of X25519(c_sk, s_pk))
SALT   = hkdf_salt (32 bytes from ServerExtension)
         XOR                                          (if PSK configured per #394)
         HMAC-SHA256(PSK, "sdr-rtl-tcp-v1-psk-salt")  (derived constant; avoids
                                                       using raw PSK as salt
                                                       which would leak length)
INFO   = "sdr-rtl-tcp-v1-session-root" || context_suffix

PRK    = HKDF-Extract(salt=SALT, ikm=IKM)
ROOT   = HKDF-Expand(prk=PRK, info=INFO, length=96)

c2s_key   = ROOT[0..32]          # 32 bytes, client-to-server AES-256 key
s2c_key   = ROOT[32..64]         # 32 bytes, server-to-client AES-256 key
c2s_iv_fixed = ROOT[64..68]      # 4 bytes, client-to-server IV fixed field
s2c_iv_fixed = ROOT[68..72]      # 4 bytes, server-to-client IV fixed field
hs_ack_key   = ROOT[72..96]      # 24 bytes, handshake-ack sub-key (unused by
                                 # steady-state traffic; reserved for the
                                 # AEAD ack verification in 6.5 and future
                                 # rotation triggers in 9.6)
```

The **IV construction** per SP 800-38D §8.2.1:

```
iv[0..4]   = iv_fixed           # per-direction, from HKDF
iv[4..12]  = u64_be(frame_counter)   # monotonic per direction
```

Frame counters are independent per direction (no interleaving), never reset within a session, and overflow fails the connection.

`context_suffix` includes the protocol version byte, a 1-byte "auth-was-used" bit, and a copy of the client and server X25519 public keys. This binds the session keys to the handshake transcript, preventing a reflection-style attack where a recorded server_extension could be replayed against a different client.

### 6.7 Frame format (post-handshake)

```
Offset  Bytes  Field
  0     4      u32_be  length (ciphertext length NOT including this field or the tag)
  4     N      ciphertext  (AES-256-GCM output)
  4+N   16     auth_tag   (GCM authentication tag)
```

The sender builds a nonce from `iv_fixed || counter`, encrypts the plaintext with GCM using the session's direction key, writes the length + ciphertext + tag in one `write_all`. The receiver reads the length, bounds-checks it (max 1 MB per frame as a sanity guard), reads ciphertext + tag, verifies. Any verification failure is a hard close with `Status::EncryptionIntegrityFailure` logged server-side.

### 6.8 What goes inside each frame

- **Command channel (client → server):** each 5-byte rtl_tcp command is its own 5-byte AEAD frame. Small frame size means per-frame auth-tag overhead (~23 bytes for 5-byte payload) is high in relative terms, but the command channel is sparse (a handful of commands per user action) so absolute bandwidth is negligible.
- **IQ stream (server → client):** the server batches IQ bytes up to a configurable frame size (default 16 KB = one poll's worth on the existing pipeline) and encrypts the batch as a single frame. LZ4 compression ([#307](https://github.com/jasonherald/rtl-sdr/issues/307)) applies BEFORE encryption — encrypt compressed ciphertext, not the other way around.

### 6.9 Graceful shutdown

AES-GCM doesn't have a "close notify" like TLS. An unclean TCP close is indistinguishable from an attacker cutting the connection. v1 accepts this: a truncated stream reads as "connection ended" and the reconnect state machine kicks in. No secret material leaks on truncation because the AEAD tag either verifies or doesn't.

---

## 7. mDNS Changes

### 7.1 New TXT fields

| Field            | Values                               | Semantics                                                                                                   |
|------------------|--------------------------------------|-------------------------------------------------------------------------------------------------------------|
| `encrypt`        | `off` \| `optional` \| `required`    | Server's policy. Absent = treat as `off` for back-compat.                                                   |
| `encrypt_v`      | `1`                                  | Version of the encryption wire format the server supports. Reserved for future protocol bumps.              |

Existing `codecs`, `auth_required`, `version`, etc. are unchanged.

### 7.2 Client-side gating

The existing client only sends `ClientHello` bytes when mDNS + config says the server speaks RTLX (existing rule from #307 / #394 to keep legacy rtl_tcp servers safe). Encryption adds a sub-rule:

- `encrypt=off` → client MUST NOT set `FLAG_REQUEST_ENCRYPT`. Fall through to existing v2 hello semantics.
- `encrypt=optional` → client MAY set `FLAG_REQUEST_ENCRYPT` per user preference.
- `encrypt=required` → client MUST set `FLAG_REQUEST_ENCRYPT` AND MAY additionally set `FLAG_ENCRYPT_REQUIRED` for defense-in-depth against a server misconfiguration.

### 7.3 Downgrade-attack awareness

An attacker on the local mDNS segment could inject a spoofed `_rtl_tcp._tcp.local.` announce with `encrypt=off` to steer the client at an unencrypted endpoint. The client's UI addresses this two ways:

1. The status bar shows "🔒 Encrypted" (green) ONLY when the handshake actually negotiated encryption. An attacker-downgraded connection reads as unencrypted in the UI — user-visible.
2. The client panel has an "Always require encryption" global toggle. With it on, the client refuses `encrypt=off` servers at the UI level (greyed-out Connect button), so mDNS spoofing can't reach a hello emission.

---

## 8. UI Surface

### 8.1 Server panel (AdwPreferencesGroup)

- New `AdwSwitchRow`: **"Require encrypted connections"**. Default off. Persists to `server.rtl_tcp.require_encryption` in sdr-config. Live-update during `rtl_tcp_server_running` flips the server's effective policy immediately (same pattern as the auth toggle from [#406](https://github.com/jasonherald/rtl-sdr/pull/406)).
- New `AdwSwitchRow`: **"Accept encrypted connections"**. Default on. Controls whether the server accepts `FLAG_REQUEST_ENCRYPT` at all. Off = server is purely legacy; encrypted clients get `Status::EncryptionNegotiationFailed`.
- When both are on, the mDNS TXT emits `encrypt=required`. "Accept only" = `encrypt=optional`. "Neither" = `encrypt=off`.

### 8.2 Client panel (source panel)

- New `AdwSwitchRow`: **"Always require encryption"**. Global client-side policy. When on, the Connect button is greyed out on discovery rows whose `encrypt=off` / missing, and the auth-row + role-row flows work as today but the hello carries `FLAG_ENCRYPT_REQUIRED`.
- New status-bar indicator: **"🔒 Encrypted"** badge, shown only during an encrypted `Connected` state. Reuses the [#408](https://github.com/jasonherald/rtl-sdr/pull/408) `RtlTcpRoleBadge` infrastructure; the encryption badge renders to the right of the role badge. Hidden on unencrypted or non-Connected states.
- Discovery row subtitle gets a lock emoji prefix when `encrypt=required` or `encrypt=optional` (not when `off`), so the user sees at a glance which servers advertise encryption.

### 8.3 Connection-state enum changes

`sdr_types::RtlTcpConnectionState::Connected` gains a `is_encrypted: bool` field, threaded from `sdr_source_network::ConnectionState::Connected.is_encrypted` via the same `From` impl pattern that today carries `granted_role`. Projection to the public type keeps `is_encrypted` as a plain `bool` so no new cross-crate dep is needed.

One new terminal state: `RtlTcpConnectionState::EncryptionNegotiationFailed` for the case where the client demanded encryption and the server refused (or vice versa). UI shows a toast "Encrypted connection required but the server doesn't support it" with a Disconnect action.

### 8.4 FFI implications

ABI bumps 0.18 → 0.19 to add:

- `SDR_RTL_TCP_STATE_ENCRYPTION_NEGOTIATION_FAILED = 8` on `SdrRtlTcpConnectionStateKind`.
- `is_encrypted: u8` field on `SdrEventRtlTcpConnectionState` (0 = unencrypted, 1 = encrypted).

Follows the same ABI-bump pattern as [#408](https://github.com/jasonherald/rtl-sdr/pull/408)'s 0.17 → 0.18 bump.

---

## 9. Replay, Rotation, Forward Secrecy

### 9.1 Replay protection within a session

Per-direction AES-GCM nonce counter (section 4.1) makes every frame's nonce unique. Replay of a captured frame causes the receiver's expected counter to mismatch, which triggers AEAD-tag verification failure → connection close. No application-layer replay window needed.

### 9.2 Replay across sessions

A new `hkdf_salt` per session means session keys are always fresh. A captured ciphertext from session A decrypts to garbage when fed into session B. Cross-session replay is structurally impossible.

### 9.3 Rekey / rotation

v1 does NOT rekey within a session. Justification:

- The longest reasonable rtl_tcp session is hours to days (a scanner running unattended). At 2.4 Msps × 8 bytes/sample × 3600s = 69 GB/hour. AES-256-GCM has a 2^64 block limit before the IV space starts reusing within a single key. That's ~18 EB of plaintext at 128-bit block size, which at 69 GB/hour is ~3 × 10^10 hours. We will not hit this.
- Session counter overflow is caught explicitly (section 4.1).
- Adding rekey means sending a new handshake mid-stream, which complicates the framer state machine for no practical security gain.

### 9.4 Forward secrecy

Every handshake is a fresh X25519 ECDHE. If an attacker later compromises the PSK (via [#394](https://github.com/jasonherald/rtl-sdr/issues/394)'s keyring entry, or by coercing the user into sharing it), they CANNOT decrypt prior captured traffic because the ephemeral ECDHE keypairs are discarded at session end.

If an attacker compromises a running session's host process (via RCE, say), they can read all in-memory key material — no session persists secrets after close. That's standard forward secrecy semantics.

### 9.5 PSK rotation policy

- Server UI's "Regenerate" button (existing from [#406](https://github.com/jasonherald/rtl-sdr/pull/406)) regenerates the PSK, which means subsequent session keys are derived from a different HKDF salt (since the salt mixes in an HMAC of the PSK). Old sessions stay decryptable to whoever's running them; new sessions use the new PSK.
- Client-side, the existing per-server keyring entry ([#408](https://github.com/jasonherald/rtl-sdr/pull/408)) gets the new key via the same user-re-enter path that already exists.

### 9.6 Future rekey trigger (not shipped v1)

The `hs_ack_key` HKDF sub-key (section 6.6) is reserved for a future in-session rekey handshake. Plausible use: every 2^31 frames per direction, emit a new handshake that derives fresh session keys from the current ECDHE secret + a new hkdf_salt + the previous handshake transcript. Not needed for v1 — reserved for implementation-epic sub-ticket (section 15).

---

## 10. Performance

### 10.1 Analytic estimate

At 2.4 Msps × 2 bytes/sample (8-bit I/Q) = 4.8 MB/s ingress. At 16 KB frame size = 300 frames/s from server to client.

- AES-256-GCM on a Raspberry Pi 4 (Cortex-A72 with ARMv8-A `aes` + `pmull` extensions): AWS-LC benchmarks ~400-700 MB/s single-core. Our load is 4.8 MB/s. Overhead: ~1% CPU.
- On x86_64 with AES-NI: 2-5 GB/s. Our load is 4.8 MB/s. Overhead: ~0.1% CPU.
- Per-frame overhead: 4 bytes length + 16 bytes tag = 20 bytes on ~16 KB of payload = 0.12% bandwidth overhead.
- Per-frame AEAD call overhead: ~5-10 µs AES-NI / ~30-50 µs Cortex-A72. At 300 frames/s that's 1.5-15 ms/s of CPU = 0.15-1.5% core utilization.

Conclusion: encryption costs are lost in the noise at this traffic profile.

### 10.2 Benchmark plan (acceptance)

Implementation epic MUST include before-and-after benchmarks on:

- **Workstation target** (user's RTX 4080 Super box, x86_64 with AES-NI). Expected: encrypted ≈ unencrypted within noise.
- **Low-end target** (Raspberry Pi 4, aarch64 Cortex-A72). Expected: encrypted adds < 3% CPU vs. unencrypted at sustained 2.4 Msps.
- **Handshake latency**: LAN connect → Connected state transition, with + without encryption. Expected delta: < 5 ms on workstation, < 50 ms on Pi.

Regression threshold for shipping: the encrypted path must not slow steady-state IQ delivery by more than **5%** on the Pi target. Workstation figures are informational.

---

## 11. Implementation Risks

### 11.1 Nonce reuse

Catastrophic for AES-GCM. Mitigated by:

- Per-direction keys (section 6.6). A bug that re-uses a nonce on one direction doesn't expose the other direction's keystream.
- Counter is a u64 that only increments, never decrements. The implementation keeps it in a `std::sync::atomic::AtomicU64` and uses `fetch_add(1, Ordering::Relaxed)` before every send; decoder uses the same counter via a separate `AtomicU64` per direction.
- Unit tests: feed a frame with a repeated counter into the decoder and assert it fails. Fuzz tests: shuffle frame order in transit and assert decoder recognizes gaps.

### 11.2 Handshake downgrade

An active attacker who can rewrite the mDNS announce could present `encrypt=off` and intercept a client that hasn't set the global "Always require encryption" toggle. Mitigated by:

- UI indicator visible in the status bar — downgrade to cleartext is visually obvious.
- Client-side "Always require encryption" toggle (section 8.2) closes the gap for users who care.

### 11.3 Key-material leakage

- Session keys live in `Vec<u8>` allocated inside `aws-lc-rs` `Aead` objects. On drop, `aws-lc-rs` zeroes the buffers — this is AWS-LC's standard behavior.
- PSK bytes in the keyring are accessed via `secret-service` on Linux / `keychain` on macOS. The `SecretManager` in the existing codebase already calls `zeroize()` on loaded values.
- The ECDHE ephemeral private keys live in `aws-lc-rs` `EphemeralPrivateKey` types which zeroize on drop.

### 11.4 Non-constant-time comparison

AES-GCM auth-tag comparison must be constant-time. `aws-lc-rs::aead::Aead::open_in_place` does this internally. We must NOT add any wrapping comparison on top (like `==` on decrypted bytes) that would leak timing info.

### 11.5 Version pinning

Protocol v3 version byte is checked by the server before any cryptographic work. A client with a hello version > the server's max supported version gets `Status::Protocol` and disconnects — no attempted crypto against mismatched fields.

### 11.6 `aws-lc-rs` version pinning for FIPS claim

The FIPS cert [#4816](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4816) is pinned to a specific `aws-lc-fips-sys` version. Cargo's SemVer lets us track `aws-lc-rs = "1.x"` but we should vendor-pin the exact `aws-lc-fips-sys` version in our lockfile AND document it in the changelog of any version bump, so a user who builds with `fips` knows which cert their binary inherits from.

---

## 12. Build + Deployment

### 12.1 Cargo feature

```toml
# sdr-server-rtltcp/Cargo.toml
[features]
default = []
encrypt = ["dep:aws-lc-rs"]
fips = ["encrypt", "aws-lc-rs/fips"]

[dependencies]
aws-lc-rs = { version = "1.14", optional = true }
```

Default build: encryption code absent. `--features encrypt`: encryption compiled in, non-FIPS `aws-lc-sys` backend. `--features fips`: encryption compiled in, `aws-lc-fips-sys` backend (implies `encrypt`).

### 12.2 FIPS-mode build (user-facing doc)

A new doc section in [`CLAUDE.md`](../../CLAUDE.md) and README under "Build flavors":

> **FIPS-mode build.** Build with `make install CARGO_FLAGS="--release --features fips"` to link against AWS-LC-FIPS. The resulting binary uses crypto primitives from a FIPS-validated code path.
>
> **This is NOT the same as a FIPS-validated deployment.** The AWS-LC FIPS certificates cover tested operational environments:
>
> - Linux x86_64: **Amazon Linux 2, Amazon Linux 2023, Ubuntu 22.04** only, on Intel Xeon Platinum 8275CL (cert [#4759](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4759)).
> - Linux aarch64: same OSes, on AWS Graviton3 only.
> - macOS, Windows, Arch, NixOS, Raspberry Pi 4: **not on any cert**.
>
> Building `--features fips` on an uncovered platform still runs FIPS-reviewed code — the algorithmic implementations are the same — but the resulting deployment is not a CMVP-validated module. If you need a CMVP-validated binary for compliance (auditor, DoD, regulated industry), build on one of the covered environments; even then, submit for your own vendor affirmation.
>
> For personal LAN use, the practical security benefit of `--features fips` over the default (non-FIPS AWS-LC) is nil — both run the same constant-time AES-GCM. The feature exists to let users who specifically need "I can point at a FIPS-validated source code path" do so.
>
> **Build requirements** (FIPS only): CMake, Go toolchain, C compiler. Ubuntu: `apt install build-essential cmake golang libclang1`. The non-FIPS default path needs none of these.

### 12.3 Why not OpenSSL-FIPS

Briefly addressed: the `openssl` Rust crate wraps system-installed OpenSSL, so FIPS mode becomes the user's packaging problem — install OpenSSL 3.x with FIPS provider, ensure `fipsmodule.cnf` is correct, load the FIPS provider in `openssl.cnf`, match the validated OpenSSL version. Workable but much higher friction than `aws-lc-rs`'s one-feature-flag opt-in. We'd revisit this only if `aws-lc-rs` became unmaintained, which doesn't look likely.

---

## 13. Benchmark + Acceptance

See section 10.2. The implementation epic's "benchmarks" sub-ticket MUST include:

- Steady-state IQ delivery CPU % (4080 Super + Pi 4).
- Handshake connect-to-Connected latency (LAN, median + p99 over 100 samples).
- Memory footprint delta (session keys + ECDHE keypair buffers) — expect negligible.

Acceptance for merge: encryption adds < 5% CPU at sustained 2.4 Msps on the Pi target. No merge if that bar is missed.

---

## 14. Out of Scope (explicit)

- **Mutual authentication beyond PSK.** Options: client certs, client-side static X25519 keys. Deferred — no user-facing need identified.
- **Post-quantum KEM.** ML-KEM-768 hybrid with X25519 is a future protocol version bump. Not shipping in v1. Documenting the hook point (the ECDHE step in section 6.5) means adding it later is additive.
- **Traffic padding.** Ciphertext lengths reveal plaintext lengths. An attacker who knows "this server is streaming at 2.4 Msps" can infer activity even under encryption. Not a stated threat in this project's model.
- **Session resumption.** v1 does a fresh ECDHE every connection. Sub-ms cost, no ops burden.
- **Multi-hop / proxy scenarios.** If someone proxies rtl_tcp through a generic TCP proxy, encryption works end-to-end as long as the proxy is purely pass-through.
- **Windows support.** Not a project target.
- **SwiftUI macOS port integration.** Will follow the same Linux-first pattern as everything else in epic #390. Separate ticket in the macOS mirror epic.

---

## 15. Implementation Plan (Sub-tickets)

The epic (to be filed at the end of this design phase) has the following sub-tickets. Each bullet becomes its own GitHub issue following the epic #390 pattern.

### 15.1 Wire protocol + handshake (protocol PR)

- New `FLAG_REQUEST_ENCRYPT` / `FLAG_ENCRYPT_REQUIRED` bits in `sdr_server_rtltcp::extension::ClientHello.flags`.
- `v3` version byte, `client_x25519_pk` field.
- `ServerExtension` v3 additions: `server_x25519_pk`, `hkdf_salt`, new `Status::EncryptionRequired` and `Status::EncryptionNegotiationFailed`.
- Handshake sequence from section 6.5, including the AEAD-acknowledgement step.
- HKDF derivation from section 6.6.
- **No UI, no mDNS.** Just the protocol and the codec module that wraps the TCP stream in AEAD frames.
- Unit tests for: happy-path encrypted handshake, anonymous ECDHE (no PSK), PSK+ECDHE, version mismatch, nonce-reuse detection, tamper detection, replay detection, truncation handling.
- Protocol fuzz tests: random wire bytes must never cause panics or UB.

### 15.2 Frame codec + session keys (crypto PR)

- `sdr-server-rtltcp::extension::codec::Encrypted` implementor of the existing `Codec` trait, wrapping a `TcpStream` with AEAD framing.
- Session-key `SessionKeys` struct owned by `RtlTcpConnectionState::Connected` internally; `zeroize`-on-drop.
- Per-direction `AtomicU64` nonce counters + overflow handling.
- Integration tests: end-to-end connect, exchange commands, exchange IQ, disconnect, verify no plaintext on the wire (wireshark-style: bind the test server on localhost, snoop with a second test socket, confirm bytes are random-looking).

### 15.3 mDNS + server UI (server-side PR)

- TXT field additions (`encrypt=off|optional|required`, `encrypt_v=1`).
- Server panel switches (section 8.1).
- `sdr-server-rtltcp::ServerConfig` gets `require_encryption: bool` + `accept_encryption: bool`.
- Live-update: flipping the switches reconfigures a running server's hello-acceptance policy without restart (same pattern as `require_auth` in [#406](https://github.com/jasonherald/rtl-sdr/pull/406)).

### 15.4 Client UI + discovery (client-side PR)

- Client-side "Always require encryption" toggle (section 8.2).
- Lock indicator in status bar + discovery row prefix.
- `RtlTcpRoleBadge`-adjacent `RtlTcpEncryptionBadge` or `is_encrypted: bool` on the Connected state.
- `EncryptionNegotiationFailed` state handling in `handle_rtl_tcp_state_toast`.
- FFI bump 0.18 → 0.19 (section 8.4).

### 15.5 FIPS build flavor (doc + CI PR)

- Cargo feature `fips` wiring (section 12.1).
- `scripts/fips-build-verify.sh` that builds with the feature + runs `aws_lc_rs::try_fips_mode()` at startup, greps the binary for the `aws-lc-fips-sys` version string.
- CI job that adds a `cargo check --features fips` build to the existing matrix. Doesn't run the full test suite under FIPS — the FIPS feature gates have to be validated on a covered platform, and our CI runs on Ubuntu-latest which drifts.
- README + CLAUDE.md updates per section 12.2.

### 15.6 Benchmarks (perf PR)

- Benchmark harness: Criterion-based micro-bench for AEAD frame round-trip (ignorable tiny numbers, mostly for regression catching), and a runtime-integration perf harness that runs a client+server pair on the Pi target for 60 seconds of sustained 2.4 Msps and reports mean/p99 CPU, throughput, handshake latency.
- Report format: a `benches/encryption-perf.md` artifact committed with numbers on the CI runner for regression baseline.
- Acceptance criterion from section 10.2.

### 15.7 Follow-up research tickets (lower urgency)

- Post-quantum KEM hybrid (ML-KEM-768 + X25519).
- Session rekey trigger (the `hs_ack_key` reserve from section 6.6).
- macOS SwiftUI integration (mirror of 15.4).
- WebAssembly / browser client compatibility evaluation — likely incompatible with X25519 via wasm-crypto, separate research.

---

## 16. Open Questions

1. **Frame size.** 16 KB default matches the current LZ4 block size for codec compatibility. Should it be larger (32 KB, 64 KB) to reduce per-frame tag overhead? Probably not — LZ4 already benefits from smaller blocks on non-uniform signal stretches. Leave at 16 KB.
2. **Should `encrypt=off` servers emit the `encrypt` TXT field at all?** Two interpretations: (a) emit `encrypt=off` explicitly so clients can tell "didn't advertise" from "advertises off"; (b) omit the field entirely — absent means off. v1 chooses (a) because it's better UX: "this server doesn't support encryption" is more reassuring than "I can't tell what this server supports."
3. **Should client-side "Always require encryption" default on?** Arguments for on: safety by default, nudges servers to opt in to encryption. Arguments for off: most existing deployed `rtl_tcp` servers (GQRX relay, SDR++ server) don't advertise `encrypt` and would become unreachable from sdr-rs clients by default. v1: default off, flag in release notes that users who care about encryption should flip it on.
4. **Should we add a CLI verification tool?** `sdr-rtl-tcp --verify-encryption host:port` that does the handshake + dumps the encryption status + tears down. Useful for scripting / CI in third-party deployments. Low priority; could be a community follow-up.
5. **PSK + no ECDHE fallback?** In a hypothetical environment where ECDHE is compromised (broken curve) but PSK is still strong, we could derive session keys from PSK alone. v1 rejects this path because it forfeits forward secrecy for no clear threat model benefit. Can be added as an opt-in if a concrete threat ever motivates it.

---

## 17. References

**NIST / FIPS:**

- [FIPS 186-5 (2023-02)](https://csrc.nist.gov/pubs/fips/186-5/final) — adds Curve25519/Ed25519 to approved curves
- [SP 800-56A Rev. 3](https://csrc.nist.gov/pubs/sp/800/56/a/r3/final) — ECDH key agreement
- [SP 800-56C Rev. 2](https://csrc.nist.gov/pubs/sp/800/56/c/r2/final) — key derivation via HKDF
- [SP 800-38D](https://csrc.nist.gov/pubs/sp/800/38/d/final) — AES-GCM authenticated encryption
- [SP 800-186](https://csrc.nist.gov/pubs/sp/800/186/final) — elliptic-curve recommendations
- [FIPS 140-3 Implementation Guidance](https://csrc.nist.gov/projects/cryptographic-module-validation-program/fips-140-3-standards)
- [CMVP validated modules search](https://csrc.nist.gov/projects/cryptographic-module-validation-program/validated-modules)

**AWS-LC / aws-lc-rs:**

- [`aws-lc-rs` crate](https://crates.io/crates/aws-lc-rs) · [docs.rs](https://docs.rs/aws-lc-rs)
- [Repo README](https://github.com/aws/aws-lc-rs/blob/main/aws-lc-rs/README.md)
- [Platform support matrix](https://aws.github.io/aws-lc-rs/platform_support.html)
- [Build requirements (Linux)](https://aws.github.io/aws-lc-rs/requirements/linux.html)
- [CMVP cert #4816 (static, 2.0, FIPS 140-3 Level 1)](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4816)
- [CMVP cert #4759 (dynamic, 2.0, FIPS 140-3 Level 1)](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4759)
- [AWS-LC-FIPS 3.x status (modules-in-process)](https://csrc.nist.gov/projects/cryptographic-module-validation-program/modules-in-process/modules-in-process-list)
- [AWS blog: AWS-LC-FIPS 3.0 with ML-KEM](https://aws.amazon.com/blogs/security/aws-lc-fips-3-0-first-cryptographic-library-to-include-ml-kem-in-fips-140-3-validation/)

**rustls (rejected option):**

- [`rustls` crate](https://crates.io/crates/rustls) · [FIPS manual](https://docs.rs/rustls/0.23.39/rustls/manual/_06_fips/index.html)
- [Rustls #174 — external PSK support](https://github.com/rustls/rustls/issues/174) — open since 2018
- [Rustls PR #2424 — closed PSK impl](https://github.com/rustls/rustls/pull/2424)
- [Rustls performance report 2026-03-07](https://rustls.dev/perf/2026-03-07-report/)

**Noise / `snow`:**

- [Noise protocol spec](https://noiseprotocol.org/noise.html)
- [`snow` Rust crate](https://github.com/mcginty/snow) · [CryptoResolver trait](https://docs.rs/snow/latest/snow/resolvers/trait.CryptoResolver.html)

**Related sdr-rs issues:**

- [#307 rtl_tcp LZ4 stream compression](https://github.com/jasonherald/rtl-sdr/issues/307) — PR [#399](https://github.com/jasonherald/rtl-sdr/pull/399)
- [#390 multi-client epic](https://github.com/jasonherald/rtl-sdr/issues/390)
- [#391 broadcaster](https://github.com/jasonherald/rtl-sdr/issues/391) — PR [#402](https://github.com/jasonherald/rtl-sdr/pull/402)
- [#392 role system](https://github.com/jasonherald/rtl-sdr/issues/392) — PR [#403](https://github.com/jasonherald/rtl-sdr/pull/403)
- [#393 takeover](https://github.com/jasonherald/rtl-sdr/issues/393) — PR [#404](https://github.com/jasonherald/rtl-sdr/pull/404)
- [#394 pre-shared key auth](https://github.com/jasonherald/rtl-sdr/issues/394) — PR [#405](https://github.com/jasonherald/rtl-sdr/pull/405)
- [#395 server UI](https://github.com/jasonherald/rtl-sdr/issues/395) — PR [#406](https://github.com/jasonherald/rtl-sdr/pull/406)
- [#396 client UI](https://github.com/jasonherald/rtl-sdr/issues/396) — PR [#408](https://github.com/jasonherald/rtl-sdr/pull/408)
- [#397 — this ticket (transport encryption design)](https://github.com/jasonherald/rtl-sdr/issues/397)
