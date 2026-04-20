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

    var body: some View {
        Form {
            Section {
                TextField("Username", text: $username)
                    .textContentType(.username)
                    .disableAutocorrection(true)
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
        .onAppear(perform: prefillFromKeychain)
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
