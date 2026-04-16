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
