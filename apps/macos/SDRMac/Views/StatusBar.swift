//
// StatusBar.swift — compact strip at the bottom of the detail
// column. Signal level, effective sample rate, antenna line,
// last error.

import SwiftUI
import SdrCoreKit

struct StatusBar: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        HStack(spacing: 16) {
            Label("\(Int(model.signalLevelDb)) dB", systemImage: "waveform")
            Label(formatRate(model.effectiveSampleRateHz), systemImage: "metronome")
            // Antenna-dimension line — λ/2 + λ/4 + V-angle hint
            // for the current tuned frequency. Mirrors the GTK
            // status bar (#157 / Linux PR #418); Mac side per
            // issue #487. Hidden below the renderable floor
            // (3 kHz) so a user mis-tuned near DC sees no noise
            // here. `Antenna` lives in SdrCoreKit so the helper
            // is share-able with future Mac surfaces.
            if let antennaLine = Antenna.formatAntennaLine(freqHz: model.centerFrequencyHz) {
                Label(antennaLine, systemImage: "antenna.radiowaves.left.and.right")
                    .help("Half-wave dipole + quarter-wave element + suggested V-dipole arm angle for \(formatRate(model.centerFrequencyHz))")
            }
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
