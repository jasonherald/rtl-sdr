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

    var body: some View {
        NavigationSplitView {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 240, ideal: 280)
        } detail: {
            VStack(spacing: 0) {
                CenterView()
                StatusBar()
            }
        }
        .toolbar { HeaderToolbar() }
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
        }
        .formStyle(.grouped)
    }
}

#Preview {
    ContentView()
        .environment(CoreModel())
}
