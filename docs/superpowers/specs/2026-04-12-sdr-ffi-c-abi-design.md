---
name: sdr-ffi C ABI — Design
description: Hand-rolled C ABI exposing the sdr-core Engine to native consumers (SwiftUI, future hosts), including header layout, command/event surface, and threading rules
type: spec
---

# `sdr-ffi` Hand-Rolled C ABI — Design

**Status:** Draft
**Date:** 2026-04-12
**Parent epic:** `2026-04-12-swift-ui-macos-epic-design.md`
**Depends on:** `2026-04-12-sdr-core-extraction-design.md` (must land first)
**Tracking issues:** TBD

---

## Goal

Define and implement a stable C ABI that exposes the `sdr-core::Engine` to non-Rust consumers. The first consumer is the SwiftUI macOS app via a thin Swift Package wrapper (`SdrCoreKit`); the design allows other consumers (future C++ tools, Python bindings, whatever) without redesign.

The ABI is **hand-written**. No `swift-bridge`, no `uniffi`, no `cbindgen`. The header file `include/sdr_core.h` is the source of truth and is checked into the repo alongside the Rust source. PR review covers the header and the Rust implementation in lockstep.

## Non-Goals

- **No async runtime crossing the FFI boundary.** Rust side stays sync (channels + threads). Swift side wraps the C surface in `AsyncStream` / `actor` types itself. This keeps the C interface trivial and lets each consumer pick its own concurrency story.
- **No generic "send any message" interface.** We do not pass JSON or msgpack over FFI. Each command and event is a typed C function/struct. The reasoning: we want the compiler (both `rustc` and `swiftc`) to catch shape mismatches at build time, and we want zero serialization cost on the audio path.
- **No transcription, no bookmarks, no RadioReference in v1.** Those land in v2 of the ABI alongside their UI panels. The header is versioned (see *ABI Versioning*) so adding them is additive, not breaking.
- **No SwiftUI code in this spec.** The Swift Package wrapper is sketched here only enough to verify the C surface is usable; the full SwiftUI app is in `2026-04-12-swift-ui-surface-design.md`.
- **No plugin system.** The library statically registers the same source/sink set that `sdr-core` does.

## Background

`sdr-core` (after the extraction series) exposes a Rust `Engine` struct with:
- `Engine::new(config_path) -> Result<Self, EngineError>`
- `Engine::send_command(UiToDsp) -> Result<(), EngineError>`
- `Engine::subscribe() -> Option<Receiver<DspToUi>>`
- `Engine::pull_fft<F>(&self, f: F) -> bool`
- `Engine::shutdown(self) -> Result<(), EngineError>`

The C ABI mirrors this surface. Three things cross the boundary:

1. **Commands** (host → engine): typed C functions, one per `UiToDsp` variant we expose in v1.
2. **Events** (engine → host): a single C callback registered once, called from the engine's dispatcher thread with a tagged-union event struct.
3. **FFT frames** (engine → host): a pull function the host calls from its render tick. Zero-copy via a borrow into a Rust-owned buffer for the duration of one callback.

## Crate Layout

```text
crates/sdr-ffi/
├── Cargo.toml
├── cbindgen.toml         — config used by `make ffi-header`; NOT in the build path
├── build.rs              — sets staticlib name, rerun-if-changed for the header
└── src/
    ├── lib.rs            — re-exports + #[no_mangle] entry points
    ├── handle.rs         — opaque handle type, lifetime tracking
    ├── error.rs          — error code enum + thread-local last-error
    ├── command.rs        — command C functions (one per UiToDsp variant in v1)
    ├── event.rs          — event struct, dispatcher thread, callback marshaling
    └── fft.rs            — pull function

include/
└── sdr_core.h            — hand-written header, source of truth, in repo
```

**`crates/sdr-ffi/Cargo.toml`:**

```toml
[package]
name = "sdr-ffi"
version = "0.1.0"
edition.workspace = true

[lib]
name = "sdr_core"
crate-type = ["staticlib"]   # v1: link statically into .app to dodge @rpath
                              # v2 may add cdylib for hot-reload during dev

[dependencies]
sdr-core = { path = "../sdr-core" }
sdr-types.workspace = true
tracing.workspace = true
tracing-subscriber = { workspace = true, optional = true }
libc.workspace = true

[features]
default = ["log_oslog"]
log_oslog = ["dep:tracing-subscriber"]   # routes Rust tracing to os_log on macOS
```

