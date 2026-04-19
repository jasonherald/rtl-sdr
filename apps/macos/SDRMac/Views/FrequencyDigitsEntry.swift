//
// FrequencyDigitsEntry.swift — the big tuner display in the
// header toolbar. 12 digits grouped as DDD.DDD.DDD.DDD Hz, one
// selected at a time. Ports the GTK widget at
// `crates/sdr-ui/src/header/frequency_selector.rs` so both apps
// feel the same at the spot users interact with most.
//
// ## Interactions
//
// - **Click a digit** → selects it (and focuses the control so
//   keyboard input works).
// - **Scroll wheel over a digit** → steps that digit (up = +,
//   down = −). Doesn't require the control to be focused.
// - **↑ / ↓** → step the SELECTED digit.
// - **← / →** → move the selection one position.
// - **0–9 typed** → sets the selected digit to that value and
//   advances the selection to the next position.
//
// Leading zeros are rendered dimmed so the displayed magnitude
// reads at a glance. Negative / out-of-range values are clamped
// to `[0, 999_999_999_999]` Hz (999.999.999.999 — way past any
// real SDR's tunable range).

import AppKit
import SwiftUI

struct FrequencyDigitsEntry: View {
    @Binding var hz: Double
    var commit: (Double) -> Void

    @State private var digits: [UInt8] = Array(repeating: 0, count: Self.numDigits)
    @State private var selectedIndex: Int = Self.numDigits - 1  // rightmost (Hz) by default
    @FocusState private var focused: Bool

    static let numDigits: Int = 12
    static let maxFrequencyHz: UInt64 = 999_999_999_999

    private static let digitFont: Font = .system(.title3, design: .monospaced)
    private static let separatorFont: Font = .system(.title3, design: .monospaced)

    var body: some View {
        HStack(spacing: 0) {
            ForEach(0..<Self.numDigits, id: \.self) { i in
                digitLabel(at: i)
                // Separator dots after digits 2, 5, 8 — matches
                // the GTK `DDD.DDD.DDD.DDD` grouping.
                if i == 2 || i == 5 || i == 8 {
                    Text(".")
                        .font(Self.separatorFont)
                        .foregroundStyle(.secondary)
                }
            }
        }
        // Horizontal padding sizes the native toolbar pill — the
        // pill hugs the content, so a bit of breathing room
        // inside the HStack translates to a wider, less-cramped
        // pill.
        .padding(.horizontal, 12)
        .padding(.vertical, 2)
        .focusable()
        .focused($focused)
        // Suppress SwiftUI's default focus ring (the blue
        // rectangle that wraps the whole digit row when the
        // control gains keyboard focus). The selected-digit
        // underline is our intentional selection indicator —
        // the focus ring is redundant + clashes with the
        // toolbar pill behind it.
        .focusEffectDisabled()
        // Per-digit scroll wheel step. Consumes scrollWheel only
        // when the cursor is over the frequency control so
        // scrolls elsewhere (spectrum, sidebar) still work.
        .background(
            DigitScrollCatcher { fracX in
                let i = digitIndex(atFraction: fracX)
                selectedIndex = i
                return i
            } onStep: { i, delta in
                step(digit: i, by: delta)
            }
            .allowsHitTesting(false)
        )
        .onAppear { syncDigitsFromHz() }
        .onChange(of: hz) { _, _ in syncDigitsFromHz() }
        .onKeyPress(phases: .down) { handleKey($0) }
        .help("""
            Click a digit and use arrow keys: ↑/↓ step the \
            selected digit, ←/→ move selection, 0–9 set the \
            digit. Scroll wheel also works over any digit.
            """)
        // Accessibility: expose the row as a single adjustable
        // element. VoiceOver reads the current frequency + the
        // selected digit, and increment/decrement drive
        // `step(digit:by:)` on the selected digit. Per-digit
        // access is still available to sighted users via click /
        // arrow keys, but a single-element adjustable is what
        // screen readers expect for a numeric spinner.
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Frequency")
        .accessibilityValue(accessibilityValue)
        .accessibilityHint("Adjustable. Increment or decrement the selected digit.")
        .accessibilityAdjustableAction { direction in
            switch direction {
            case .increment:
                step(digit: selectedIndex, by: +1)
            case .decrement:
                step(digit: selectedIndex, by: -1)
            @unknown default:
                break
            }
        }
        .accessibilityAction(named: "Select next digit") {
            if selectedIndex < Self.numDigits - 1 {
                selectedIndex += 1
            }
        }
        .accessibilityAction(named: "Select previous digit") {
            if selectedIndex > 0 {
                selectedIndex -= 1
            }
        }
    }

