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
        ZStack {
            // 1. Metal spectrum + waterfall (bottom layer)
            SpectrumWaterfallView(
                model: model,
                minDb: $m.minDb,
                maxDb: $m.maxDb
            )
            // 2. Frequency / dB grid + labels. Non-hit-testing
            //    so clicks pass through to the VFO overlay above.
            SpectrumGridView(model: model)
            // 3. VFO band + center tick + click-to-tune. On top
            //    so its DragGesture captures clicks. The grid
            //    underneath renders behind the translucent VFO
            //    band — same layering as SDR++ / the GTK UI.
            VfoOverlayView(model: model)
        }
        .frame(minHeight: 300)
    }
}
