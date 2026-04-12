---
name: SwiftUI App Surface — Design
description: SwiftUI app shell, screen-by-screen mapping from the GTK panels, observable model layered on SdrCoreKit, MVP cut and v2 deferrals
type: spec
---

# SwiftUI App Surface — Design

**Status:** Draft
**Date:** 2026-04-12
**Parent epic:** `2026-04-12-swift-ui-macos-epic-design.md`
**Depends on:** `2026-04-12-sdr-ffi-c-abi-design.md`, `2026-04-12-swift-ui-rendering-design.md`
**Tracking issues:** TBD

---

## Goal

Define the SwiftUI app shell, the observable model that bridges `SdrCoreKit` to views, and the screen-by-screen mapping from the existing GTK panels to native macOS controls. This is the spec for everything you can *see* in the macOS app — windowing, sidebar, toolbars, settings, menus — and how those views talk to the engine.

## Non-Goals

- **Exact pixel placement.** Auto layout via SwiftUI defaults; tweaking comes after the first build runs.
- **Theming / dark-mode customization.** SwiftUI gets system appearance for free; we respect it.
- **Localization.** English-only in v1. Strings live in `Localizable.strings` so we can localize later without source edits.
- **Accessibility audit beyond defaults.** SwiftUI controls are accessible by default; a real audit (VoiceOver, keyboard nav for waterfall) is v2.
- **Animation polish.** SwiftUI default transitions are fine for v1.

## Background

The GTK app's user-facing surface (from the project review):

- **AdwHeaderBar:** big frequency display, play/stop transport, demod selector, screenshot button
- **AdwFlap sidebar (collapsible)** with accordion panels:
  - Source Panel: device picker, sample rate, gain, AGC, PPM, network/file config, decimation, DC blocking, IQ inversion/correction, IQ recording
  - Radio Panel: bandwidth, squelch (level/auto), de-emphasis, noise blanker, FM IF NR, WFM stereo, notch
  - Display Panel: FFT size, averaging mode, min/max dB, FPS
  - Navigation/Bookmarks: frequency bookmarks + RadioReference
  - Transcript: speech-to-text display, model selection
- **Center pane (GtkPaned split):** spectrum (top 30%) + waterfall (bottom 70%)
- **Status bar:** SNR, sample rate, buffer metrics

The MVP cut (from the epic doc) takes the **Source**, **Radio**, **Display** panels and the spectrum/waterfall — and ignores everything else for v1. v2 adds Bookmarks, Transcript, Network/File sources, recording, and the rest.

## App Shell Structure

```text
SDRMacApp                      (struct App)
├── WindowGroup                 — main window
│   └── ContentView             — top-level NavigationSplitView
│       ├── Sidebar             — sidebar panels (collapsible)
│       │   ├── SourceSection
│       │   ├── RadioSection
│       │   └── DisplaySection
│       └── DetailColumn        — spectrum + waterfall + status
│           ├── HeaderToolbar   — big frequency, transport, demod picker
│           ├── SpectrumWaterfallView (NSViewRepresentable from rendering spec)
│           └── StatusBar
└── Settings                    — settings scene (Cmd-,)
    └── SettingsView
        ├── GeneralPane         — config file location, log level
        ├── AudioPane           — output device, latency target
        └── AdvancedPane        — debug overlays, ffi version display
```

```swift
@main
struct SDRMacApp: App {
    @State private var core = CoreModel()           // observable, owns SdrCore

    var body: some Scene {
        WindowGroup("SDR") {
            ContentView()
                .environment(core)
                .frame(minWidth: 900, minHeight: 600)
        }
        .windowToolbarStyle(.unified)
        .commands { SDRCommands(core: core) }       // menu bar items

        Settings {
            SettingsView()
                .environment(core)
        }
    }
}
```

`NavigationSplitView` is the macOS-native sidebar pattern. Sidebar collapses into a toolbar button via standard SwiftUI behavior — no custom AdwFlap analogue needed.

`@Observable` (macOS 14+, fully cooked on macOS 26) is the model framework. One root model, passed via `.environment()`, observed by views automatically. No `@StateObject` / `ObservableObject` boilerplate.

## The Observable Model

`CoreModel` is the single source of truth for everything the UI displays. It owns the `SdrCore` instance, exposes typed bindings to views, and consumes the event stream on `MainActor`.

