//
// CenterView.swift — main spectrum + waterfall area.
//
// Hosts the Metal-backed renderer via `SpectrumWaterfallView`
// (NSViewRepresentable + MTKView). The renderer consumes
// min/max dB bindings from `CoreModel` — the user adjusts these
// via the Display sidebar section, and the shader saturate()
// maps the dB range to the visible vertical axis.
//
// In this sub-PR (M4/1) the renderer is fed by a synthetic
// FFT source, so you'll see three moving peaks on a noise floor
// regardless of whether Play is active. Sub-PR 3 swaps the
// source for the real `SdrCore.withLatestFftFrame` and
// invalidates on each FFT tick.

import SwiftUI

struct CenterView: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        @Bindable var m = model
        SpectrumWaterfallView(
            minDb: $m.minDb,
            maxDb: $m.maxDb
        )
        .frame(minHeight: 300)
    }
}
