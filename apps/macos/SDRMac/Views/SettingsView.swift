//
// SettingsView.swift — Cmd-, settings scene.
//
// Panes: General (config file location), Audio (output device +
// volume), RadioReference (credentials — issue #241), Advanced
// (ABI version).

import SwiftUI
import SdrCoreKit

struct SettingsView: View {
    var body: some View {
        TabView {
            GeneralPane()
                .tabItem { Label("General", systemImage: "gear") }
            AudioPane()
                .tabItem { Label("Audio", systemImage: "speaker.wave.2") }
            RadioReferencePane()
                .tabItem { Label("RadioReference", systemImage: "antenna.radiowaves.left.and.right") }
            AdvancedPane()
                .tabItem { Label("Advanced", systemImage: "wrench.and.screwdriver") }
        }
        .padding(20)
        .frame(width: 520, height: 360)
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

    /// Local edit buffer for the network sink port. Bound to a
    /// `TextField` so the user can backspace through the value
    /// without triggering a per-keystroke engine command. An
    /// empty string parses as "no port chosen" and the Apply
    /// button disables. Committed on Apply.
    @State private var portEdit: String = ""

    /// Local edit buffer for the host. Same rationale as
    /// `portEdit` — host edits shouldn't rebuild the listener on
    /// every character.
    @State private var hostEdit: String = ""

    /// One-shot latch so `.onAppear` only seeds `hostEdit` /
    /// `portEdit` from the model the first time this pane is
    /// rendered. Without it, tabbing away from Audio and back
    /// clobbers any in-progress endpoint edits (same pattern
    /// `RadioReferencePane` uses). Per `CodeRabbit` round 1.
    @State private var didPrefill: Bool = false

    var body: some View {
        Form {
            LabeledContent("Output device") {
                Picker("", selection: Binding(
                    get: { model.selectedAudioDeviceUid },
                    set: { model.setAudioDevice($0) }
                )) {
                    Text("System default").tag("")
                    ForEach(model.audioDevices) { dev in
                        if !dev.uid.isEmpty {
                            Text(dev.displayName).tag(dev.uid)
                        }
                    }
                }
                .labelsHidden()
                .onAppear { model.refreshAudioDevices() }
                // Greyed out while the network sink is active —
                // the setting still round-trips to the engine, but
                // it only affects audio once the user switches the
                // sink back to `.local`.
                .disabled(model.audioSinkType == .network)
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

            // --------------------------------------------------
            //  Network audio sink — issue #247
            // --------------------------------------------------
            Section {
                Picker("Sink", selection: Binding(
                    get: { model.audioSinkType },
                    set: { model.setAudioSinkType($0) }
                )) {
                    ForEach(AudioSinkType.allCases, id: \.self) { t in
                        Text(t.label).tag(t)
                    }
                }
                .pickerStyle(.segmented)

                if model.audioSinkType == .network {
                    TextField("Host", text: $hostEdit)
                        .textFieldStyle(.roundedBorder)
                        .autocorrectionDisabled()
                    TextField("Port", text: $portEdit)
                        .textFieldStyle(.roundedBorder)
                    Picker("Protocol", selection: Binding(
                        get: { model.networkSinkProtocol },
                        set: { proto in
                            // Protocol changes count as endpoint
                            // changes — rebuild with the buffered
                            // host/port instead of letting the
                            // picker drift out of sync with the
                            // engine. Empty / whitespace-only host
                            // or bad port input keeps the engine's
                            // current endpoint.
                            let host = normalizedHost
                            if !host.isEmpty, let port = portValue() {
                                model.applyNetworkSinkConfig(
                                    host: host,
                                    port: port,
                                    protocol: proto
                                )
                            }
                        }
                    )) {
                        ForEach(NetworkProtocol.allCases, id: \.self) { p in
                            Text(p.label).tag(p)
                        }
                    }

                    HStack {
                        Button {
                            let host = normalizedHost
                            guard !host.isEmpty, let port = portValue() else { return }
                            model.applyNetworkSinkConfig(
                                host: host,
                                port: port,
                                protocol: model.networkSinkProtocol
                            )
                        } label: {
                            Label("Apply", systemImage: "arrow.up.circle")
                        }
                        .disabled(applyButtonDisabled)
                    }

                    networkStatusRow
                }
            } header: {
                Text("Network stream")
            } footer: {
                Text(
                    """
                    Stream post-demod audio (48 kHz stereo) to a TCP or UDP \
                    endpoint. TCP server mode has the app listen on the \
                    configured port for client connections. UDP sends \
                    unicast packets to the chosen host.
                    """
                )
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        }
        .onAppear {
            // One-shot prefill: seed the local edit buffers from
            // the model only on first appear. Repeated .onAppear
            // fires when tabbing away and back in the TabView
            // would otherwise clobber any in-progress edits the
            // user hadn't hit Apply on yet.
            guard !didPrefill else { return }
            didPrefill = true
            hostEdit = model.networkSinkHost
            portEdit = String(model.networkSinkPort)
        }
    }

    /// Parse the current port-edit buffer into a `UInt16`.
    /// Returns `nil` on empty / out-of-range / non-numeric
    /// input; the Apply button and protocol-change side effect
    /// disable/no-op in that case instead of pushing a bad value.
    private func portValue() -> UInt16? {
        guard let raw = Int(portEdit.trimmingCharacters(in: .whitespaces)),
              (1...Int(UInt16.max)).contains(raw) else {
            return nil
        }
        return UInt16(raw)
    }

    /// Host with leading/trailing whitespace stripped. Single
    /// source of truth for the normalization step every
    /// push-to-engine path runs through.
    private var normalizedHost: String {
        hostEdit.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var applyButtonDisabled: Bool {
        normalizedHost.isEmpty || portValue() == nil
    }

    /// Render the engine-reported network sink status below the
    /// form. Colors mirror the RadioReference pane's convention
    /// (red = error, green = success, secondary = idle).
    @ViewBuilder
    private var networkStatusRow: some View {
        switch model.networkSinkStatus {
        case .inactive:
            Text("Status: inactive")
                .font(.caption)
                .foregroundStyle(.secondary)
        case .active(let endpoint, let proto):
            Text("Status: streaming to \(endpoint) over \(proto.label)")
                .font(.caption)
                .foregroundStyle(.green)
        case .error(let msg):
            Text("Status: \(msg)")
                .font(.caption)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }
    }
}

/// RadioReference credential management — mirrors the GTK
/// Preferences → Accounts page. Saved credentials live in the
/// macOS Keychain (same keychain item the Linux build uses so
/// cross-platform installs share a login). The pane offers
/// test-before-save and a one-click delete.
private struct RadioReferencePane: View {
    @Environment(CoreModel.self) private var model

    @State private var username: String = ""
    @State private var password: String = ""
    @State private var isWorking: Bool = false
    @State private var statusMessage: String = ""
    @State private var statusIsError: Bool = false
    /// One-shot latch so `prefillFromKeychain` only runs on the
    /// first `.onAppear`. In a TabView, switching tabs away and
    /// back re-fires `.onAppear`, which would otherwise clobber
    /// any username edits the user made (password isn't affected
    /// because it's never prefilled, but that asymmetry was
    /// exactly the mixed-pair risk the rabbit flagged in round 5
    /// of PR #346).
    @State private var didPrefill: Bool = false

    var body: some View {
        Form {
            Section {
                TextField("Username", text: $username)
                    .textContentType(.username)
                    .autocorrectionDisabled()
                SecureField("Password", text: $password)
                    .textContentType(.password)
            } header: {
                Text("Account")
            } footer: {
                Text(
                    """
                    Requires a RadioReference.com account with API access. \
                    Credentials are stored in your macOS Keychain and are \
                    never written to the app's config file.
                    """
                )
                .font(.caption)
                .foregroundStyle(.secondary)
            }

            Section {
                HStack {
                    Button {
                        Task { await testAndSave() }
                    } label: {
                        Label("Test & Save", systemImage: "checkmark.seal")
                    }
                    .disabled(isWorking || username.isEmpty || password.isEmpty)

                    Button(role: .destructive) {
                        deleteCredentials()
                    } label: {
                        Label("Clear stored", systemImage: "trash")
                    }
                    .disabled(isWorking || !model.radioReferenceHasCredentials)

                    if isWorking {
                        ProgressView().controlSize(.small)
                    }
                }

                if !statusMessage.isEmpty {
                    Text(statusMessage)
                        .font(.callout)
                        .foregroundStyle(statusIsError ? .red : .green)
                        .textSelection(.enabled)
                }

                if model.radioReferenceHasCredentials {
                    Text("Credentials are currently stored.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    Text("No credentials stored.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .onAppear {
            guard !didPrefill else { return }
            didPrefill = true
            prefillFromKeychain()
        }
    }

    /// Load the stored username (if any) into the TextField so
    /// the user can see which account is currently active. We
    /// deliberately DON'T prefill the password — SecureField +
    /// stored password is a bad combination (copy/paste risk,
    /// visible on clear-password-field toggles). The user
    /// re-enters it only if they want to change it.
    private func prefillFromKeychain() {
        // `try?` flattens the function's `(String, String)?` return
        // into a single optional, so `guard let` gives us a
        // non-optional tuple.
        guard let creds = try? SdrCore.loadRadioReferenceCredentials() else { return }
        username = creds.user
    }

    private func testAndSave() async {
        isWorking = true
        defer { isWorking = false }
        statusMessage = "Testing credentials…"
        statusIsError = false

        let user = username
        let pass = password
        let result = await Task.detached(priority: .userInitiated) {
            SdrCore.testRadioReferenceCredentials(user: user, password: pass)
        }.value

        switch result {
        case .valid:
            // Credentials check out — persist them. A save error
            // here would be a keyring-backend issue, surfaced as
            // a red status without invalidating the test result.
            do {
                try SdrCore.saveRadioReferenceCredentials(user: user, password: pass)
                statusMessage = "Credentials saved."
                statusIsError = false
                password = ""
                model.refreshRadioReferenceCredentialsFlag()
            } catch let err as SdrCoreError {
                // Surface the FFI message directly — the default
                // `localizedDescription` for our error type is an
                // unhelpful "The operation couldn't be completed"
                // string, which eats the real reason.
                statusMessage = "Credentials valid, but save failed: [\(err.code)] \(err.message)"
                statusIsError = true
            } catch {
                statusMessage = "Credentials valid, but save failed: \(error)"
                statusIsError = true
            }
        case .invalidCredentials(let msg):
            statusMessage = "Invalid credentials: \(msg)"
            statusIsError = true
        case .invalidInput(let msg):
            statusMessage = "\(msg)"
            statusIsError = true
        case .networkError(let msg):
            statusMessage = "Network error: \(msg)"
            statusIsError = true
        }
    }

    private func deleteCredentials() {
        do {
            try SdrCore.deleteRadioReferenceCredentials()
            statusMessage = "Stored credentials cleared."
            statusIsError = false
            username = ""
            password = ""
            model.refreshRadioReferenceCredentialsFlag()
        } catch let err as SdrCoreError {
            // Match the save path: surface the FFI message so
            // backend failures stay actionable. `localizedDescription`
            // on our SdrCoreError collapses to a generic "The
            // operation couldn't be completed" string. Per
            // CodeRabbit round 1 on PR #346.
            statusMessage = "Delete failed: [\(err.code)] \(err.message)"
            statusIsError = true
        } catch {
            statusMessage = "Delete failed: \(error)"
            statusIsError = true
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
