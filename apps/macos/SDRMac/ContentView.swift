//
// ContentView.swift — top-level layout.
//
// `NavigationSplitView` gives us the macOS-native sidebar pattern
// (collapsible via the toolbar button for free). Sidebar holds
// the Source/Radio/Display accordion; the detail column holds the
// spectrum/waterfall + status bar, with the header toolbar
// attached via `.toolbar`.

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
