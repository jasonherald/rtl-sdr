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
                // Picker rebuilds the device list on every appear —
                // CoreAudio hot-plugs are common (Bluetooth headsets,
                // USB interfaces); the user shouldn't have to restart
                // the app to see a freshly-connected device. The
                // engine's transactional swap guarantees a bad pick
                // rolls back to the previous route automatically.
                Picker("", selection: Binding(
                    get: { model.selectedAudioDeviceUid },
                    set: { model.setAudioDevice($0) }
                )) {
                    // "" is the engine's "system default" sentinel —
                    // show it as a first-class option regardless of
                    // backend so the user always has a guaranteed-
                    // working route to fall back on.
                    Text("System default").tag("")
                    ForEach(model.audioDevices) { dev in
                        // Skip a backend-emitted empty-UID duplicate
                        // so we never show two "System default"
                        // options (stub backend does emit one; real
                        // backends typically don't).
                        if !dev.uid.isEmpty {
                            Text(dev.displayName).tag(dev.uid)
                        }
                    }
                }
                .labelsHidden()
                .onAppear { model.refreshAudioDevices() }
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
