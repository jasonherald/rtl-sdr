//
// SourceSection.swift — sidebar panel for source/tuner controls.
//
// MVP scope: RTL-SDR only (no device picker). Sample rate, gain
// (discrete when AGC off), AGC, PPM. The device-picker and the
// Network/File source forms land in v2 behind feature flags.

import SwiftUI

/// RTL-SDR supported sample rates (in Hz). Matches
/// `crates/sdr-rtlsdr::RATE_OPTIONS`.
private let rtlSdrSampleRates: [Double] = [
    250_000, 1_024_000, 1_536_000, 1_792_000, 1_920_000,
    2_048_000, 2_160_000, 2_400_000, 2_560_000, 2_880_000,
    3_200_000,
]

struct SourceSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("Source") {
            LabeledContent("Device") {
                Text(model.deviceInfo.isEmpty ? "—" : model.deviceInfo)
                    .foregroundStyle(.secondary)
            }

            LabeledContent("Sample rate") {
                @Bindable var m = model
                Picker("", selection: $m.sampleRateHz) {
                    ForEach(rtlSdrSampleRates, id: \.self) {
                        Text(formatRate($0)).tag($0)
                    }
                }
                .labelsHidden()
            }

            LabeledContent("Gain") {
                if model.agcEnabled {
                    Text("AGC").foregroundStyle(.secondary)
                } else if model.availableGains.isEmpty {
                    Text("—").foregroundStyle(.secondary)
                } else {
                    GainSlider(
                        steps: model.availableGains,
                        value: model.gainDb,
                        commit: { model.setGain($0) }
                    )
                }
            }

            Toggle("AGC", isOn: Binding(
                get: { model.agcEnabled },
                set: { model.setAgc($0) }
            ))

            LabeledContent("PPM") {
                Stepper(value: Binding(
                    get: { model.ppmCorrection },
                    set: { model.setPpm($0) }
                ), in: -100...100) {
                    Text("\(model.ppmCorrection)")
                }
            }
        }
    }
}

/// RTL-SDR exposes a discrete set of gain values (not a continuous
/// range). This slider snaps to the nearest entry in `steps`.
private struct GainSlider: View {
    let steps: [Double]
    let value: Double
    let commit: (Double) -> Void

    @State private var index: Double = 0

    var body: some View {
        VStack(spacing: 2) {
            Slider(
                value: $index,
                in: 0...Double(max(steps.count - 1, 0)),
                step: 1,
                onEditingChanged: { _ in
                    let i = Int(index.rounded())
                    if steps.indices.contains(i) { commit(steps[i]) }
                }
            )
            Text(String(format: "%.1f dB", currentDb))
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .onAppear { index = Double(closestIndex(for: value)) }
        .onChange(of: value) { _, new in
            index = Double(closestIndex(for: new))
        }
    }

    private var currentDb: Double {
        let i = Int(index.rounded())
        return steps.indices.contains(i) ? steps[i] : value
    }

    private func closestIndex(for v: Double) -> Int {
        guard !steps.isEmpty else { return 0 }
        var best = 0
        var bestDiff = abs(steps[0] - v)
        for (i, s) in steps.enumerated().dropFirst() {
            let d = abs(s - v)
            if d < bestDiff { bestDiff = d; best = i }
        }
        return best
    }
}

func formatRate(_ hz: Double) -> String {
    if hz >= 1_000_000 {
        return String(format: "%.3f MHz", hz / 1_000_000)
    } else if hz >= 1_000 {
        return String(format: "%.1f kHz", hz / 1_000)
    } else {
        return String(format: "%.0f Hz", hz)
    }
}
