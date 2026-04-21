//
// ContentView.swift — top-level layout.
//
// `NavigationSplitView` gives us the macOS-native sidebar pattern
// (collapsible via the toolbar button for free). Sidebar holds
// the Source/Radio/Display accordion; the detail column holds the
// spectrum/waterfall + status bar, with the header toolbar
// attached via `.toolbar`.

import AppKit
import SwiftUI

struct ContentView: View {
    @Environment(CoreModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    /// Sheet state lives up here (not inside the toolbar)
    /// because the subview-wrapped version of the RR button
    /// didn't render in the toolbar — inlining the button in
    /// the ToolbarItem closure did. Hoisting `@State` here
    /// keeps the toolbar closure flat while letting `.sheet`
    /// attach to a plain View that renders the dialog.
    @State private var showingRadioReference: Bool = false

    /// Right-side transcription panel visibility. Same pattern
    /// as the RadioReference sheet: view-layer state, toggled
    /// from the header toolbar. The driver runs independent of
    /// panel visibility so the user can show/hide without
    /// stopping transcription.
    @State private var showingTranscription: Bool = false

    var body: some View {
        NavigationSplitView {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 240, ideal: 280)
        } detail: {
            HStack(spacing: 0) {
                VStack(spacing: 0) {
                    CenterView()
                    StatusBar()
                }
                if showingTranscription {
                    Divider()
                    TranscriptionPanel()
                        .transition(.move(edge: .trailing))
                }
            }
            // Match GTK's 200 ms Revealer SlideLeft animation.
            .animation(.easeInOut(duration: 0.2), value: showingTranscription)
        }
        .toolbar {
            HeaderToolbar(
                showingRadioReference: $showingRadioReference,
                showingTranscription: $showingTranscription
            )
        }
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
        // Also re-probe the USB bus for RTL-SDR hardware on
        // refocus — without IOKit hotplug monitoring (tracked
        // in issue #363) a dongle plugged in after launch would
        // otherwise stay invisible to `hasLocalRtlSdr` and hide
        // the rtl_tcp server panel until the next launch.
        // Refocus covers the common "plug it in, return to the
        // app" flow; a dongle inserted while the window is
        // already focused still won't surface until focus
        // flips, which is what #363 fixes properly. Per
        // `CodeRabbit` round 6 on PR #362.
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

/// Accordion sidebar: three sections, each collapsible. SwiftUI's
/// `Form` + `Section` inside a `List` gives the native "sidebar
/// panels" look without custom drawing.
struct SidebarView: View {
    var body: some View {
        Form {
            SourceSection()
            RadioSection()
            DisplaySection()
            RecordingSection()
            BookmarksSection()
            // `RtlTcpServerSection` is visible only when a
            // local RTL-SDR dongle is detected — the section
            // itself is always included in the form, but the
            // body collapses to a single "no dongle" caption
            // otherwise so it doesn't clutter the sidebar on a
            // network/file source setup.
            RtlTcpServerSection()
        }
        .formStyle(.grouped)
    }
}

#Preview {
    ContentView()
        .environment(CoreModel())
}