```swift
// CoreModel.swift
import Observation
import SdrCoreKit

@MainActor
@Observable
final class CoreModel {
    // Engine handle (constructed on first use)
    private var core: SdrCore?

    // Lifecycle state
    var isRunning: Bool = false
    var lastError: String? = nil

    // Tuning
    var centerFrequencyHz: Double = 100_700_000
    var vfoOffsetHz: Double = 0
    var sampleRateHz: Double = 2_000_000          // raw source rate (display bandwidth)
    var effectiveSampleRateHz: Double = 250_000   // post-decimation
    var ppmCorrection: Int = 0

    // Tuner
    var availableGains: [Double] = []
    var gainDb: Double = 0
    var agcEnabled: Bool = false
    var deviceInfo: String = ""

    // Demod
    var demodMode: DemodMode = .wfm
    var bandwidthHz: Double = 200_000
    var squelchEnabled: Bool = false
    var squelchDb: Float = -60
    var deemphasis: Deemphasis = .us75

    // Audio
    var volume: Float = 0.5

    // Display
    var fftSize: Int = 2048
    var fftWindow: FftWindow = .blackman
    var fftRateFps: Double = 20
    var minDb: Float = -100
    var maxDb: Float = 0

    // Status
    var signalLevelDb: Float = -120

    // Setup — called once on app launch
    func bootstrap(configPath: URL) async {
        do {
            let c = try SdrCore(configPath: configPath)
            self.core = c
            Task { await consumeEvents(from: c.events) }
        } catch {
            self.lastError = "Failed to start engine: \(error)"
        }
    }

    private func consumeEvents(from stream: AsyncStream<SdrCoreEvent>) async {
        for await event in stream {
            switch event {
            case .sourceStopped:
                isRunning = false
            case .sampleRateChanged(let hz):
                effectiveSampleRateHz = hz
            case .signalLevel(let db):
                signalLevelDb = db
            case .deviceInfo(let s):
                deviceInfo = s
            case .gainList(let gains):
                availableGains = gains
            case .displayBandwidth(let hz):
                sampleRateHz = hz
            case .error(let msg):
                lastError = msg
            }
        }
    }

    // Command dispatch
    // ---------------------------------------------------------------
    // Two patterns:
    //
    // 1. **Lifecycle** (start/stop) — *strict*: only mutate UI state if the
    //    engine accepts the command. A failed start() must NOT leave
    //    isRunning=true. Errors land in `lastError`.
    //
    // 2. **Setters** (frequency/gain/squelch/etc.) — *optimistic*: mutate
    //    the UI binding immediately for input responsiveness, then forward
    //    to the engine. The engine processes commands asynchronously over
    //    its mpsc channel, so a successful return only means "queued", not
    //    "applied". Real engine-side rejections come back via the event
    //    stream and surface as state corrections in `consumeEvents`.
    //    Synchronous FFI failures (e.g., engine handle dropped) still
    //    capture into `lastError`.
    //
    // Neither pattern uses `try?`. Errors are never silently dropped.

    private func capture(_ work: () throws -> Void) {
        do { try work() }
        catch { lastError = "\(error)" }
    }

    // --- Strict / lifecycle ---
    func start() {
        guard let core else { lastError = "engine not initialized"; return }
        do {
            try core.start()
            isRunning = true
            lastError = nil
        } catch {
            lastError = "start failed: \(error)"
        }
    }
    func stop() {
        guard let core else { return }
        do {
            try core.stop()
            isRunning = false
        } catch {
            lastError = "stop failed: \(error)"
        }
    }

    // --- Optimistic setters ---
    func setCenter(_ hz: Double)       { centerFrequencyHz = hz; capture { try core?.tune(hz) } }
    func setVfoOffset(_ hz: Double)    { vfoOffsetHz = hz;       capture { try core?.setVfoOffset(hz) } }
    func setDemodMode(_ m: DemodMode)  { demodMode = m;          capture { try core?.setDemodMode(m) } }
    func setBandwidth(_ hz: Double)    { bandwidthHz = hz;       capture { try core?.setBandwidth(hz) } }
    func setGain(_ db: Double)         { gainDb = db;            capture { try core?.setGain(db) } }
    func setAgc(_ on: Bool)            { agcEnabled = on;        capture { try core?.setAgc(on) } }
    func setSquelch(_ db: Float)       { squelchDb = db;         capture { try core?.setSquelchDb(db) } }
    func setSquelchEnabled(_ on: Bool) { squelchEnabled = on;    capture { try core?.setSquelchEnabled(on) } }
    func setDeemphasis(_ m: Deemphasis){ deemphasis = m;         capture { try core?.setDeemphasis(m) } }
    func setVolume(_ v: Float)         { volume = v;             capture { try core?.setVolume(v) } }
    func setMinDb(_ db: Float)         { minDb = db }     // pure UI, no engine call
    func setMaxDb(_ db: Float)         { maxDb = db }     // pure UI, no engine call
    func setFftSize(_ n: Int)          { fftSize = n;             capture { try core?.setFftSize(n) } }
    func setFftWindow(_ w: FftWindow)  { fftWindow = w;           capture { try core?.setFftWindow(w) } }
    func setFftRate(_ fps: Double)     { fftRateFps = fps;        capture { try core?.setFftRate(fps) } }
    func setPpm(_ ppm: Int)            { ppmCorrection = ppm;     capture { try core?.setPpmCorrection(ppm) } }

    // Renderer pulls FFT through this — see rendering spec
    var sdrCore: SdrCore? { core }
}
```

