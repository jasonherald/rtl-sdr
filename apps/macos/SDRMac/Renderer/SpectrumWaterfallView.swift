//
// SpectrumWaterfallView.swift — SwiftUI wrapper around the
// CAMetalLayer-backed `MetalSpectrumNSView` via
// `NSViewRepresentable`.
//
// `makeNSView` runs once per view lifetime and wires up the
// renderer. `updateNSView` runs on every binding change and
// forwards new values into the persistent NSView. The NSView is
// NOT recreated on every binding update — that would thrash
// Metal resources.

import SwiftUI
import SdrCoreKit

struct SpectrumWaterfallView: NSViewRepresentable {
    /// Source of truth for the engine handle. The renderer pulls
    /// FFT frames via a closure that reads `model.core` each
    /// draw tick. Passed by unowned reference — `CoreModel` is a
    /// reference type and its lifetime is tied to the app root,
    /// so we won't outlive it.
    let model: CoreModel
    @Binding var minDb: Float
    @Binding var maxDb: Float

    func makeNSView(context: Context) -> NSView {
        // Fail-soft: if Metal device creation / shader
        // compilation fails, render a SwiftUI-ish fallback
        // rather than crashing. Extremely rare — only matters
        // on Macs without Metal support, which predate our
        // macOS 14 floor anyway.
        guard let renderer = SpectrumRenderer.make() else {
            return makeFallbackView()
        }
        // Capture the model weakly in the provider closure so a
        // stray retain through the renderer can't outlive
        // CoreModel. CoreModel is @MainActor; the renderer polls
        // the closure on the main runloop (where the display
        // link is scheduled), so the actor-isolated read is
        // safe without an explicit hop.
        let view = MetalSpectrumNSView(renderer: renderer) { [weak model] in
            model?.core
        }
        view.applyBindings(minDb: minDb, maxDb: maxDb)
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {
        (view as? MetalSpectrumNSView)?.applyBindings(minDb: minDb, maxDb: maxDb)
    }

    private func makeFallbackView() -> NSView {
        let view = NSView(frame: .zero)
        let label = NSTextField(labelWithString: "Metal renderer unavailable on this machine.")
        label.textColor = .systemRed
        label.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(label)
        NSLayoutConstraint.activate([
            label.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            label.centerYAnchor.constraint(equalTo: view.centerYAnchor),
        ])
        return view
    }
}
