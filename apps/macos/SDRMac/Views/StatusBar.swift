//
// StatusBar.swift — compact strip at the bottom of the detail
// column. Signal level, effective sample rate, last error.

import SwiftUI

struct StatusBar: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        HStack(spacing: 16) {
            Label("\(Int(model.signalLevelDb)) dB", systemImage: "waveform")
            Label(formatRate(model.effectiveSampleRateHz), systemImage: "metronome")
            Spacer()
            if let err = model.lastError {
                HStack(spacing: 4) {
                    Label(err, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.red)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .help(err)
                    Button {
                        model.clearError()
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                            .foregroundStyle(.red.opacity(0.7))
                    }
                    .buttonStyle(.plain)
                    .help("Dismiss error")
                }
            }
        }
        .font(.caption)
        .padding(.horizontal, 12)
        .frame(height: 22)
        .background(.bar)
    }
}
