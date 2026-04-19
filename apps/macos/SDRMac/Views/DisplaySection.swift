//
// DisplaySection.swift — sidebar panel for spectrum/waterfall
// display settings. FFT size/window (engine-side), min/max dB
// (pure UI, consumed by the Metal renderer in M4).

import SwiftUI
import SdrCoreKit

struct DisplaySection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("Display") {
            Picker("FFT Size", selection: Binding(
                get: { model.fftSize },
                set: { model.setFftSize($0) }
            )) {
                ForEach([1024, 2048, 4096, 8192], id: \.self) {
                    Text("\($0)").tag($0)
                }
            }

            Picker("Window", selection: Binding(
                get: { model.fftWindow },
                set: { model.setFftWindow($0) }
            )) {
                ForEach(FftWindow.allCases, id: \.self) {
                    Text($0.label).tag($0)
                }
            }

            // Display-side averaging — pure Swift renderer work,
            // no engine round-trip. Switching modes reseeds the
            // averaging buffer so there's no one-frame artifact.
            Picker("Averaging", selection: Binding(
                get: { model.averagingMode },
                set: { model.averagingMode = $0 }
            )) {
                ForEach(AveragingMode.allCases, id: \.self) {
                    Text($0.label).tag($0)
                }
            }

            LabeledContent("FFT Rate") {
                VStack(spacing: 2) {
                    @Bindable var m = model
                    Slider(
                        value: $m.fftRateFps,
                        in: 5...60,
                        step: 1,
                        onEditingChanged: { editing in
                            if !editing {
                                model.setFftRate(model.fftRateFps)
                            }
                        }
                    )
                    Text("\(Int(model.fftRateFps)) fps")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            LabeledContent("Min dB") {
                VStack(spacing: 2) {
                    @Bindable var m = model
                    Slider(value: $m.minDb, in: -150...0)
                    Text("\(Int(model.minDb))").font(.caption).foregroundStyle(.secondary)
                }
            }

            LabeledContent("Max dB") {
                VStack(spacing: 2) {
                    @Bindable var m = model
                    Slider(value: $m.maxDb, in: -150...0)
                    Text("\(Int(model.maxDb))").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }
}
