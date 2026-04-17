//
// SpectrumWaterfallView.swift — SwiftUI wrapper around
// `SpectrumMTKView` via `NSViewRepresentable`.
//
// SwiftUI doesn't host `NSView` subclasses natively;
// `NSViewRepresentable` is the bridge. `makeNSView` runs once
// per view lifetime; `updateNSView` runs on every binding
// change and forwards the new values into the persistent
// MTKView. The MTKView is NOT recreated on every binding update
// — that would thrash Metal resources.

import SwiftUI
// SdrCoreKit is not imported in sub-PR 1 — only the engine
// wiring in sub-PR 3 needs it. Keeping the import out now
// means a cleaner review of what's actually used.

struct SpectrumWaterfallView: NSViewRepresentable {
    @Binding var minDb: Float
    @Binding var maxDb: Float

    func makeNSView(context: Context) -> NSView {
        // Fail-soft: if Metal device creation / shader
        // compilation fails, render a SwiftUI fallback
        // message rather than crashing. Wrapping in an
        // `NSHostingView` would be the pure-SwiftUI path; for
        // simplicity we return a lightweight `NSView` with a
        // red-ish label. Extremely rare failure mode — only
        // happens on Macs without Metal support, which predate
        // our macOS 14 floor anyway.
        guard let mtk = SpectrumMTKView.make() else {
            return makeFallbackView()
        }
        mtk.applyBindings(minDb: minDb, maxDb: maxDb)
        return mtk
    }

    func updateNSView(_ view: NSView, context: Context) {
        guard let mtk = view as? SpectrumMTKView else { return }
        mtk.applyBindings(minDb: minDb, maxDb: maxDb)
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
