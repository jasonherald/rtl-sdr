//
// CenterView.swift — main spectrum + waterfall area.
//
// Hosts the Metal-backed renderer via `SpectrumWaterfallView`
// (NSViewRepresentable + CAMetalLayer). The renderer consumes
// min/max dB bindings from `CoreModel` — the user adjusts these
// via the Display sidebar section, and the shader saturate()
// maps the dB range to the visible vertical axis. The renderer
// also pulls FFT frames directly from `model.core` on each
// display-link tick.

import SwiftUI

struct CenterView: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        @Bindable var m = model
        SpectrumWaterfallView(
            model: model,
            minDb: $m.minDb,
            maxDb: $m.maxDb
        )
        .frame(minHeight: 300)
    }
}