**Why `@Observable` and not `ObservableObject`:** `@Observable` (Observation framework) tracks dependencies at the property-access level instead of the object level. A view that reads `model.frequencyHz` only re-renders when *that* property changes — not when any other property on the model changes. For a UI driven by 30 Hz event streams across many independent properties, this is a meaningful win and removes most of the `@Published`/`objectWillChange` ceremony.

**Why one big model and not many smaller ones:** initial instinct is to split into `TunerModel`, `DemodModel`, `DisplayModel`, etc. Rejected for v1 because all of those touch the same engine handle and share lifecycle, and the Observation framework already gives us per-property granularity. We can split if it grows uncomfortable.

## Screen-by-Screen

### Header Toolbar

```swift
struct HeaderToolbar: ToolbarContent {
    @Environment(CoreModel.self) private var model

    var body: some ToolbarContent {
        ToolbarItem(placement: .navigation) {
            Button {
                model.isRunning ? model.stop() : model.start()
            } label: {
                Image(systemName: model.isRunning ? "stop.fill" : "play.fill")
            }
            .keyboardShortcut("r", modifiers: .command)
        }
        ToolbarItem(placement: .principal) {
            FrequencyEntry(hz: Bindable(model).centerFrequencyHz)
                .font(.system(.title, design: .monospaced))
                .frame(width: 240)
        }
        ToolbarItem(placement: .primaryAction) {
            Picker("Mode", selection: Bindable(model).demodMode) {
                ForEach(DemodMode.allCases, id: \.self) { Text($0.label).tag($0) }
            }
            .pickerStyle(.menu)
            .frame(width: 110)
        }
    }
}
```

`FrequencyEntry` is a custom view: monospace text field with arrow-key step (configurable: 1 Hz / 100 Hz / 1 kHz / 10 kHz / 100 kHz / 1 MHz), rejects non-numeric input, accepts MHz/kHz suffixes ("100.7M"). Tab moves between digit groups.

### Source Section (sidebar)

MVP scope: RTL-SDR only. The whole "device picker" comes back in v2.

```swift
struct SourceSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("Source") {
            LabeledContent("Device") { Text(model.deviceInfo).foregroundStyle(.secondary) }
            LabeledContent("Sample rate") {
                Picker("", selection: Bindable(model).sampleRateHz) {
                    ForEach(rtlSdrSampleRates, id: \.self) { Text(formatRate($0)).tag($0) }
                }
                .labelsHidden()
            }
            LabeledContent("Gain") {
                if model.agcEnabled {
                    Text("AGC").foregroundStyle(.secondary)
                } else {
                    Slider(
                        value: Bindable(model).gainDb,
                        in: gainRange(from: model.availableGains),
                        step: 1,
                        onEditingChanged: { _ in model.setGain(model.gainDb) }
                    )
                }
            }
            Toggle("AGC", isOn: Bindable(model).agcEnabled)
            LabeledContent("PPM") {
                Stepper(value: Bindable(model).ppmCorrection, in: -100...100) {
                    Text("\(model.ppmCorrection)")
                }
            }
        }
    }
}
```

