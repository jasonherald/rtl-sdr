//
// HeaderToolbar.swift — main window toolbar.
//
// Play/stop, center frequency (primary placement), demod picker.
// Uses `@Bindable(model)` to expose two-way bindings into views
// that want `$foo` syntax (the `@Observable` equivalent of
// `$model.foo` on `ObservableObject`).

import SwiftUI
import SdrCoreKit

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
            .help(model.isRunning ? "Stop (⌘R)" : "Start (⌘R)")
        }

        ToolbarItem(placement: .principal) {
            @Bindable var m = model
            FrequencyEntry(hz: $m.centerFrequencyHz) { hz in
                model.setCenter(hz)
            }
            .frame(width: 220)
        }

        ToolbarItem(placement: .primaryAction) {
            @Bindable var m = model
            Picker("Mode", selection: $m.demodMode) {
                ForEach(DemodMode.allCases, id: \.self) {
                    Text($0.label).tag($0)
                }
            }
            .pickerStyle(.menu)
            .frame(width: 110)
            .onChange(of: model.demodMode) { _, new in
                model.setDemodMode(new)
            }
        }
    }
}

/// Simple frequency text field. Accepts plain Hz, or MHz/kHz
/// suffixes ("100.7M", "446k"). Commits via `onSubmit` and
/// reverts on parse failure.
///
/// v1: no per-digit arrow-key step, no tab between digit groups.
/// Good enough for the first milestone — the polished entry
/// widget (per-digit steppers, scroll-to-tune) is a v2 follow-up.
struct FrequencyEntry: View {
    @Binding var hz: Double
    var commit: (Double) -> Void

    @State private var text: String = ""
    @FocusState private var focused: Bool

    var body: some View {
        TextField("Hz", text: $text)
            .textFieldStyle(.roundedBorder)
            .font(.system(.title3, design: .monospaced))
            .multilineTextAlignment(.center)
            .focused($focused)
            .onAppear { text = formatHz(hz) }
            .onChange(of: hz) { _, new in
                if !focused { text = formatHz(new) }
            }
            .onSubmit {
                if let v = parseHz(text) {
                    hz = v
                    commit(v)
                    text = formatHz(v)
                } else {
                    text = formatHz(hz)
                }
            }
    }
}

private func formatHz(_ hz: Double) -> String {
    let mhz = hz / 1_000_000
    return String(format: "%.4f MHz", mhz)
}

private func parseHz(_ s: String) -> Double? {
    let trimmed = s.trimmingCharacters(in: .whitespaces).lowercased()
    let multipliers: [(String, Double)] = [
        ("ghz", 1_000_000_000), ("g", 1_000_000_000),
        ("mhz", 1_000_000),     ("m", 1_000_000),
        ("khz", 1_000),         ("k", 1_000),
        ("hz", 1),
    ]
    for (suffix, mult) in multipliers where trimmed.hasSuffix(suffix) {
        let body = trimmed.dropLast(suffix.count).trimmingCharacters(in: .whitespaces)
        if let v = Double(body) { return v * mult }
    }
    return Double(trimmed)
}
