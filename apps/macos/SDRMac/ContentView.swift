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

    /// Right-side bookmarks flyout visibility. Mirrors the GTK
    /// bookmarks_panel.rs flyout (issue #339). Toggled from the
    /// header toolbar / `⌘B`, persisted across launches in
    /// UserDefaults so the user's last-chosen layout sticks.
    /// Mutually exclusive with `showingTranscription` — the
    /// content area only has room for one right-side flyout;
    /// the `.onChange` handlers below enforce that.
    @State private var showingBookmarks: Bool =
        UserDefaults.standard.bool(forKey: ContentView.bookmarksFlyoutOpenKey)

    /// UserDefaults key for persisting the bookmarks flyout's
    /// open/closed state. Matches the Linux `bookmarks_flyout_open`
    /// config key so both frontends share wording.
    static let bookmarksFlyoutOpenKey = "SDRMac.bookmarks.flyoutOpen"

    /// Right-side flyout slide transition duration, in seconds.
    /// Shared between the transcription and bookmarks revealers
    /// so they stay in lockstep if one is tweaked. Matches GTK's
    /// 200 ms `Revealer.transition_duration`.
    static let rightFlyoutTransitionSeconds: Double = 0.2

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
                if showingBookmarks {
                    Divider()
                    BookmarksPanel(isPresented: $showingBookmarks)
                        .transition(.move(edge: .trailing))
                }
            }
            // Match GTK's Revealer SlideLeft animation. Shared
            // duration constant so both flyouts stay in lockstep.
            .animation(
                .easeInOut(duration: ContentView.rightFlyoutTransitionSeconds),
                value: showingTranscription
            )
            .animation(
                .easeInOut(duration: ContentView.rightFlyoutTransitionSeconds),
                value: showingBookmarks
            )
        }
        .toolbar {
            HeaderToolbar(
                showingRadioReference: $showingRadioReference,
                showingTranscription: $showingTranscription,
                showingBookmarks: $showingBookmarks
            )
        }
        // Mutual exclusivity between the two right-side flyouts.
        // Opening one closes the other rather than stacking,
        // matching the Linux treatment (a793cdc) and fitting
        // the content area's single-flyout width budget. Also
        // persists the bookmarks flyout's open/closed state —
        // the transcription panel is ephemeral, but bookmarks
        // is a durable browse surface the user wants back where
        // they left it.
        .onChange(of: showingBookmarks) { _, newValue in
            if newValue && showingTranscription {
                showingTranscription = false
            }
            UserDefaults.standard.set(newValue, forKey: ContentView.bookmarksFlyoutOpenKey)
        }
        .onChange(of: showingTranscription) { _, newValue in
            if newValue && showingBookmarks {
                showingBookmarks = false
            }
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