> **Why not `cbindgen` for header generation?** We use `cbindgen` *manually* (`make ffi-header`) only to **diff** the generated header against the hand-written one as a CI lint, catching drift between Rust signatures and the header. The hand-written `include/sdr_core.h` remains the canonical artifact reviewed in PRs. This gives us a human-readable header (with comments, sectioning) and a machine-checked safety net.

## Design Principles

1. **Opaque handle.** `SdrCoreHandle` is an opaque pointer; the host never dereferences it. All operations are functions taking the handle as their first argument. One handle = one engine instance.
2. **Errors return integers.** Functions that can fail return `int32_t` where `0 == OK` and negative values are `SdrCoreError` enum variants. A separate `sdr_core_last_error_message()` returns a thread-local C string with the most recent error's text. This is the standard C-FFI pattern (`errno`-style) and avoids out-parameters.
3. **No allocations across the boundary.** Strings passed in are caller-owned `const char*` (UTF-8). Strings passed out are pointers into Rust-owned static or thread-local storage with an explicit lifetime contract (valid until the next call on the same thread).
4. **No panics.** Every `#[no_mangle] extern "C"` function is wrapped in `std::panic::catch_unwind`. A panic returns `SDR_CORE_ERR_INTERNAL` and sets the last-error message. This is required: a Rust panic across an FFI boundary is UB.
5. **Reentrancy is explicit.** The header documents which functions can be called from the event callback (most can; the destroy function cannot). This is the same rule GTK has for its main loop.
6. **No `Send`-unsafe types in the API.** All inputs are POD or owned by the C side. All outputs are values copied into caller-provided buffers, or borrows valid for the callback's duration.

## Header Sketch

This is the v1 surface. Comments are abbreviated for the spec; the real header is fully documented.

