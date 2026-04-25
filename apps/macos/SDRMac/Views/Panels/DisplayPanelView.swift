//
// DisplayPanelView.swift — Display activity panel (closes #446).
//
// Four flat Sections matching the GTK
// `crates/sdr-ui/src/sidebar/display_panel.rs` layout:
//
//   - FFT          — size + window + frame rate
//   - Waterfall    — colormap (Mac wiring TBD; placeholder)
//   - Levels       — min/max dB + averaging
//   - Appearance   — System / Dark / Light theme
//
// Min/Max dB are cross-coupled — picking a min above the
// current max bumps max along, and vice versa, so the slider
// pair never settles into an inverted-range state where the
// spectrum collapses to a sliver.

import SwiftUI
import SdrCoreKit

struct DisplayPanelView: View {
    var body: some View {
        Form {
            FftSection()
            WaterfallSection()
            LevelsSection()
            AppearanceSection()
        }
        .formStyle(.grouped)
    }
}

// ============================================================
//  FFT — size, window, frame rate
// ============================================================

private struct FftSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            Picker("Size", selection: Binding(
                get: { model.fftSize },
                set: { model.setFftSize($0) }
            )) {
                ForEach([1024, 2048, 4096, 8192], id: \.self) {
                    Text("\($0)").tag($0)
                }
            }

            Picker("Window", selection: Binding(
                get: { model.fftWindow },
                set: { model.setFftWindow($0) }
            )) {
                ForEach(FftWindow.allCases, id: \.self) {
                    Text($0.label).tag($0)
                }
            }

            LabeledContent("Frame rate") {
                VStack(spacing: 2) {
                    @Bindable var m = model
                    Slider(
                        value: $m.fftRateFps,
                        in: 5...60,
                        step: 1,
                        onEditingChanged: { editing in
                            if !editing {
                                model.setFftRate(model.fftRateFps)
                            }
                        }
                    )
                    Text("\(Int(model.fftRateFps)) fps")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        } header: {
            Text("FFT")
        } footer: {
            Text("Frequency transform resolution and update rate.")
                .font(.caption)
        }
    }
}

// ============================================================
//  Waterfall — colormap (placeholder)
// ============================================================

private struct WaterfallSection: View {
    var body: some View {
        Section {
            Text("Waterfall colormap selection lives on the Linux side. Mac renderer ships a single fixed colormap today — picker arrives in a follow-up.")
                .font(.caption)
                .foregroundStyle(.secondary)
        } header: {
            Text("Waterfall")
        } footer: {
            Text("Color mapping for the scrolling history.")
                .font(.caption)
        }
    }
}

// ============================================================
//  Levels — min/max dB + averaging
// ============================================================

private struct LevelsSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            LabeledContent("Min dB") {
                VStack(spacing: 2) {
                    Slider(
                        value: Binding<Float>(
                            get: { model.minDb },
                            set: { setMin($0) }
                        ),
                        in: -150...0
                    )
                    Text("\(Int(model.minDb))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            LabeledContent("Max dB") {
                VStack(spacing: 2) {
                    Slider(
                        value: Binding<Float>(
                            get: { model.maxDb },
                            set: { setMax($0) }
                        ),
                        in: -150...0
                    )
                    Text("\(Int(model.maxDb))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            Picker("Averaging", selection: Binding(
                get: { model.averagingMode },
                set: { model.averagingMode = $0 }
            )) {
                ForEach(AveragingMode.allCases, id: \.self) {
                    Text($0.label).tag($0)
                }
            }
        } header: {
            Text("Levels")
        } footer: {
            Text("Signal range and averaging on the spectrum trace.")
                .font(.caption)
        }
    }

    /// Cross-coupled write: lowering min above current max
    /// drags max up by the same delta, keeping the range
    /// non-inverted. Without this, the spectrum view collapses
    /// to a sliver until the user manually fixes the order.
    private func setMin(_ newMin: Float) {
        model.setMinDb(newMin)
        if newMin > model.maxDb {
            model.setMaxDb(newMin)
        }
    }

    private func setMax(_ newMax: Float) {
        model.setMaxDb(newMax)
        if newMax < model.minDb {
            model.setMinDb(newMax)
        }
    }
}

// ============================================================
//  Appearance — System / Dark / Light theme
// ============================================================

private struct AppearanceSection: View {
    @AppStorage("SDRMac.appearance") private var rawAppearance: String = "system"

    var body: some View {
        Section {
            Picker("Theme", selection: Binding(
                get: { Appearance(rawValue: rawAppearance) ?? .system },
                set: { rawAppearance = $0.rawValue }
            )) {
                ForEach(Appearance.allCases, id: \.self) { a in
                    Text(a.label).tag(a)
                }
            }
            .pickerStyle(.segmented)
        } header: {
            Text("Appearance")
        } footer: {
            Text("Override the system color scheme for this app.")
                .font(.caption)
        }
    }
}

/// Color-scheme override applied via `.preferredColorScheme(_:)`
/// at the window root in `ContentView`. Stored as a string in
/// `UserDefaults` so a future settings reset can clear it without
/// touching multiple keys. Per #446.
enum Appearance: String, CaseIterable, Hashable {
    case system, light, dark

    var label: String {
        switch self {
        case .system: return "System"
        case .light:  return "Light"
        case .dark:   return "Dark"
        }
    }

    var colorScheme: ColorScheme? {
        switch self {
        case .system: return nil
        case .light:  return .light
        case .dark:   return .dark
        }
    }
}