Native `Slider`, `Stepper`, `Picker` — zero custom drawing. Bindings come from `Bindable(model).property` (the `@Observable` equivalent of `$model.property`).

### Radio Section (sidebar)

```swift
struct RadioSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("Radio") {
            LabeledContent("Bandwidth") {
                BandwidthEntry(hz: Bindable(model).bandwidthHz, mode: model.demodMode)
            }
            Toggle("Squelch", isOn: Bindable(model).squelchEnabled)
            if model.squelchEnabled {
                LabeledContent("Threshold") {
                    Slider(value: Bindable(model).squelchDb, in: -120...0)
                    Text("\(Int(model.squelchDb)) dB")
                }
            }
            if model.demodMode == .wfm || model.demodMode == .nfm {
                Picker("De-emphasis", selection: Bindable(model).deemphasis) {
                    Text("None").tag(Deemphasis.none)
                    Text("US 75µs").tag(Deemphasis.us75)
                    Text("EU 50µs").tag(Deemphasis.eu50)
                }
                .pickerStyle(.segmented)
            }
            LabeledContent("Volume") {
                Slider(value: Bindable(model).volume, in: 0...1)
            }
        }
    }
}
```

`BandwidthEntry` is another custom view that knows the sensible range for each demod mode (NFM 12.5 kHz default, WFM 200 kHz, AM 6 kHz, etc.) and offers presets in a pop-up.

### Display Section (sidebar)

```swift
struct DisplaySection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("Display") {
            Picker("FFT Size", selection: Bindable(model).fftSize) {
                ForEach([1024, 2048, 4096, 8192], id: \.self) { Text("\($0)").tag($0) }
            }
            Picker("Window", selection: Bindable(model).fftWindow) {
                Text("Rectangular").tag(FftWindow.rect)
                Text("Hann").tag(FftWindow.hann)
                Text("Hamming").tag(FftWindow.hamming)
                Text("Blackman").tag(FftWindow.blackman)
            }
            LabeledContent("Min dB") {
                Slider(value: Bindable(model).minDb, in: -150...0)
            }
            LabeledContent("Max dB") {
                Slider(value: Bindable(model).maxDb, in: -150...0)
            }
        }
    }
}
```

### Center: Spectrum + Waterfall

The Metal view from the rendering spec, in a `ZStack` with a SwiftUI overlay for the frequency scale and dB grid:

```swift
struct CenterView: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        ZStack(alignment: .topLeading) {
            SpectrumWaterfallView(
                core: model.sdrCore,
                centerHz: model.centerFrequencyHz,
                bandwidthHz: model.sampleRateHz,
                vfoOffsetHz: Bindable(model).vfoOffsetHz,
                vfoBandwidthHz: Bindable(model).bandwidthHz,
                minDb: Bindable(model).minDb,
                maxDb: Bindable(model).maxDb
            )
            FrequencyScaleOverlay(
                centerHz: model.centerFrequencyHz,
                spanHz: model.sampleRateHz
            )
            .allowsHitTesting(false)
        }
    }
}
```

The Metal view forwards click-to-tune via the `vfoOffsetHz` binding. The model's `setVfoOffset` is what fires the engine command — the binding setter calls it.

### Status Bar

```swift
struct StatusBar: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        HStack(spacing: 16) {
            Label("\(Int(model.signalLevelDb)) dB", systemImage: "waveform")
            Label(formatRate(model.effectiveSampleRateHz), systemImage: "metronome")
            Spacer()
            if let err = model.lastError {
                Label(err, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.red)
            }
        }
        .font(.caption)
        .padding(.horizontal, 12)
        .frame(height: 22)
        .background(.bar)
    }
}
```

### Settings Window

Standard `Settings { ... }` scene; macOS gives us the Cmd-, shortcut and the proper window chrome for free.

- **General:** config file path display (read from `~/Library/Application Support/SDRMac/config.json`), "Show in Finder" button, log level dropdown (writes through `sdr_core_init_logging`).
- **Audio:** output device picker (v2 — placeholder in v1 saying "Default device" disabled), volume.
- **Advanced:** ABI version (`sdr_core_abi_version()` formatted), Rust core version, debug overlays toggle.