```c
// include/sdr_core.h
// Hand-written FFI for sdr-core. Source of truth.
// ABI version: see SDR_CORE_ABI_VERSION below.
//
// Threading: see "Threading model" section in the design doc.

#ifndef SDR_CORE_H
#define SDR_CORE_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ============================================================ */
/*  ABI versioning                                              */
/* ============================================================ */

#define SDR_CORE_ABI_VERSION_MAJOR 0
#define SDR_CORE_ABI_VERSION_MINOR 1

/* Returns the ABI version the dylib was built with.
 * Hosts call this once at startup and abort if it doesn't match
 * what they were compiled against. */
uint32_t sdr_core_abi_version(void);

/* ============================================================ */
/*  Errors                                                      */
/* ============================================================ */

typedef enum SdrCoreError {
    SDR_CORE_OK                  =  0,
    SDR_CORE_ERR_INTERNAL        = -1,  /* panic, unwrap, etc. */
    SDR_CORE_ERR_INVALID_HANDLE  = -2,
    SDR_CORE_ERR_INVALID_ARG     = -3,
    SDR_CORE_ERR_NOT_RUNNING     = -4,
    SDR_CORE_ERR_DEVICE          = -5,  /* USB / RTL-SDR open/io */
    SDR_CORE_ERR_AUDIO           = -6,  /* CoreAudio */
    SDR_CORE_ERR_IO              = -7,  /* file/network */
    SDR_CORE_ERR_CONFIG          = -8,
} SdrCoreError;

/* Thread-local. Valid until the next sdr_core_* call on this thread.
 * Returns NULL if no error has been set on this thread. */
const char* sdr_core_last_error_message(void);

/* ============================================================ */
/*  Lifecycle                                                   */
/* ============================================================ */

typedef struct SdrCore SdrCore;   /* opaque */

/* Initialize logging. Idempotent. Routes Rust `tracing` events to os_log
 * on macOS, stderr on Linux. Optional — call once before sdr_core_create
 * if you want logs. `min_level`: 0=trace 1=debug 2=info 3=warn 4=error. */
void sdr_core_init_logging(uint8_t min_level);

/* Create an engine. `config_path_utf8` is the JSON config file location,
 * caller-owned. The engine reads it on startup and writes on shutdown.
 * On success, *out_handle is set and 0 is returned. */
int32_t sdr_core_create(const char* config_path_utf8, SdrCore** out_handle);

/* Destroy. Blocks until the DSP thread joins. Always succeeds.
 * After this call the handle is invalid. NOT reentrant — must not be
 * called from the event callback. */
void sdr_core_destroy(SdrCore* handle);

/* ============================================================ */
/*  Commands (host -> engine)                                   */
/* ============================================================ */

/* Lifecycle */
int32_t sdr_core_start(SdrCore*);
int32_t sdr_core_stop (SdrCore*);

/* Tuning */
int32_t sdr_core_tune                 (SdrCore*, double freq_hz);
int32_t sdr_core_set_vfo_offset       (SdrCore*, double offset_hz);
int32_t sdr_core_set_sample_rate      (SdrCore*, double rate_hz);
int32_t sdr_core_set_decimation       (SdrCore*, uint32_t factor);
int32_t sdr_core_set_ppm_correction   (SdrCore*, int32_t ppm);

/* Tuner */
int32_t sdr_core_set_gain             (SdrCore*, double gain_db);
int32_t sdr_core_set_agc              (SdrCore*, bool enabled);

/* Demod */
typedef enum SdrDemodMode {
    SDR_DEMOD_WFM = 0,
    SDR_DEMOD_NFM = 1,
    SDR_DEMOD_AM  = 2,
    SDR_DEMOD_USB = 3,
    SDR_DEMOD_LSB = 4,
    SDR_DEMOD_DSB = 5,
    SDR_DEMOD_CW  = 6,
    SDR_DEMOD_RAW = 7,
} SdrDemodMode;

int32_t sdr_core_set_demod_mode       (SdrCore*, SdrDemodMode mode);
int32_t sdr_core_set_bandwidth        (SdrCore*, double bw_hz);
int32_t sdr_core_set_squelch_enabled  (SdrCore*, bool enabled);
int32_t sdr_core_set_squelch_db       (SdrCore*, float db);

typedef enum SdrDeemphasis {
    SDR_DEEMPH_NONE = 0,
    SDR_DEEMPH_US75 = 1,
    SDR_DEEMPH_EU50 = 2,
} SdrDeemphasis;
int32_t sdr_core_set_deemphasis       (SdrCore*, SdrDeemphasis mode);

/* Audio */
int32_t sdr_core_set_volume           (SdrCore*, float volume_0_1);

/* IQ frontend */
int32_t sdr_core_set_dc_blocking      (SdrCore*, bool enabled);
int32_t sdr_core_set_iq_inversion     (SdrCore*, bool enabled);
int32_t sdr_core_set_iq_correction    (SdrCore*, bool enabled);

/* Spectrum */
int32_t sdr_core_set_fft_size         (SdrCore*, size_t n);    /* power of 2, 1024..16384 */

typedef enum SdrFftWindow {
    SDR_FFT_WIN_RECT     = 0,
    SDR_FFT_WIN_HANN     = 1,
    SDR_FFT_WIN_HAMMING  = 2,
    SDR_FFT_WIN_BLACKMAN = 3,
} SdrFftWindow;
int32_t sdr_core_set_fft_window       (SdrCore*, SdrFftWindow w);
int32_t sdr_core_set_fft_rate         (SdrCore*, double fps);

/* ============================================================ */
/*  Events (engine -> host)                                     */
/* ============================================================ */

typedef enum SdrEventKind {
    SDR_EVT_SOURCE_STOPPED          = 1,
    SDR_EVT_SAMPLE_RATE_CHANGED     = 2,
    SDR_EVT_SIGNAL_LEVEL            = 3,
    SDR_EVT_DEVICE_INFO             = 4,
    SDR_EVT_GAIN_LIST               = 5,
    SDR_EVT_DISPLAY_BANDWIDTH       = 6,
    SDR_EVT_ERROR                   = 7,
    /* v2: recording, transcription, bookmarks, ... */
} SdrEventKind;

typedef struct SdrEvent {
    SdrEventKind kind;
    union {
        double    sample_rate_hz;       /* SAMPLE_RATE_CHANGED */
        float     signal_level_db;      /* SIGNAL_LEVEL */
        double    display_bandwidth_hz; /* DISPLAY_BANDWIDTH */
        struct {                        /* DEVICE_INFO */
            const char* utf8;           /* borrow valid for callback duration */
        } device_info;
        struct {                        /* GAIN_LIST */
            const double* values;       /* borrow valid for callback duration */
            size_t        len;
        } gain_list;
        struct {                        /* ERROR */
            const char* utf8;
        } error;
    } u;
} SdrEvent;

/* Callback signature. Called from the engine's dispatcher thread.
 * `user_data` is the same pointer the host passed to set_event_callback.
 * The callback MUST NOT call sdr_core_destroy. All other commands are safe.
 * Borrowed pointers in `evt` are valid only for the duration of this call. */
typedef void (*SdrEventCallback)(const SdrEvent* evt, void* user_data);

/* Register a callback. Replaces any previous callback. Pass NULL to clear.
 * Returns 0 on success. The dispatcher thread starts on first non-NULL set. */
int32_t sdr_core_set_event_callback(SdrCore*, SdrEventCallback cb, void* user_data);

/* ============================================================ */
/*  FFT frame pull (engine -> host, render-tick driven)         */
/* ============================================================ */

/* Borrow handed to the FFT frame callback. Pointer valid only for the
 * duration of the callback (Rust holds a mutex on the buffer for that
 * window). Do NOT retain. Copy out what you need. */
typedef struct SdrFftFrame {
    const float* magnitudes_db;   /* len = `len` */
    size_t       len;
    double       sample_rate_hz;  /* effective rate (for x-axis labelling) */
    double       center_freq_hz;
} SdrFftFrame;

typedef void (*SdrFftCallback)(const SdrFftFrame* frame, void* user_data);

/* Pulls the latest FFT frame, if a new one is ready since the last call.
 * Returns true and invokes `cb` if a frame was available. Returns false
 * (without calling `cb`) if no new frame. Lock-free fast path. */
bool sdr_core_pull_fft(SdrCore*, SdrFftCallback cb, void* user_data);

#ifdef __cplusplus
}
#endif
#endif /* SDR_CORE_H */
```

