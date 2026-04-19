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
            FrequencyDigitsEntry(hz: $m.centerFrequencyHz) { hz in
                model.setCenter(hz)
            }
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

// The big tuner display lives in `FrequencyDigitsEntry` — 12
// individual digits with click/scroll/keyboard per digit,
// matching the GTK widget. The old `FrequencyEntry` text-field
// approach was removed in favor of the digit grid.
