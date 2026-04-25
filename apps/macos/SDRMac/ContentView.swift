//
// ContentView.swift — top-level layout with activity-bar
// sidebar redesign (epic #441, sub-ticket #442).
//
// Layout mirrors the GTK VS Code-style activity-bar pattern:
//
//   ┌─────────────────────────────────────────────────────────┐
//   │ [L] │ [L panel] │ spectrum + status + [old flyouts] │ [R panel] │ [R] │
//   └─────────────────────────────────────────────────────────┘
//      ↑       ↑                 ↑                      ↑        ↑
//    left   optional          detail column          optional  right
//   activity left panel       (existing content)     right     activity
//    bar      (per-selection)                        panel      bar
//
// Scaffolding phase (this ticket): all left/right panels are
// `ComingSoonPanel` placeholders pointing at the follow-up
// sub-tickets that port their real content (#443–#447, #448).
// The old flyouts (Transcription, Bookmarks) and the RR sheet
// stay toolbar-driven — #448 consolidates the right side,
// which may remove or relocate the existing flyouts.

import AppKit
import SwiftUI

struct ContentView: View {
    @Environment(CoreModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase

    /// Appearance override applied via `.preferredColorScheme(_:)`
    /// at the root. Read directly from `UserDefaults` (same key
    /// the Display panel writes) — `@AppStorage` here would set
    /// up two write paths and diverge if the Display picker and
    /// this binding ever fired in the same tick. Per #446.
    @AppStorage("SDRMac.appearance") private var rawAppearance: String = "system"

    // ----------------------------------------------------------
    //  Pre-redesign toolbar-driven surfaces — preserved as-is
    //  during scaffolding. #448 may relocate the right flyouts
    //  into the activity-bar-driven `rightSelection` panel.
    // ----------------------------------------------------------

    /// Sheet state lives up here (not inside the toolbar)
    /// because the subview-wrapped version of the RR button
    /// didn't render in the toolbar — inlining the button in
    /// the ToolbarItem closure did. Hoisting `@State` here
    /// keeps the toolbar closure flat while letting `.sheet`
    /// attach to a plain View that renders the dialog.
    @State private var showingRadioReference: Bool = false

    // ----------------------------------------------------------
    //  Activity-bar selections + width drag — sidebar session
    //
    //  Selection / open / width all live on CoreModel
    //  (`sidebarLeft*` / `sidebarRight*`) so the activity-bar
    //  click handler and the resize gesture write straight to
    //  the shared `sdr-config` JSON via the engine FFI.
    //  CoreModel restores from config in `bootstrap()` BEFORE
    //  this view first paints, so the @Observable bindings
    //  here pick up the persisted state without flashing the
    //  default first. Per #449.
    //
    //  `liveLeftWidth` / `liveRightWidth` track the live drag
    //  position separately so we don't write to the shared
    //  config on every pixel. The drag-end commit pushes the
    //  final value to `model.setSidebar*Width` in one shot.
    // ----------------------------------------------------------

    /// Live drag width for the left panel — `nil` when no drag
    /// is in progress (the model's `sidebarLeftWidth` drives
    /// the layout in that case). Set on drag start, updated on
    /// drag, cleared + flushed to the model on drag end.
    @State private var liveLeftWidth: CGFloat? = nil
    /// Same pattern for the right panel.
    @State private var liveRightWidth: CGFloat? = nil

    var body: some View {
        @Bindable var model = model
        let leftSelectionBinding = Binding<LeftActivity>(
            get: { LeftActivity(rawValue: model.sidebarLeftSelected) ?? .general },
            set: { model.setSidebarLeftSelected($0.rawValue) }
        )
        let leftOpenBinding = Binding<Bool>(
            get: { model.sidebarLeftOpen },
            set: { model.setSidebarLeftOpen($0) }
        )
        let rightSelectionBinding = Binding<RightActivity>(
            get: { RightActivity(rawValue: model.sidebarRightSelected) ?? .transcript },
            set: { model.setSidebarRightSelected($0.rawValue) }
        )
        let rightOpenBinding = Binding<Bool>(
            get: { model.sidebarRightOpen },
            set: { model.setSidebarRightOpen($0) }
        )
        let leftWidth = liveLeftWidth ?? CGFloat(model.sidebarLeftWidth)
        let rightWidth = liveRightWidth ?? CGFloat(model.sidebarRightWidth)

        return HStack(spacing: 0) {
            // Left activity bar — always visible.
            ActivityBarView(
                selection: leftSelectionBinding,
                isOpen: leftOpenBinding,
                shortcutModifiers: .command
            )
            Divider()

            // Left panel — visible only when `sidebarLeftOpen`.
            // The remembered selection stays put when closed
            // so a re-open snaps back to the same panel.
            if model.sidebarLeftOpen {
                LeftPanelHost(activity: leftSelectionBinding.wrappedValue)
                    .frame(width: leftWidth)
                resizeHandle(side: .left)
            }

            // Detail column — spectrum + status bar.
            VStack(spacing: 0) {
                CenterView()
                StatusBar()
            }
            .frame(maxWidth: .infinity)

            // Right panel — driven by the right activity bar.
            if model.sidebarRightOpen {
                resizeHandle(side: .right)
                RightPanelHost(
                    activity: rightSelectionBinding.wrappedValue,
                    isOpen: rightOpenBinding
                )
                .frame(width: rightWidth)
            }
            Divider()

            // Right activity bar — always visible.
            ActivityBarView(
                selection: rightSelectionBinding,
                isOpen: rightOpenBinding,
                shortcutModifiers: [.command, .shift]
            )
        }
        // Display panel's Appearance picker writes the same
        // UserDefaults key; this read drives the actual
        // window-wide override. Per #446.
        .preferredColorScheme((Appearance(rawValue: rawAppearance) ?? .system).colorScheme)
        .toolbar {
            HeaderToolbar(showingRadioReference: $showingRadioReference)
        }
        // Pre-redesign mutual exclusivity between the
        // Transcription and Bookmarks toolbar buttons, plus
        // the bookmarks UserDefaults persistence, both went
        // away in #448. The right activity bar's selection
        // is inherently single-valued (one panel open at a
        // time), and #449 will handle session persistence
        // for `rightSelection` + `rightPanelOpen` via the
        // shared sdr-config keys.
        .sheet(isPresented: $showingRadioReference) {
            RadioReferenceDialog()
        }
        // Re-sync the RadioReference credentials flag whenever
        // the main window becomes active. Handles the case where
        // something outside the app's Settings flow changed the
        // keychain (Keychain Access, another process, another
        // build of the app) — the next time the user focuses
        // this window, the toolbar reflects reality.
        //
        // The Settings save flow ALSO updates the flag directly,
        // so in the happy path this is a no-op double-check. If
        // cross-scene `@Observable` propagation ever drops an
        // update, scenePhase change acts as the safety net.
        //
        // Re-probe the USB bus for RTL-SDR hardware on refocus
        // as a safety-net fallback alongside the live IOKit
        // hotplug monitor wired in `CoreModel.bootstrap()`.
        // The monitor delivers plug/unplug events immediately
        // in the normal case (closed issue #363); this hook
        // catches edge cases where the monitor might miss a
        // transition (OS sleep/wake with a dongle swap,
        // notification port restarted underneath us). Cheap
        // enough to keep even if redundant.
        .onChange(of: scenePhase) { _, newPhase in
            if newPhase == .active {
                model.refreshRadioReferenceCredentialsFlag()
                model.refreshDeviceInfo()
            }
        }
        // Fatal ABI-mismatch modal. The binding's setter is a
        // no-op so dismissing the alert is impossible — the
        // only action is Quit. Matches the spec ("fail launch
        // with a dialog, since nothing else will work") in
        // `2026-04-12-swift-ui-surface-design.md`.
        .alert(
            "SDR engine version mismatch",
            isPresented: Binding(
                get: { model.abiMismatch != nil },
                set: { _ in }
            ),
            presenting: model.abiMismatch
        ) { _ in
            Button("Quit", role: .destructive) {
                NSApplication.shared.terminate(nil)
            }
        } message: { mismatch in
            Text("""
                This build of SDR was compiled against engine \
                ABI \(mismatch.compiled.major).\(mismatch.compiled.minor), \
                but the linked library reports \
                \(mismatch.runtime.major).\(mismatch.runtime.minor). \
                A major-version difference means the Swift side \
                and the Rust engine disagree on fundamental data \
                layouts; running anyway would crash or produce \
                bad output. Reinstall a matching build.
                """)
        }
    }

    /// Which sidebar a resize handle controls. Drag direction
    /// is inverted between sides — dragging the left handle
    /// rightward grows the left panel, while dragging the
    /// right handle leftward grows the right panel.
    private enum Side {
        case left, right
    }

    /// Visible 1 px divider wrapped in an 8 px invisible hit
    /// target. Mirrors the GTK `build_resize_handle` pattern
    /// (custom gesture on a narrow strip — `HSplitView` would
    /// also work but takes over more layout decisions than we
    /// want here, since the panels and bars are part of the
    /// same `HStack`).
    ///
    /// **Color.white.opacity(0.001) over Color.clear** —
    /// SwiftUI treats a `.clear` fill as non-hit-testable even
    /// with an explicit `contentShape`. A near-zero-opacity
    /// fill is visually identical but draws (and therefore
    /// hits) normally. This is the documented SwiftUI
    /// workaround — any opacity > 0 will do; we pick 0.001 to
    /// stay inside the "no pixels drawn" optimization. Kept as
    /// a constant on the view so a future Retina cap or
    /// cursor-snap tweak stays in one place.
    ///
    /// Live-drag width is held in `liveLeftWidth` /
    /// `liveRightWidth` so we don't write to the shared config
    /// on every pixel; the on-drag-end commit pushes the final
    /// value through the model setter (which clamps + writes
    /// to the FFI config).
    private func resizeHandle(side: Side) -> some View {
        let baseWidth: CGFloat = side == .left
            ? CGFloat(model.sidebarLeftWidth)
            : CGFloat(model.sidebarRightWidth)
        // 8 px of draggable strip centered on the 1 px divider.
        // Wider than the divider so the hit target is forgiving
        // — matches macOS's own HSplitView separator feel.
        let hitWidth: CGFloat = 8
        return Color.white.opacity(0.001)
            .frame(width: hitWidth)
            .overlay(
                // The visible 1 px line. Uses the system
                // separator color so the divider looks native
                // in both Light and Dark modes and tracks any
                // accent-tint changes the user makes.
                Rectangle()
                    .fill(Color(nsColor: .separatorColor))
                    .frame(width: 1)
            )
            .contentShape(Rectangle())
            .onHover { inside in
                // Push/pop gives stable cursor behavior when
                // the drag crosses above / below other views
                // — a `set`-only approach can leave the resize
                // cursor stuck after a fast exit.
                if inside {
                    NSCursor.resizeLeftRight.push()
                } else {
                    NSCursor.pop()
                }
            }
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { value in
                        let delta = value.translation.width
                        let next: CGFloat
                        switch side {
                        case .left:
                            next = baseWidth + delta
                        case .right:
                            next = baseWidth - delta
                        }
                        // Clamp to the model's configured range
                        // so the live preview stays inside the
                        // visible bounds (setter clamps too).
                        let lo = CGFloat(CoreModel.sidebarWidthRange.lowerBound)
                        let hi = CGFloat(CoreModel.sidebarWidthRange.upperBound)
                        let clamped = min(max(next, lo), hi)
                        switch side {
                        case .left: liveLeftWidth = clamped
                        case .right: liveRightWidth = clamped
                        }
                    }
                    .onEnded { _ in
                        // Commit the final width to CoreModel
                        // (which writes to the shared config).
                        switch side {
                        case .left:
                            if let w = liveLeftWidth {
                                model.setSidebarLeftWidth(UInt32(w.rounded()))
                            }
                            liveLeftWidth = nil
                        case .right:
                            if let w = liveRightWidth {
                                model.setSidebarRightWidth(UInt32(w.rounded()))
                            }
                            liveRightWidth = nil
                        }
                    }
            )
    }
}

#Preview {
    ContentView()
        .environment(CoreModel())
}