That's the entire v1 surface: **~25 command functions, 7 event variants, 1 FFT pull**.

## Threading Model

```text
                       ┌────────────────────────┐
                       │ Host main thread       │
                       │ (SwiftUI / GTK)        │
                       └─────────┬──────────────┘
                                 │
        sdr_core_set_*()         │           sdr_core_pull_fft()
        (any thread, lock-free) │           (called per render tick)
                                 ▼
                      ┌──────────────────────┐
                      │  sdr-ffi shim        │
                      │  (no own state)      │
                      └─────────┬────────────┘
                                │
                                │  mpsc::Sender<UiToDsp>     SharedFftBuffer
                                ▼
                      ┌────────────────────────────────┐
                      │  sdr-core::Engine              │
                      │  • DSP thread (writer)         │
                      │  • Dispatcher thread (events)  │
                      └────────────┬───────────────────┘
                                   │ DspToUi events
                                   ▼
                      ┌──────────────────────┐
                      │ Dispatcher thread    │
                      │ converts to SdrEvent │
                      │ calls SdrEventCallback│
                      └─────────┬────────────┘
                                │ (host's responsibility to marshal
                                ▼  to its own UI thread)
                      ┌──────────────────────┐
                      │ Host SdrEventCallback │
                      └──────────────────────┘
```

**Two background threads owned by `sdr-ffi`:**

