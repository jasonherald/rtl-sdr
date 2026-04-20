//
// TranscriptionPanel.swift — right-side slide-out for live
// transcription via Apple SpeechAnalyzer.
//
// Layout mirrors the GTK transcript panel:
//   [Enable toggle]          (SwitchRow equivalent)
//   [Status line]            (hidden when idle)
//   [Progress bar]           (hidden when no download)
//   -- divider --
//   [Scrolling timestamped lines]  (monospace, non-editable)
//   [Dimmed italic partial]  (live hypothesis)
//   -- divider --
//   [Clear button]
//
// Width 320pt, matching GTK's `transcript_scrolled.set_size_request(320, -1)`.

import SwiftUI

struct TranscriptionPanel: View {
    @Environment(TranscriptionDriver.self) private var driver

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            transcriptArea
            Divider()
            footer
        }
        .frame(width: 320)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    // MARK: - Header

    private var header: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Transcription")
                    .font(.headline)
                Spacer()
                Toggle(
                    "Enable",
                    isOn: Binding(
                        get: { driver.enabled },
                        set: { driver.toggle($0) }
                    )
                )
                .labelsHidden()
                .toggleStyle(.switch)
                .controlSize(.small)
            }

            if let statusText = statusMessage {
                Text(statusText)
                    .font(.caption)
                    .foregroundStyle(statusTextColor)
                    .lineLimit(2)
                    .textSelection(.enabled)
            }

            if let progress = driver.downloadProgress {
                ProgressView(value: progress)
                    .progressViewStyle(.linear)
                    .controlSize(.small)
            }
        }
        .padding(12)
    }

    private var statusMessage: String? {
        switch driver.status {
        case .idle: return nil
        case .preparing(let msg): return msg
        case .listening: return "Listening…"
        case .error(let msg): return msg
        }
    }

    private var statusTextColor: Color {
        if case .error = driver.status { return .red }
        return .secondary
    }

    // MARK: - Transcript area

    private var transcriptArea: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(driver.finalizedLines) { line in
                        HStack(alignment: .top, spacing: 6) {
                            Text(Self.timestampFormatter.string(from: line.timestamp))
                                .font(.system(.caption, design: .monospaced))
                                .foregroundStyle(.secondary)
                            Text(line.text)
                                .font(.system(.body, design: .monospaced))
                                .textSelection(.enabled)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        .id(line.id)
                    }

                    if !driver.partialText.isEmpty {
                        Text(driver.partialText)
                            .font(.system(.body, design: .monospaced).italic())
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                            .id(Self.partialAnchorID)
                    }
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            // Auto-scroll to the most recent finalized line (or
            // the partial if a new one is currently being heard).
            // Matches GTK's `scroll_to_mark` on insert.
            .onChange(of: driver.finalizedLines.count) { _, _ in
                if let last = driver.finalizedLines.last {
                    withAnimation { proxy.scrollTo(last.id, anchor: .bottom) }
                }
            }
            .onChange(of: driver.partialText) { _, newValue in
                if !newValue.isEmpty {
                    proxy.scrollTo(Self.partialAnchorID, anchor: .bottom)
                }
            }
        }
    }

    // MARK: - Footer

    private var footer: some View {
        HStack {
            Button {
                driver.clearTranscript()
            } label: {
                Label("Clear", systemImage: "trash")
            }
            .disabled(driver.finalizedLines.isEmpty && driver.partialText.isEmpty)

            Spacer()
        }
        .padding(12)
    }

    // MARK: - Helpers

    private static let partialAnchorID = "partial-line"

    private static let timestampFormatter: DateFormatter = {
        let df = DateFormatter()
        df.dateFormat = "HH:mm:ss"
        return df
    }()
}