    /// VoiceOver-friendly rendering of the current state. Reads
    /// like "100.700 megahertz, selecting 10 kilohertz digit"
    /// so the user knows both the value and what the next
    /// increment/decrement will affect.
    private var accessibilityValue: String {
        let freq = formatRate(hz)
            .replacingOccurrences(of: "MHz", with: "megahertz")
            .replacingOccurrences(of: "kHz", with: "kilohertz")
            .replacingOccurrences(of: "Hz", with: "hertz")
        let stepSize = Self.digitStep(selectedIndex)
        let stepLabel: String
        switch stepSize {
        case 1_000_000_000...: stepLabel = "\(stepSize / 1_000_000_000) gigahertz"
        case 1_000_000...:     stepLabel = "\(stepSize / 1_000_000) megahertz"
        case 1_000...:         stepLabel = "\(stepSize / 1_000) kilohertz"
        default:               stepLabel = "\(stepSize) hertz"
        }
        return "\(freq), selecting \(stepLabel) digit"
    }

    // ----------------------------------------------------------
    //  Digit rendering
    // ----------------------------------------------------------

    private func digitLabel(at i: Int) -> some View {
        let firstNonzero = digits.firstIndex(where: { $0 != 0 }) ?? Self.numDigits
        let isLeading = i < firstNonzero && i != Self.numDigits - 1
        // Selection is independent of keyboard focus — the user
        // who clicks a digit and then moves the mouse still has
        // a meaningful "this is the target digit" expectation.
        // Paint the selected digit in the system accent color
        // (bold weight too) so it reads at a glance without any
        // background chrome. Dim when the control doesn't have
        // keyboard focus, brighten when focused, so "typing lands
        // here" vs "click first" is still distinguishable.
        // Closes #328.
        let isSelected = i == selectedIndex

        let digitColor: Color
        if isSelected {
            digitColor = focused
                ? .accentColor
                : .accentColor.opacity(0.65)
        } else if isLeading {
            digitColor = .secondary.opacity(0.6)
        } else {
            digitColor = .primary
        }

        return Text("\(digits[i])")
            .font(Self.digitFont)
            .fontWeight(isSelected ? .bold : .regular)
            .foregroundStyle(digitColor)
            .frame(minWidth: 12)
            .contentShape(Rectangle())
            .onTapGesture {
                selectedIndex = i
                focused = true
            }
    }

    // ----------------------------------------------------------
    //  Key handling
    // ----------------------------------------------------------

    private func handleKey(_ press: KeyPress) -> KeyPress.Result {
        switch press.key {
        case .upArrow:
            step(digit: selectedIndex, by: +1)
            return .handled
        case .downArrow:
            step(digit: selectedIndex, by: -1)
            return .handled
        case .leftArrow:
            if selectedIndex > 0 { selectedIndex -= 1 }
            return .handled
        case .rightArrow:
            if selectedIndex < Self.numDigits - 1 { selectedIndex += 1 }
            return .handled
        default:
            break
        }
        // Digit keys 0-9. `press.characters` includes the typed
        // string form (including keypad digits).
        if let ch = press.characters.first,
           let n = ch.wholeNumberValue,
           (0...9).contains(n)
        {
            setDigit(UInt8(n))
            return .handled
        }
        return .ignored
    }

    // ----------------------------------------------------------
    //  State mutations
    // ----------------------------------------------------------

