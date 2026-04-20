//
// RecordingSection.swift — sidebar panel for WAV recording.
//
// Two engine-side features colocated in one sidebar section:
//   - Audio (#239): demodulated audio at AUDIO_SAMPLE_RATE,
//     `UiToDsp::StartAudioRecording` / `StopAudioRecording`.
//     ~96 KB/s mono @ 16-bit / 48 kHz — small files.
//   - IQ (#238): raw complex samples at the current tuner
//     rate, `UiToDsp::StartIqRecording` / `StopIqRecording`.
//     Two-channel @ the source rate — at 2.048 MHz that's
//     ~15 MB/s, so files grow quickly.
//
// The engine owns the writers, WAV headers, and buffer
// discipline; this view only toggles the commands and
// reflects the engine's confirmed state
// (`audioRecordingPath` / `iqRecordingPath`).
//
// Default destinations:
//   Audio → `~/Documents/SDRMac/Audio/sdr-audio-<timestamp>.wav`
//   IQ    → `~/Documents/SDRMac/IQ/sdr-iq-<timestamp>.wav`

import AppKit
import SwiftUI

struct RecordingSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("Recording") {
            RecordingToggleRow(
                title: "Audio",
                recordingPath: model.audioRecordingPath,
                generatePath: Self.generateAudioRecordingPath,
                start: { path in model.startAudioRecording(to: path) },
                stop: { model.stopAudioRecording() }
            )

            RecordingToggleRow(
                title: "IQ",
                recordingPath: model.iqRecordingPath,
                generatePath: Self.generateIqRecordingPath,
                start: { path in model.startIqRecording(to: path) },
                stop: { model.stopIqRecording() }
            )
        }
    }

    /// Audio WAV destination path. Filename pattern
    /// `sdr-audio-YYYYMMDD-HHMMSS-SSS.wav` — sortable,
    /// human-readable, and millisecond-precise so rapid
    /// stop/start cycles produce distinct filenames. An
    /// integer suffix is appended on the off chance two
    /// timestamps still collide.
    ///
    /// `nonisolated` because these only touch `FileManager`,
    /// `DateFormatter`, and path-building — no main-actor
    /// state — and the `RecordingToggleRow` closure params
    /// need a callable type that's safe from any isolation
    /// domain.
    nonisolated static func generateAudioRecordingPath() -> String {
        generateRecordingPath(
            in: SDRMacApp.audioRecordingsDirectory(),
            prefix: "sdr-audio"
        )
    }

    /// IQ WAV destination path. Same filename pattern as
    /// audio but lives under `~/Documents/SDRMac/IQ/`.
    nonisolated static func generateIqRecordingPath() -> String {
        generateRecordingPath(
            in: SDRMacApp.iqRecordingsDirectory(),
            prefix: "sdr-iq"
        )
    }

    /// Shared path builder for both recording kinds.
    private nonisolated static func generateRecordingPath(in dir: URL, prefix: String) -> String {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyyMMdd-HHmmss-SSS"
        let stamp = formatter.string(from: Date())
        var candidate = dir.appendingPathComponent("\(prefix)-\(stamp).wav")
        var suffix = 1
        while FileManager.default.fileExists(atPath: candidate.path) {
            candidate = dir.appendingPathComponent("\(prefix)-\(stamp)-\(suffix).wav")
            suffix += 1
        }
        return candidate.path
    }
}

/// One toggle + filename + reveal-in-Finder row for a recording
/// feature. Parameterized so audio and IQ share the exact same
/// UX without copy-pasted event-watching code.
///
/// Each row owns its own `pendingTransition` lock — audio and IQ
/// commands are independent; a pending IQ start shouldn't block
/// the user from also starting audio recording.
private struct RecordingToggleRow: View {
    @Environment(CoreModel.self) private var model

    let title: String
    let recordingPath: String?
    let generatePath: () -> String
    let start: (String) -> Void
    let stop: () -> Void

    /// True between firing a start/stop command and observing
    /// the matching `recordingPath` change. Locks this toggle in
    /// the meantime so a rapid double-click can't fire two
    /// Start* commands (the controller replaces the writer on
    /// each start, dropping the first writer mid-write → two
    /// partial WAV files). Cleared in `.onChange(of:)` when the
    /// engine confirms the transition, or when a `lastError`
    /// arrives while we're pending (start failed).
    @State private var pendingTransition: Bool = false

    var body: some View {
        let recording = recordingPath != nil

        Toggle(title, isOn: Binding(
            get: { recording },
            set: { on in
                guard !pendingTransition else { return }
                pendingTransition = true
                if on {
                    start(generatePath())
                } else {
                    stop()
                }
            }
        ))
        .disabled(pendingTransition)
        .onChange(of: recordingPath) { _, _ in
            pendingTransition = false
        }
        .onChange(of: model.lastError) { _, new in
            if pendingTransition && new != nil {
                pendingTransition = false
            }
        }

        if let path = recordingPath {
            LabeledContent("File") {
                Button {
                    // Reveal in Finder. Falls back to the parent
                    // directory if the file isn't on disk yet
                    // (engine may not have flushed the first
                    // frame by the time the user clicks).
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
        }
    }
}
