//
// AudioPanelView.swift — Audio activity panel (closes #445).
//
// Four flat Sections matching the GTK
// `crates/sdr-ui/src/sidebar/audio_panel.rs` layout:
//
//   - Output         — device picker + sink type (Local / Network)
//   - Volume         — slider 0..100% (moved here from RadioSection)
//   - Network sink   — host / port / protocol / status
//                      (visible only when sink type = Network)
//   - Recording      — Audio + IQ toggles
//
// Volume persistence on the Mac side already round-trips
// through `UserDefaults` via `CoreModel.setVolume`. Cross-
// frontend `audio_volume` config-key sharing with the Linux
// side is a follow-up — not blocking the panel landing.

import SwiftUI
import SdrCoreKit

struct AudioPanelView: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Form {
            OutputSection()
            VolumeSection()
            // Network-sink section is conditional — only
            // shown when the user routes audio over the
            // network. Hides clutter for the local-sink path.
            if model.audioSinkType == .network {
                NetworkSinkSection()
            }
            // Existing RecordingSection slots in cleanly —
            // its body is already a `Section("Recording") { … }`
            // with the two toggle rows.
            RecordingSection()
        }
        .formStyle(.grouped)
    }
}

// ============================================================
//  Output — device picker + sink type
// ============================================================

private struct OutputSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            LabeledContent("Sink") {
                Picker("", selection: Binding(
                    get: { model.audioSinkType },
                    set: { model.setAudioSinkType($0) }
                )) {
                    ForEach(AudioSinkType.allCases, id: \.self) { t in
                        Text(t.label).tag(t)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
            }

            // Device picker is only meaningful for the local
            // sink — network audio doesn't run through CoreAudio.
            if model.audioSinkType == .local {
                LabeledContent("Device") {
                    Picker("", selection: Binding(
                        get: { model.selectedAudioDeviceUid },
                        set: { model.setAudioDevice($0) }
                    )) {
                        // Empty string → "system default" sentinel
                        // (matches the engine's empty-string-is-default
                        // contract).
                        Text("System default").tag("")
                        ForEach(model.audioDevices) { dev in
                            Text(dev.displayName).tag(dev.uid)
                        }
                    }
                    .labelsHidden()
                }
            }
        } header: {
            Text("Output")
        } footer: {
            Text("Where demodulated audio goes — local CoreAudio device or a network stream.")
                .font(.caption)
        }
        .onAppear {
            // Re-enumerate CoreAudio outputs whenever the
            // panel opens so a hot-plugged headset / DAC
            // shows up in the picker without an app
            // relaunch. Cheap call; runs on main actor. Per
            // `CodeRabbit` round 2 on PR #493.
            model.refreshAudioDevices()
        }
    }
}

// ============================================================
//  Volume
// ============================================================

private struct VolumeSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            LabeledContent("Volume") {
                VStack(spacing: 2) {
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
                    Text("\(Int(model.volume * 100))%")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        } header: {
            Text("Volume")
        } footer: {
            Text("Output level for the demodulated audio stream.")
                .font(.caption)
        }
    }
}

// ============================================================
//  Network sink — host / port / protocol / status
// ============================================================

private struct NetworkSinkSection: View {
    @Environment(CoreModel.self) private var model
    @State private var hostEdit: String = ""
    @State private var portEdit: String = ""
    /// Draft-local protocol value — only flushes to the model
    /// on Apply, matching how `hostEdit` / `portEdit` work.
    /// Per `CodeRabbit` round 2 on PR #493: the previous
    /// direct-write to `model.networkSinkProtocol` violated
    /// the explicit-commit contract, leaving an un-applied
    /// edit live for `syncToEngine()` to pick up.
    @State private var protocolEdit: NetworkProtocol = .tcpServer
    @State private var didPrefill: Bool = false

    var body: some View {
        Section {
            TextField("Host", text: $hostEdit)
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()

            LabeledContent("Port") {
                TextField("3490", text: $portEdit)
                    .textFieldStyle(.roundedBorder)
                    .multilineTextAlignment(.trailing)
                    .frame(maxWidth: 90)
            }

            LabeledContent("Protocol") {
                Picker("", selection: $protocolEdit) {
                    ForEach(NetworkProtocol.allCases, id: \.self) { p in
                        Text(p.label).tag(p)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
            }

            Button {
                guard let port = parsePort() else { return }
                let host = hostEdit.trimmingCharacters(in: .whitespacesAndNewlines)
                model.applyNetworkSinkConfig(
                    host: host,
                    port: port,
                    protocol: protocolEdit
                )
            } label: {
                Label("Apply", systemImage: "checkmark.circle")
            }
            .disabled(applyDisabled)

            LabeledContent("Status") {
                Text(formatStatus(model.networkSinkStatus))
                    .font(.caption)
                    .foregroundStyle(statusColor(model.networkSinkStatus))
                    .lineLimit(2)
                    .multilineTextAlignment(.trailing)
            }
        } header: {
            Text("Network sink")
        } footer: {
            Text("Stream demodulated audio to a remote host. TCP server listens for clients; UDP fires-and-forgets.")
                .font(.caption)
        }
        .onAppear {
            guard !didPrefill else { return }
            didPrefill = true
            hostEdit = model.networkSinkHost
            portEdit = String(model.networkSinkPort)
            protocolEdit = model.networkSinkProtocol
        }
    }

    private func parsePort() -> UInt16? {
        guard let raw = Int(portEdit.trimmingCharacters(in: .whitespacesAndNewlines)),
              (1...Int(UInt16.max)).contains(raw) else {
            return nil
        }
        return UInt16(raw)
    }

    private var applyDisabled: Bool {
        hostEdit.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || parsePort() == nil
    }

    private func formatStatus(_ s: NetworkSinkStatus) -> String {
        switch s {
        case .inactive:
            return "Inactive"
        case .active(let endpoint, let proto):
            return "Active — \(proto.label) \(endpoint)"
        case .error(let message):
            return "Error — \(message)"
        }
    }

    private func statusColor(_ s: NetworkSinkStatus) -> Color {
        switch s {
        case .error:    return .red
        case .active:   return .secondary
        case .inactive: return .secondary
        }
    }
}