1. **DSP thread** — already exists in `sdr-core`. Writes to `SharedFftBuffer`. Sends `DspToUi` events through the engine's mpsc channel.
2. **Dispatcher thread** — new in `sdr-ffi`. Owns the `mpsc::Receiver<DspToUi>` taken from `Engine::subscribe()`. Loops on `recv()`, converts each `DspToUi` into an `SdrEvent`, calls the registered C callback, drops borrowed buffers (Rust-owned strings/vecs are kept alive for the callback's stack frame). Joins on `sdr_core_destroy`.

The dispatcher thread is the only thread that calls into the host's event callback. The host *must not* assume which thread that is — it's not the host's main thread. SwiftUI code marshals to `MainActor` on receipt; GTK code marshals to the GLib main loop.

**FFT frames are pulled, not pushed.** The host calls `sdr_core_pull_fft` from its render tick (`MTKView`'s `draw(in:)` or `CVDisplayLink` for SwiftUI; `glib::timeout_add_local` at FFT rate for GTK). The pull is lock-free when no new frame is available, and acquires a short mutex when one is. This matches `SharedFftBuffer::take_if_ready` exactly. Zero allocations on the hot path.

**Reentrancy rules** (documented in the header):

| Function                       | Safe from event callback? |
|--------------------------------|---------------------------|
| `sdr_core_destroy`             | **No** (would deadlock on dispatcher join) |
| `sdr_core_set_event_callback`  | No                        |
| `sdr_core_pull_fft`            | Yes                       |
| All other `sdr_core_*` commands | Yes                      |

## Swift Package Wrapper Sketch

This is just enough to verify the C surface is usable from Swift. The full wrapper lives in `apps/macos/Packages/SdrCoreKit/` and is detailed in the surface spec.

```swift
// SdrCoreKit/Sources/SdrCoreKit/SdrCore.swift
import Foundation
import sdr_core_c   // SwiftPM systemModule wrapping include/sdr_core.h

@MainActor
public final class SdrCore {
    private let handle: OpaquePointer
    private var eventContinuation: AsyncStream<SdrCoreEvent>.Continuation?

    public let events: AsyncStream<SdrCoreEvent>

    public init(configPath: URL) throws {
        var raw: OpaquePointer? = nil
        let rc = configPath.path.withCString { sdr_core_create($0, &raw) }
        guard rc == 0, let raw else { throw SdrCoreError(rawValue: rc) ?? .internal }
        self.handle = raw

        var continuation: AsyncStream<SdrCoreEvent>.Continuation!
        self.events = AsyncStream { continuation = $0 }
        self.eventContinuation = continuation

        // Register a C trampoline that yields into the AsyncStream.
        let opaque = Unmanaged.passUnretained(self).toOpaque()
        sdr_core_set_event_callback(handle, { evt, user in
            guard let evt, let user else { return }
            let me = Unmanaged<SdrCore>.fromOpaque(user).takeUnretainedValue()
            me.eventContinuation?.yield(SdrCoreEvent(c: evt.pointee))
        }, opaque)
    }

    deinit {
        eventContinuation?.finish()
        sdr_core_destroy(handle)
    }

    public func start()                  throws { try check(sdr_core_start(handle)) }
    public func tune(_ hz: Double)       throws { try check(sdr_core_tune(handle, hz)) }
    public func setDemodMode(_ m: DemodMode) throws { try check(sdr_core_set_demod_mode(handle, m.cValue)) }
    // ...one wrapper per command function...

    /// Render-tick FFT pull. Calls `body` synchronously with a borrowed view.
    public func withLatestFftFrame(_ body: (UnsafeBufferPointer<Float>, Double, Double) -> Void) -> Bool {
        var captured = false
        let opaque = Unmanaged.passUnretained(BodyBox(body)).toOpaque()
        let got = sdr_core_pull_fft(handle, { frame, user in
            guard let frame, let user else { return }
            let box = Unmanaged<BodyBox>.fromOpaque(user).takeUnretainedValue()
            let buf = UnsafeBufferPointer(start: frame.pointee.magnitudes_db, count: frame.pointee.len)
            box.body(buf, frame.pointee.sample_rate_hz, frame.pointee.center_freq_hz)
        }, opaque)
        return got
    }
}
```

The Swift wrapper turns:
- The C callback into an `AsyncStream<SdrCoreEvent>` consumed by SwiftUI views with `for await`.
- The C pull into a closure-style API used inside Metal `draw(in:)`.
- Error codes into Swift `throws`.

Note that `BodyBox` is a temporary heap allocation per FFT frame. We accept this for v1 (one alloc/free per frame at 20 fps is negligible). If profiling shows it matters at high FFT rates, replace with a per-`SdrCore`-instance reusable box.

## ABI Versioning

Header declares `SDR_CORE_ABI_VERSION_MAJOR` and `_MINOR`. Rules:

- **Minor bump** = additive change only (new function, new event variant, new error code). Hosts built against an older minor still work; they just don't see the new things.
- **Major bump** = breaking change (signature change, removed function, struct layout change). Hosts built against an older major fail at `sdr_core_create` with `SDR_CORE_ERR_INTERNAL` and a clear error message.
- **`sdr_core_abi_version()` is the first call** the Swift wrapper makes. If it returns a major mismatch, the Swift `init` throws `SdrCoreError.abiVersionMismatch` and the app shows a "library mismatch — reinstall" dialog.

For v1 we are at `0.1`. We bump to `0.2` when we add transcription events, network/file source commands, etc.

## Build Integration

The Rust side produces `libsdr_core.a` (staticlib). Xcode is told about it via a SwiftPM `binaryTarget` *or* a build phase that runs `cargo build --release -p sdr-ffi --target <triple>` for both `aarch64-apple-darwin` and `x86_64-apple-darwin`, then `lipo`s them into a universal `libsdr_core.a`. Details in `2026-04-12-swift-ui-packaging-design.md`.

The header `include/sdr_core.h` is exposed as a SwiftPM systemModule:

```text
apps/macos/Packages/SdrCoreKit/
├── Package.swift
├── Sources/
│   ├── sdr_core_c/                 — systemModule, just module.modulemap + the .h
│   │   └── module.modulemap
│   └── SdrCoreKit/                 — Swift wrappers (SdrCore class, types, errors)
│       └── *.swift
└── Tests/
    └── SdrCoreKitTests/
```

## Test Strategy

- **Rust unit tests** in `sdr-ffi`: round-trip every command function via a mock `Engine` (the same `MockSource`-based tests `sdr-core` already uses). Verify error codes for invalid args, NULL handles, double-destroy.
- **Header drift CI lint:** `make ffi-header-check` runs `cbindgen` and diffs against `include/sdr_core.h`. Fails CI if they disagree on signatures (not on formatting).
- **Swift integration test** in `SdrCoreKitTests`: create an engine pointing at a test config, register an event callback, send `start`/`tune`/`stop`, assert events arrive on the stream.
- **Panic-safety test:** an internal test command that deliberately panics inside a `#[no_mangle]` function — must return `SDR_CORE_ERR_INTERNAL` with the panic message in `last_error_message`, not abort the process.

## Risks

| Risk | Mitigation |
|------|------------|
| Header and Rust drift silently | `make ffi-header-check` in CI; cbindgen used as a linter, not a generator |
| Panic across FFI = UB | Every `#[no_mangle]` function wrapped in `catch_unwind`. Test enforces it. |
| String borrow lifetime confusing for Swift devs | Wrapper copies all strings to `String` immediately on receipt. Header documents the rule. Tests would catch a use-after-free. |
| Dispatcher thread blocks if Swift callback is slow | Document the rule: callbacks must marshal-and-return. SwiftUI wrapper yields into an `AsyncStream` (~constant time). |
| Adding a command means editing 4 places (Rust enum, FFI fn, header, Swift wrapper) | Accepted cost of hand-rolled. Compensated by: small set, build-time errors at every layer, clean reviewable diffs. A code-gen approach would have its own 4-places-to-edit problem (IDL, generator output, Rust glue, Swift wrapper). |
| Swift `BodyBox` per-frame alloc shows up in profiler | Pre-allocate one box per `SdrCore` instance and reuse; trivial fix if needed |

## Open Questions

- **Pre-MVP spike:** validate that `cargo build --release -p sdr-ffi --target aarch64-apple-darwin` produces a static lib with no PipeWire symbols. (`sdr-sink-audio` must default-feature off `pipewire` so the macOS build doesn't try to link it.) — covered by the CoreAudio sink spec.
- **Should `sdr_core_create` accept a `JSON config blob` instead of a path?** Lean: no. Path is simpler, and the engine already knows how to read/write its own config. Host doesn't need to parse it.
- **Logging:** is `os_log` integration via a custom `tracing` subscriber the right call, or do we just emit JSON to stderr and let the `.app` capture it? Lean: `os_log` for v1, behind the `log_oslog` feature; can add JSON later if needed.

## References

- `include/sdr_core.h` (to be created in PR series M2)
- `crates/sdr-core/src/engine.rs` (created in M1)
- `crates/sdr-ui/src/messages.rs` — current message enums; FFI commands map 1:1 to these
- Swift docs: [Calling C APIs from Swift](https://developer.apple.com/documentation/swift/calling-apis-across-language-boundaries) — used by the wrapper
- `2026-04-12-sdr-core-extraction-design.md` — prerequisite
- `2026-04-12-swift-ui-packaging-design.md` — how the static lib gets into the .app
