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
    @Binding var showingRadioReference: Bool

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

        // Transcription + Bookmarks toolbar buttons removed
        // in #448 — those panels migrated to the right activity
        // bar (`⌘⇧1` for Transcript, `⌘⇧2` for Bookmarks).
        // Click the matching activity-bar icon to open or close
        // each panel.

        // RadioReference button — mirrors the GTK header-bar
        // entry point.
        //
        // Always visible (not gated on saved credentials) for
        // two reasons:
        //   1. SwiftUI's macOS toolbar didn't re-lay out
        //      reliably when we gated on
        //      `model.radioReferenceHasCredentials` — the
        //      button stayed hidden even after credentials
        //      were saved. An always-present item sidesteps
        //      the layout-caching quirk entirely.
        //   2. The dialog already handles the no-credentials
        //      case with a "configure in Settings → RadioReference"
        //      message, so clicking the button is always
        //      actionable — either search or guidance to
        //      set up auth.
        //
        // **Inline** — no `RadioReferenceToolbarButton`
        // subview wrapper. During debugging (v4/v5), wrapping
        // the button in a separate View struct caused
        // ToolbarItem not to render on macOS; inlining the
        // Button + Label directly in the ToolbarItem closure
        // works reliably. Sheet presentation state lives on
        // ContentView so this ToolbarContent struct doesn't
        // need its own `@State`.
        //
        // **`Label(text, systemImage:)`** — not a bare
        // `Image`. macOS toolbars have a user-controlled
        // display mode (Icon Only / Icon and Text / Text
        // Only via right-click). A bare `Image` whose symbol
        // isn't recognized on the current macOS version
        // renders nothing in Icon Only mode. The `Label`
        // falls back to text so the button surfaces
        // regardless. Per PR #346 debugging and the
        // `feedback_swiftui_toolbar_placement` memory.
        ToolbarItem(placement: .automatic) {
            Button {
                showingRadioReference = true
            } label: {
                Label(
                    "RadioReference",
                    systemImage: "antenna.radiowaves.left.and.right"
                )
            }
            .help("RadioReference Frequency Browser")
        }
    }
}

// The big tuner display lives in `FrequencyDigitsEntry` — 12
// individual digits with click/scroll/keyboard per digit,
// matching the GTK widget. The old `FrequencyEntry` text-field
// approach was removed in favor of the digit grid.

