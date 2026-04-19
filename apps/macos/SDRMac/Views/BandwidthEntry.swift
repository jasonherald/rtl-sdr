//
// BandwidthEntry.swift — compact text field + demod-aware
// preset picker for the Radio sidebar's bandwidth row.
//
// The plain `TextField` + `.formatted(.number)` it replaces
// forced the user to type an exact Hz value every time — fine
// for precision but hostile to the common case of "give me
// standard WFM broadcast", which every SDR app has as a preset.
//
// This view wraps the same permissive parse rules as the big
// `FrequencyEntry` (accepts "200k", "12.5kHz", "2.4M", etc.)
// plus a pop-up menu of bandwidths per demod mode:
//
//   WFM: 150k / 200k / 250k
//   NFM: 6.25k / 12.5k / 25k
//   AM:  3k / 6k / 9k / 10k
//   SSB: 1.8k / 2.4k / 2.7k / 3k           (USB & LSB share)
//   DSB: 6k / 10k / 16k
//   CW:  100 / 250 / 500 / 1k
//   RAW: no presets (user picks raw bandwidth)
//
// Presets come from typical amateur / broadcast bandwidths — a
// future GitHub issue can surface them in Settings if users
// want to customize. For v1 the list is hardcoded and matches
// what the GTK panel offers.

import SwiftUI
import SdrCoreKit

struct BandwidthEntry: View {
    @Binding var hz: Double
    let mode: DemodMode
    var commit: (Double) -> Void

    @State private var text: String = ""
    @FocusState private var focused: Bool

    var body: some View {
        HStack(spacing: 2) {
            TextField("Hz", text: $text)
                .textFieldStyle(.roundedBorder)
                .monospacedDigit()
                .focused($focused)
                .onAppear { text = formatRate(hz) }
                .onChange(of: hz) { _, new in
                    if !focused { text = formatRate(new) }
                }
                .onChange(of: mode) { _, _ in
                    // Mode switched — don't auto-pick a preset
                    // (the engine handles that on its side), but
                    // re-render in case units cross a threshold
                    // after the model's bandwidth settles.
                    if !focused { text = formatRate(hz) }
                }
                .onSubmit(commitFromField)

            // Chevron-down menu for presets. Hidden entirely for
            // `raw` mode where a canonical preset list doesn't
            // exist.
            if !Self.presets(for: mode).isEmpty {
                Menu {
                    ForEach(Self.presets(for: mode), id: \.self) { preset in
                        Button(formatRate(preset)) {
                            apply(preset)
                        }
                    }
                } label: {
                    Image(systemName: "chevron.down")
                        .imageScale(.small)
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .help("Bandwidth presets for \(mode.label)")
            }
        }
    }

    private func commitFromField() {
        if let v = parseHzFrequency(text), v > 0 {
            apply(v)
        } else {
            // Revert on parse failure — same contract as
            // FrequencyEntry.
            text = formatRate(hz)
        }
    }

    private func apply(_ value: Double) {
        hz = value
        commit(value)
        text = formatRate(value)
    }

    /// Common bandwidths per demod mode. Hz. Ordered narrow →
    /// wide so the menu reads naturally.
    static func presets(for mode: DemodMode) -> [Double] {
        switch mode {
        case .wfm:        return [150_000, 200_000, 250_000]
        case .nfm:        return [6_250, 12_500, 25_000]
        case .am:         return [3_000, 6_000, 9_000, 10_000]
        case .usb, .lsb:  return [1_800, 2_400, 2_700, 3_000]
        case .dsb:        return [6_000, 10_000, 16_000]
        case .cw:         return [100, 250, 500, 1_000]
        case .raw:        return []
        }
    }
}
