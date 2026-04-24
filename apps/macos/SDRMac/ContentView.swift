//
// ContentView.swift — top-level layout with activity-bar
// sidebar redesign (epic #441, sub-ticket #442).
//
// Layout mirrors the GTK VS Code-style activity-bar pattern:
//
//   ┌─────────────────────────────────────────────────────────┐
//   │ [L] │ [L panel] │ spectrum + status + [old flyouts] │ [R panel] │ [R] │
//   └─────────────────────────────────────────────────────────┘
//      ↑       ↑                 ↑                      ↑        ↑
//    left   optional          detail column          optional  right
//   activity left panel       (existing content)     right     activity
//    bar      (per-selection)                        panel      bar
//
// Scaffolding phase (this ticket): all left/right panels are
// `ComingSoonPanel` placeholders pointing at the follow-up
// sub-tickets that port their real content (#443–#447, #448).
// The old flyouts (Transcription, Bookmarks) and the RR sheet
// stay toolbar-driven — #448 consolidates the right side,
// which may remove or relocate the existing flyouts.

import AppKit
import SwiftUI

struct ContentView: View {
    @Environment(CoreModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase

    /// Appearance override applied via `.preferredColorScheme(_:)`
    /// at the root. Read directly from `UserDefaults` (same key
    /// the Display panel writes) — `@AppStorage` here would set
    /// up two write paths and diverge if the Display picker and
    /// this binding ever fired in the same tick. Per #446.
    @AppStorage("SDRMac.appearance") private var rawAppearance: String = "system"

    // ----------------------------------------------------------
    //  Pre-redesign toolbar-driven surfaces — preserved as-is
    //  during scaffolding. #448 may relocate the right flyouts
    //  into the activity-bar-driven `rightSelection` panel.
    // ----------------------------------------------------------

    /// Sheet state lives up here (not inside the toolbar)
    /// because the subview-wrapped version of the RR button
    /// didn't render in the toolbar — inlining the button in
    /// the ToolbarItem closure did. Hoisting `@State` here
    /// keeps the toolbar closure flat while letting `.sheet`
    /// attach to a plain View that renders the dialog.
    @State private var showingRadioReference: Bool = false

    // ----------------------------------------------------------
    //  Activity-bar selections — new in #442
    //
    //  Ephemeral for scaffolding; session persistence wires
    //  these bindings into `sdr-config` via #449.
    // ----------------------------------------------------------

    /// Currently-selected left activity. Stays stable across
    /// panel open/close so the icon highlight persists when
    /// the user collapses the panel via a second click.
    /// `leftPanelOpen` controls visibility independently.
    /// This split mirrors the Linux
    /// `ui_sidebar_left_{selected,open}` config-key pair, so
    /// #449's session-persistence wires both bindings into
    /// the shared sdr-config JSON. Per `CodeRabbit` round 1
    /// on PR #491.
    @State private var leftSelection: LeftActivity = .general
    @State private var leftPanelOpen: Bool = true

    /// Same split for the right bar. Defaults: Transcript
    /// remembered as the active activity, panel starts closed
    /// — matches Linux startup.
    @State private var rightSelection: RightActivity = .transcript
    @State private var rightPanelOpen: Bool = false

    /// Ideal width of a left / right panel. `HSplitView` in
    /// #450 will let the user drag these; today they're fixed.
    private static let leftPanelWidth: CGFloat = 280
    private static let rightPanelWidth: CGFloat = 360

    var body: some View {
        HStack(spacing: 0) {
            // Left activity bar — always visible.
            ActivityBarView(
                selection: $leftSelection,
                isOpen: $leftPanelOpen,
                shortcutModifiers: .command
            )
            Divider()

            // Left panel — visible only when `leftPanelOpen`.
            // The remembered `leftSelection` stays put when
            // closed so a re-open snaps back to the same panel.
            // Placeholder bodies during scaffolding; real
            // panels land in #443–#447.
            if leftPanelOpen {
                LeftPanelHost(activity: leftSelection)
                    .frame(width: Self.leftPanelWidth)
                Divider()
            }

            // Detail column — spectrum + status bar. The
            // pre-redesign right-side flyouts (Transcription,
            // Bookmarks) moved to the right activity-bar
            // panels in #448, so the detail column is now
            // just the visualization stack.
            VStack(spacing: 0) {
                CenterView()
                StatusBar()
            }
            .frame(maxWidth: .infinity)

            // Right panel — driven by the right activity bar.
            // Hosts TranscriptionPanel or BookmarksPanel
            // depending on `rightSelection`; bookmarks gets
            // the `isOpen` binding so its X close button
            // toggles the activity bar's state directly.
            if rightPanelOpen {
                Divider()
                RightPanelHost(
                    activity: rightSelection,
                    isOpen: $rightPanelOpen
                )
                .frame(width: Self.rightPanelWidth)
            }
            Divider()

            // Right activity bar — always visible. One icon
            // in scaffolding; #448 adds the second.
            ActivityBarView(
                selection: $rightSelection,
                isOpen: $rightPanelOpen,
                shortcutModifiers: [.command, .shift]
            )
        }
        // Display panel's Appearance picker writes the same
        // UserDefaults key; this read drives the actual
        // window-wide override. Per #446.
        .preferredColorScheme((Appearance(rawValue: rawAppearance) ?? .system).colorScheme)
        .toolbar {
            HeaderToolbar(showingRadioReference: $showingRadioReference)
        }
        // Pre-redesign mutual exclusivity between the
        // Transcription and Bookmarks toolbar buttons, plus
        // the bookmarks UserDefaults persistence, both went
        // away in #448. The right activity bar's selection
        // is inherently single-valued (one panel open at a
        // time), and #449 will handle session persistence
        // for `rightSelection` + `rightPanelOpen` via the
        // shared sdr-config keys.
        .sheet(isPresented: $showingRadioReference) {
            RadioReferenceDialog()
        }
        // Re-sync the RadioReference credentials flag whenever
        // the main window becomes active. Handles the case where
        // something outside the app's Settings flow changed the
        // keychain (Keychain Access, another process, another
        // build of the app) — the next time the user focuses
        // this window, the toolbar reflects reality.
        //
        // The Settings save flow ALSO updates the flag directly,
        // so in the happy path this is a no-op double-check. If
        // cross-scene `@Observable` propagation ever drops an
        // update, scenePhase change acts as the safety net.
        //
        // Re-probe the USB bus for RTL-SDR hardware on refocus
        // as a safety-net fallback alongside the live IOKit
        // hotplug monitor wired in `CoreModel.bootstrap()`.
        // The monitor delivers plug/unplug events immediately
        // in the normal case (closed issue #363); this hook
        // catches edge cases where the monitor might miss a
        // transition (OS sleep/wake with a dongle swap,
        // notification port restarted underneath us). Cheap
        // enough to keep even if redundant.
        .onChange(of: scenePhase) { _, newPhase in
            if newPhase == .active {
                model.refreshRadioReferenceCredentialsFlag()
                model.refreshDeviceInfo()
            }
        }
        // Fatal ABI-mismatch modal. The binding's setter is a
        // no-op so dismissing the alert is impossible — the
        // only action is Quit. Matches the spec ("fail launch
        // with a dialog, since nothing else will work") in
        // `2026-04-12-swift-ui-surface-design.md`.
        .alert(
            "SDR engine version mismatch",
            isPresented: Binding(
                get: { model.abiMismatch != nil },
                set: { _ in }
            ),
            presenting: model.abiMismatch
        ) { _ in
            Button("Quit", role: .destructive) {
                NSApplication.shared.terminate(nil)
            }
        } message: { mismatch in
            Text("""
                This build of SDR was compiled against engine \
                ABI \(mismatch.compiled.major).\(mismatch.compiled.minor), \
                but the linked library reports \
                \(mismatch.runtime.major).\(mismatch.runtime.minor). \
                A major-version difference means the Swift side \
                and the Rust engine disagree on fundamental data \
                layouts; running anyway would crash or produce \
                bad output. Reinstall a matching build.
                """)
        }
    }
}

#Preview {
    ContentView()
        .environment(CoreModel())
}
