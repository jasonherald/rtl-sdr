//
// CenterView.swift — main spectrum + waterfall area.
//
// v1 placeholder: just a solid-color rectangle with a text
// label. The real Metal-backed `SpectrumWaterfallView` lands
// in M4 (see `docs/superpowers/specs/2026-04-12-swift-ui-rendering-design.md`)
// and replaces the body of this view with the `NSViewRepresentable`
// wrapping `SpectrumMTKView`.

import SwiftUI

struct CenterView: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        ZStack {
            Rectangle()
                .fill(Color(nsColor: .windowBackgroundColor))
            VStack(spacing: 8) {
                Image(systemName: "waveform.path.ecg")
                    .font(.system(size: 48))
                    .foregroundStyle(.secondary)
                Text("Spectrum + waterfall")
                    .font(.headline)
                    .foregroundStyle(.secondary)
                Text("(Metal renderer lands in M4)")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
        .frame(minHeight: 300)
    }
}