### Menu Bar (`.commands`)

```swift
struct SDRCommands: Commands {
    let core: CoreModel

    var body: some Commands {
        CommandGroup(replacing: .newItem) { }   // hide File > New (no doc model)
        CommandMenu("Radio") {
            Button("Start") { core.start() }.keyboardShortcut("r", modifiers: .command)
            Button("Stop")  { core.stop() }.keyboardShortcut(".", modifiers: .command)
            Divider()
            Button("Tune Up 100 kHz")   { core.setCenter(core.centerFrequencyHz + 100_000) }.keyboardShortcut(.upArrow, modifiers: [.command])
            Button("Tune Down 100 kHz") { core.setCenter(core.centerFrequencyHz - 100_000) }.keyboardShortcut(.downArrow, modifiers: [.command])
        }
        CommandMenu("View") {
            Button("Toggle Sidebar") { /* SwiftUI native */ }.keyboardShortcut("s", modifiers: [.command, .control])
        }
    }
}
```

## Configuration & Persistence

`sdr-config` already handles JSON read/write. The macOS app passes its config path on `sdr_core_create`:

```swift
let configDir = FileManager.default
    .url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
    .appendingPathComponent("SDRMac")
let configPath = configDir.appendingPathComponent("config.json")
try FileManager.default.createDirectory(at: configDir, withIntermediateDirectories: true)
try await model.bootstrap(configPath: configPath)
```

The engine reads on startup, writes on shutdown. SwiftUI never touches the JSON file directly.

There's one wrinkle: SwiftUI-only state (like `splitFraction` for the spectrum/waterfall divider, sidebar collapsed state, window size) isn't engine state and shouldn't be in the JSON. We use `@AppStorage` for those — they persist into the standard macOS user defaults.

```swift
@AppStorage("spectrumSplitFraction") private var splitFraction: Double = 0.3
@AppStorage("sidebarVisible")        private var sidebarVisible: Bool = true
```

## Lifecycle & Threading

```text
App launch
  ├─ SDRMacApp.init                     (creates @State CoreModel, no engine yet)
  ├─ ContentView.task                   (calls model.bootstrap → creates SdrCore)
  │   └─ SdrCore.init                   (calls sdr_core_create, registers callback)
  │   └─ Task { consumeEvents }         (long-running for-await on AsyncStream)
  └─ Windows render
       └─ User clicks Play
            └─ model.start()
                 └─ sdr_core_start
                      └─ DSP thread spins up

App quit (Cmd-Q)
  └─ Window closes
       └─ CoreModel.deinit              (not guaranteed for @State; see below)
            └─ SdrCore.deinit
                 └─ sdr_core_destroy    (joins DSP + dispatcher threads, writes config)
```

`@State`-owned models don't get deterministic deinit on app termination. To guarantee config persistence on Cmd-Q, we hook `applicationWillTerminate` via an `NSApplicationDelegateAdaptor` and call `model.shutdown()` explicitly there. Standard SwiftUI macOS pattern.

```swift
final class AppDelegate: NSObject, NSApplicationDelegate {
    var model: CoreModel?
    func applicationWillTerminate(_ notification: Notification) {
        model?.shutdown()       // sync; blocks until DSP joined
    }
}
```

## v2 / Backlog Surface (excluded from MVP)

These views exist as stubs in the file tree but are commented out / hidden behind feature flags:

- **Source Picker** (RTL-SDR / Network / File)
- **Network Source** form (host/port/protocol)
- **File Source** picker
- **Bookmarks Section** (sidebar) — list, add/edit, group by tag
- **RadioReference Section** — county lookup, frequency table, "Tune" buttons
- **Transcript Section** — scrolling text view, model picker (blocked on sherpa-onnx work)
- **Recording Section** — IQ + audio recording controls, file path, level meter
- **Display Section extras** — averaging mode, FPS slider
- **Audio Device Picker** (Settings → Audio)
- **Noise Blanker / FM IF NR / WFM Stereo / Notch** controls (Radio Section advanced)
- **DC Blocking / IQ Inversion / IQ Correction** toggles (Source Section advanced)

Each one is its own GitHub issue (tracked in the epic) with its own short design note when picked up.

## Test Strategy

