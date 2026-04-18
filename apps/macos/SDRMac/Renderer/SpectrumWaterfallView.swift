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
// SdrCoreKit is not imported yet — sub-PR 3 wires the real
// engine. Keeping the import out for now means a cleaner review
// of what's actually used.

struct SpectrumWaterfallView: NSViewRepresentable {
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
        let view = MetalSpectrumNSView(renderer: renderer)
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
