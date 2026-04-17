//
// SettingsView.swift — Cmd-, settings scene.
//
// Three panes: General (config file location, log level), Audio
// (output device picker deferred to v2), Advanced (ABI version).

import SwiftUI
import SdrCoreKit

struct SettingsView: View {
    var body: some View {
        TabView {
            GeneralPane()
                .tabItem { Label("General", systemImage: "gear") }
            AudioPane()
                .tabItem { Label("Audio", systemImage: "speaker.wave.2") }
            AdvancedPane()
                .tabItem { Label("Advanced", systemImage: "wrench.and.screwdriver") }
        }
        .padding(20)
        .frame(width: 480, height: 320)
    }
}

private struct GeneralPane: View {
    var body: some View {
        Form {
            LabeledContent("Config file") {
                // Render the live path via the same helper
                // SDRMacApp uses at bootstrap time, so a bundle-id
                // rename or FileManager layout change keeps the
                // settings pane in sync automatically.
                Text(verbatim: SDRMacApp.defaultConfigPath().path)
                    .font(.system(.body, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
            }
        }
    }
}

private struct AudioPane: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Form {
            LabeledContent("Output device") {
                Text("System default").foregroundStyle(.secondary)
            }
            LabeledContent("Volume") {
                @Bindable var m = model
                Slider(
                    value: $m.volume,
                    in: 0...1,
                    onEditingChanged: { editing in
                        if !editing {
                            model.setVolume(model.volume)
                        }
                    }
                )
            }
        }
    }
}

private struct AdvancedPane: View {
    var body: some View {
        Form {
            LabeledContent("ABI version") {
                let v = SdrCore.abiVersion
                Text("\(v.major).\(v.minor)")
                    .font(.system(.body, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
        }
    }
}