- **`@Observable` model unit tests** (`SDRMacTests`): construct `CoreModel` with a mock `SdrCore` (Swift protocol over the same surface), drive it with synthetic events, assert published properties update correctly.
- **Snapshot tests** of each panel using `swift-snapshot-testing` against fixed model state. Catches accidental layout regressions.
- **Manual smoke checklist** before merging M5: launch app → tune FM 100.7 → audio plays → drag VFO → audio retunes → quit → relaunch and verify last frequency restored.
- **No SwiftUI preview-only tests** — previews are dev-time only, not assertions.

## Risks

| Risk | Mitigation |
|------|------------|
| `@Observable` granularity surprises (a view re-renders on a property it didn't read) | Verified by `_printChanges()` in previews during development. Per-property observation is well-tested in macOS 14+; macOS 26 gets the latest fixes. |
| `NavigationSplitView` sidebar doesn't collapse to a button on narrow windows the way `AdwFlap` does | macOS native behavior since macOS 13. Verified in spike. If missing, fall back to a toggle button in the toolbar that drives sidebar visibility. |
| `@AppStorage` and engine config drift apart (user changes a setting via UI but the engine's JSON has a stale value) | Engine state is *only* in engine JSON; UI-only state is *only* in `@AppStorage`. There is no overlap. Reviewer enforces this. |
| Frequency text entry feels stiff because every keystroke fires a tune command | Debounce via SwiftUI `.onChange(of:) { ... }` with a `Task.sleep(milliseconds: 100)` cancellation pattern. Already a common idiom. |
| The Settings window throws because `core` isn't bootstrapped yet | Settings reads `model.core` lazily; if nil, panes show placeholders. Bootstrap is sync-fast (no engine work, just `sdr_core_create`). |
| Click-to-tune on Metal view doesn't update SwiftUI binding cleanly across the `NSViewRepresentable` boundary | Metal view stores a closure passed via `Coordinator`. The closure calls `model.setVfoOffset(...)` directly on `MainActor`. No binding round-trip needed. |

## Open Questions

- **Should we surface `availableGains` as discrete steps** (RTL-SDR exposes ~30 specific dB values, not a continuous range) **or as a continuous slider that snaps to nearest?** Lean: **discrete steps** via a custom `Slider`-replacement. The GTK UI does the same. Easier to communicate "you're at 33.8 dB" than "33.7-ish".
- **Should we fail the app launch** if `SDR_CORE_ABI_VERSION_MAJOR` mismatches the bundled lib, or just show an error banner? Lean: **fail launch with a dialog**, since nothing else will work. Includes a "Show in Finder" button pointing at the lib.
- **Should the FM/AM/SSB demod picker live in the toolbar (where I have it) or in the sidebar?** Lean: **toolbar**. It's the most-changed setting; the GTK UI has it in the header for the same reason.
- **Big Sur-style "tab view" Settings vs. macOS 13+ "form" Settings?** Lean: **Form-style (`Form { ... }` with sections)**, the modern default.

## Implementation Sequencing

This is M5 in the epic. Internal sub-PRs:

1. **App skeleton + window + empty `CoreModel`** — `cargo build` produces the static lib, Xcode links it, SwiftPM wraps it, the app launches showing just a placeholder view. No engine commands wired yet.
2. **Header toolbar + frequency entry + start/stop**, model wired to a real `SdrCore`.
3. **Source/Radio/Display sidebar sections**, all bindings wired.
4. **Center `SpectrumWaterfallView`** (consumes the M4 renderer).
5. **Status bar + Settings scene + menu commands**.
6. **Quit handling, config persistence, ABI version check**.

After (6), the MVP is feature-complete and ready for M6 (signing/notarization/CI).

## References

- `2026-04-12-swift-ui-rendering-design.md` — the Metal view this surface embeds
- `2026-04-12-sdr-ffi-c-abi-design.md` — the C surface that `SdrCoreKit` wraps
- `crates/sdr-ui/src/window.rs` — GTK reference for layout, behavior, defaults
- `crates/sdr-ui/src/sidebar/source_panel.rs` (and `radio_panel.rs`, `display_panel.rs`) — exact controls and ranges to mirror
- [Apple — Observation framework](https://developer.apple.com/documentation/observation)
- [Apple — `NavigationSplitView`](https://developer.apple.com/documentation/swiftui/navigationsplitview)
