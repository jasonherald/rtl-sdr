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
            // Route through the model setter (not `$m.demodMode`
            // directly) so the binding's set path fires
            // `setDemodMode` exactly once — the straightforward
            // two-way binding would write the property first, then
            // the onChange handler would write it again via the
            // setter, which both worked and smelled.
            Picker("Mode", selection: Binding(
                get: { model.demodMode },
                set: { model.setDemodMode($0) }
            )) {
                ForEach(DemodMode.allCases, id: \.self) {
                    Text($0.label).tag($0)
                }
            }
            .pickerStyle(.menu)
            .frame(width: 110)
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
    var trimmed = s.trimmingCharacters(in: .whitespaces).lowercased()
    // Reject negative frequencies up front — the engine would
    // reject them too, but we can give faster feedback here
    // (FrequencyEntry's onSubmit reverts to the last good value
    // on nil) rather than letting a doomed tune command flow all
    // the way to the DSP thread. Accept a leading `+` as a
    // no-op so "+100M" isn't surprising.
    if trimmed.hasPrefix("-") { return nil }
    if trimmed.hasPrefix("+") { trimmed = String(trimmed.dropFirst()) }
    let multipliers: [(String, Double)] = [
        ("ghz", 1_000_000_000), ("g", 1_000_000_000),
        ("mhz", 1_000_000),     ("m", 1_000_000),
        ("khz", 1_000),         ("k", 1_000),
        ("hz", 1),
    ]
    for (suffix, mult) in multipliers where trimmed.hasSuffix(suffix) {
        let body = trimmed.dropLast(suffix.count).trimmingCharacters(in: .whitespaces)
        if let v = Double(body), v >= 0 { return v * mult }
    }
    guard let v = Double(trimmed), v >= 0 else { return nil }
    return v
}