    private func syncDigitsFromHz() {
        let clamped = UInt64(max(0, min(Double(Self.maxFrequencyHz), hz)))
        digits = Self.freqToDigits(clamped)
    }

    /// Step the frequency by ±(10^position). Positive delta =
    /// increase, negative = decrease. Clamped to the valid range.
    ///
    /// Clamp `hz` into range BEFORE converting, not just after.
    /// If a caller set an out-of-range value externally (e.g. a
    /// tune command that the engine clamps silently), the base
    /// we step from must be the clamped value, not the stale
    /// binding — otherwise `+1` on an already-too-big number
    /// produces garbage. Per #327 review.
    private func step(digit position: Int, by delta: Int) {
        let stepHz = Self.digitStep(position)
        let current = UInt64(
            max(0, min(Double(Self.maxFrequencyHz), hz))
        )
        let new: UInt64
        if delta > 0 {
            // Plain `+` (not `&+`) so an unforeseen overflow
            // traps loudly instead of wrapping silently. The
            // `min` above guarantees we can't actually overflow
            // a UInt64 here — maxFrequencyHz + 100_000_000_000
            // is nowhere near UInt64.max.
            new = min(Self.maxFrequencyHz, current + stepHz)
        } else {
            new = current >= stepHz ? current - stepHz : 0
        }
        commitFrequency(new)
    }

    /// Set the selected digit to `value` and advance selection.
    private func setDigit(_ value: UInt8) {
        var d = digits
        d[selectedIndex] = value
        let new = min(Self.maxFrequencyHz, Self.digitsToFreq(d))
        commitFrequency(new)
        if selectedIndex < Self.numDigits - 1 {
            selectedIndex += 1
        }
    }

    private func commitFrequency(_ newFreq: UInt64) {
        let f = Double(newFreq)
        digits = Self.freqToDigits(newFreq)
        // Skip the engine round-trip when the frequency is
        // unchanged. Hitting the clamp floor / ceiling or
        // re-entering a digit with the same value would
        // otherwise spam duplicate `setCenter(...)` calls onto
        // the DSP command channel. Per #327 review.
        guard hz != f else { return }
        hz = f
        commit(f)
    }

    // ----------------------------------------------------------
    //  Hit-test helper for scroll wheel
    // ----------------------------------------------------------

    /// Map a 0..1 fraction across the control's width to a
    /// digit index. Approximate — assumes uniform digit widths
    /// and ignores separator dots. Close enough for scroll
    /// intent; click-to-select uses `.onTapGesture` on the
    /// digit views directly so it's pixel-accurate.
    ///
    /// Multiply by `numDigits` (not `numDigits - 1`) so each
    /// digit owns an equal 1/12 slice of the width. Using
    /// `numDigits - 1` made the rightmost slice reachable only
    /// at exactly frac == 1.0, which effectively hid the 1 Hz
    /// digit from the scroll path. Per #327 review.
    private func digitIndex(atFraction frac: CGFloat) -> Int {
        let f = max(0, min(1, frac))
        let idx = Int(f * CGFloat(Self.numDigits))
        return max(0, min(Self.numDigits - 1, idx))
    }

    // ----------------------------------------------------------
    //  Digit arithmetic (ported from frequency_selector.rs)
    // ----------------------------------------------------------

    static func freqToDigits(_ freq: UInt64) -> [UInt8] {
        var digits = Array(repeating: UInt8(0), count: numDigits)
        var remaining = min(freq, maxFrequencyHz)
        for i in stride(from: numDigits - 1, through: 0, by: -1) {
            digits[i] = UInt8(remaining % 10)
            remaining /= 10
        }
        return digits
    }

    static func digitsToFreq(_ digits: [UInt8]) -> UInt64 {
        var freq: UInt64 = 0
        for d in digits {
            freq = freq * 10 + UInt64(d)
        }
        return freq
    }

