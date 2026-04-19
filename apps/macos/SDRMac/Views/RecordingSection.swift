//
// RecordingSection.swift — sidebar panel for audio WAV
// recording (#239). Engine-side: see
// `UiToDsp::StartAudioRecording` / `StopAudioRecording` at
// `crates/sdr-core/src/controller.rs`. The engine owns the
// writer, WAV header, and buffer discipline; this view only
// toggles the command and reflects the engine's confirmed
// state (`audioRecordingPath`).
//
// Default destination: `~/Documents/SDRMac/Audio/<timestamp>.wav`,
// created on first Record tap.

import AppKit
import SwiftUI

struct RecordingSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("Recording") {
            let recording = model.audioRecordingPath != nil

            Toggle("Audio", isOn: Binding(
                get: { recording },
                set: { on in
                    if on {
                        let path = Self.generateRecordingPath()
                        model.startAudioRecording(to: path)
                    } else {
                        model.stopAudioRecording()
                    }
                }
            ))

            if let path = model.audioRecordingPath {
                LabeledContent("File") {
                    Button {
                        // Reveal in Finder — select the file so the
                        // user can immediately drag / play / delete
                        // it. If the file isn't actually there yet
                        // (engine hasn't flushed), fall back to the
                        // parent directory so the button stays
                        // useful.
                        let url = URL(fileURLWithPath: path)
                        if FileManager.default.fileExists(atPath: url.path) {
                            NSWorkspace.shared.activateFileViewerSelecting([url])
                        } else {
                            NSWorkspace.shared.open(url.deletingLastPathComponent())
                        }
                    } label: {
                        Text((path as NSString).lastPathComponent)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                    .buttonStyle(.plain)
                    .help(path)
                }
            } else {
                Text("Not recording")
                    .foregroundStyle(.secondary)
                    .font(.caption)
            }
        }
    }

    /// Build a full destination path for a new recording. The
    /// filename pattern is `sdr-audio-YYYYMMDD-HHMMSS-SSS.wav` —
    /// sortable, human-readable, and millisecond-precise so
    /// rapid stop/start cycles (button spam, scripted automation)
    /// produce distinct filenames. On the off chance that two
    /// timestamps still collide — or the user explicitly asked
    /// for a name that exists from a previous session — an
    /// integer suffix is appended until an unused name is found.
    static func generateRecordingPath() -> String {
        let dir = SDRMacApp.audioRecordingsDirectory()
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyyMMdd-HHmmss-SSS"
        let stamp = formatter.string(from: Date())
        var candidate = dir.appendingPathComponent("sdr-audio-\(stamp).wav")
        var suffix = 1
        while FileManager.default.fileExists(atPath: candidate.path) {
            candidate = dir.appendingPathComponent("sdr-audio-\(stamp)-\(suffix).wav")
            suffix += 1
        }
        return candidate.path
    }
}