    /// Step size (Hz) for each digit position. Module-private
    /// static so key-repeat / scroll events don't re-allocate a
    /// 12-entry array on every invocation.
    /// Position 0 = 100 GHz (10^11); position 11 = 1 Hz (10^0).
    private static let digitStepTable: [UInt64] = [
        100_000_000_000,  // 100 GHz
        10_000_000_000,   // 10 GHz
        1_000_000_000,    // 1 GHz
        100_000_000,      // 100 MHz
        10_000_000,       // 10 MHz
        1_000_000,        // 1 MHz
        100_000,          // 100 kHz
        10_000,           // 10 kHz
        1_000,             // 1 kHz
        100,              // 100 Hz
        10,               // 10 Hz
        1,                // 1 Hz
    ]

    /// Step size (Hz) for a digit position.
    /// Position 0 = 100 GHz (10^11); position 11 = 1 Hz (10^0).
    static func digitStep(_ position: Int) -> UInt64 {
        precondition(position >= 0 && position < numDigits,
                     "digit position out of range")
        return digitStepTable[position]
    }
}

// ============================================================
//  DigitScrollCatcher — NSEvent local monitor for per-digit
//  scroll-wheel stepping.
// ============================================================
//
//  Same pattern as `ScrollWheelZoomCatcher` in CenterView but
//  with a finer-grained callback: instead of one zoom delta, we
//  pass (digit_index, +1/-1) so the parent view can route each
//  event to the right digit.

private struct DigitScrollCatcher: NSViewRepresentable {
    /// Map the cursor's 0..1 fractional x-position across the
    /// view to a digit index. Called on each scroll event.
    let digitAtFraction: (CGFloat) -> Int
    /// Called with the chosen digit + step direction (+1 up,
    /// -1 down).
    let onStep: (Int, Int) -> Void

    func makeNSView(context: Context) -> MonitorView {
        MonitorView(digitAtFraction: digitAtFraction, onStep: onStep)
    }

    func updateNSView(_ nsView: MonitorView, context: Context) {
        nsView.digitAtFraction = digitAtFraction
        nsView.onStep = onStep
    }

    final class MonitorView: NSView {
        var digitAtFraction: (CGFloat) -> Int
        var onStep: (Int, Int) -> Void
        private var monitor: Any?

        init(
            digitAtFraction: @escaping (CGFloat) -> Int,
            onStep: @escaping (Int, Int) -> Void
        ) {
            self.digitAtFraction = digitAtFraction
            self.onStep = onStep
            super.init(frame: .zero)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) {
            fatalError("DigitScrollCatcher.MonitorView does not support NSCoder init")
        }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if window != nil {
                startMonitor()
            } else {
                stopMonitor()
            }
        }

        deinit {
            stopMonitor()
        }

        private func startMonitor() {
            guard monitor == nil else { return }
            monitor = NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) { [weak self] event in
                guard let self, let window = self.window,
                      event.window === window else { return event }
                let locInView = self.convert(event.locationInWindow, from: nil)
                guard self.bounds.contains(locInView) else { return event }
                // Skip the trackpad momentum tail — same reason
                // as CenterView's zoom: a flick would advance 40
                // digits.
                if !event.momentumPhase.isEmpty {
                    return event
                }
                let fracX = self.bounds.width > 0
                    ? locInView.x / self.bounds.width
                    : 0.5
                let idx = self.digitAtFraction(fracX)
                // Horizontal-only trackpad gestures still
                // deliver scrollWheel events with dy == 0.
                // Treating 0 as "negative" would decrement the
                // digit on every stray horizontal swipe; skip
                // those cleanly. Per #327 review.
                let dy = event.scrollingDeltaY
                guard dy != 0 else { return event }
                // dy > 0 on trackpad = natural scroll up, i.e.
                // content moves up, meaning the user wants to
                // see larger numbers. Step +1.
                let direction = dy > 0 ? 1 : -1
                self.onStep(idx, direction)
                return nil
            }
        }

        private func stopMonitor() {
            if let m = monitor {
                NSEvent.removeMonitor(m)
                monitor = nil
            }
        }
    }
}
