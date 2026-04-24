//
// RadioPanelView.swift — Radio activity panel (closes #444).
//
// Five flat Sections matching the GTK
// `crates/sdr-ui/src/sidebar/radio_panel.rs` layout:
//
//   - Bandwidth     — channel filter width
//   - Squelch       — mute below threshold
//   - Filters       — noise blanker / FM IF NR / WFM stereo /
//                     notch (collapsed-Disclosure removed —
//                     flat layout matches the GTK decision)
//   - De-emphasis   — FM-only restore for high-frequency audio
//   - CTCSS         — tone-squelch (Mac wiring TBD; placeholder
//                     section explaining the gap)
//
// Volume moves to the Audio panel (#445); it lived here in
// the pre-redesign sidebar but conceptually belongs with
// output device + recording, not with demod mute behavior.

import SwiftUI
import SdrCoreKit

struct RadioPanelView: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Form {
            BandwidthSection()
            SquelchSection()
            FiltersSection()
            DeemphasisSection()
            CtcssSection()
        }
        .formStyle(.grouped)
    }
}

// ============================================================
//  Bandwidth
// ============================================================

private struct BandwidthSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            LabeledContent("Bandwidth") {
                @Bindable var m = model
                BandwidthEntry(
                    hz: $m.bandwidthHz,
                    mode: model.demodMode
                ) { hz in
                    model.setBandwidth(hz)
                }
            }
        } header: {
            Text("Bandwidth")
        } footer: {
            Text("Filter width around the tuned frequency.")
                .font(.caption)
        }
    }
}

// ============================================================
//  Squelch
// ============================================================

private struct SquelchSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            Toggle("Squelch", isOn: Binding(
                get: { model.squelchEnabled },
                set: { model.setSquelchEnabled($0) }
            ))

            if model.squelchEnabled {
                Toggle("Auto", isOn: Binding(
                    get: { model.autoSquelchEnabled },
                    set: { model.setAutoSquelch($0) }
                ))

                LabeledContent("Threshold") {
                    VStack(spacing: 2) {
                        @Bindable var m = model
                        Slider(
                            value: $m.squelchDb,
                            in: -120...0,
                            onEditingChanged: { editing in
                                if !editing {
                                    model.setSquelchDb(model.squelchDb)
                                }
                            }
                        )
                        .disabled(model.autoSquelchEnabled)
                        Text(
                            model.autoSquelchEnabled
                                ? "Auto: \(Int(model.squelchDb)) dB"
                                : "\(Int(model.squelchDb)) dB"
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    }
                }
            }
        } header: {
            Text("Squelch")
        } footer: {
            Text("Mute audio when the signal is too weak.")
                .font(.caption)
        }
    }
}

// ============================================================
//  Filters — noise blanker / FM IF NR / WFM stereo / notch
// ============================================================

private struct FiltersSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            Toggle("Noise blanker", isOn: Binding(
                get: { model.noiseBlankerEnabled },
                set: { model.setNoiseBlankerEnabled($0) }
            ))

            if model.noiseBlankerEnabled {
                LabeledContent("NB level") {
                    VStack(spacing: 2) {
                        @Bindable var m = model
                        Slider(
                            value: $m.noiseBlankerLevel,
                            in: 1.0...10.0,
                            onEditingChanged: { editing in
                                if !editing {
                                    model.setNoiseBlankerLevel(model.noiseBlankerLevel)
                                }
                            }
                        )
                        Text(String(format: "%.1f×", model.noiseBlankerLevel))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }

            // FM IF NR is only meaningful in FM demod modes; hide
            // the toggle outside WFM / NFM so the user can't arm
            // a control that the engine will silently ignore.
            if model.demodMode == .wfm || model.demodMode == .nfm {
                Toggle("FM IF NR", isOn: Binding(
                    get: { model.fmIfNrEnabled },
                    set: { model.setFmIfNrEnabled($0) }
                ))
            }

            if model.demodMode == .wfm {
                Toggle("WFM stereo", isOn: Binding(
                    get: { model.wfmStereoEnabled },
                    set: { model.setWfmStereo($0) }
                ))
            }

            Toggle("Notch", isOn: Binding(
                get: { model.notchEnabled },
                set: { model.setNotchEnabled($0) }
            ))

            if model.notchEnabled {
                LabeledContent("Notch Hz") {
                    VStack(spacing: 2) {
                        @Bindable var m = model
                        Slider(
                            value: $m.notchFrequencyHz,
                            in: 200...4000,
                            onEditingChanged: { editing in
                                if !editing {
                                    model.setNotchFrequencyHz(model.notchFrequencyHz)
                                }
                            }
                        )
                        Text("\(Int(model.notchFrequencyHz)) Hz")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
        } header: {
            Text("Filters")
        } footer: {
            Text("Clean up interference and noise.")
                .font(.caption)
        }
    }
}

// ============================================================
//  De-emphasis (WFM / NFM only)
// ============================================================

private struct DeemphasisSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        // Conditional section — only rendered for FM demod
        // modes. Matches the GTK panel's mode-dependent
        // visibility for `deemphasis_group`.
        if model.demodMode == .wfm || model.demodMode == .nfm {
            Section {
                Picker("De-emphasis", selection: Binding(
                    get: { model.deemphasis },
                    set: { model.setDeemphasis($0) }
                )) {
                    Text("None").tag(Deemphasis.none)
                    Text("US 75µs").tag(Deemphasis.us75)
                    Text("EU 50µs").tag(Deemphasis.eu50)
                }
                .pickerStyle(.segmented)
            } header: {
                Text("De-emphasis")
            } footer: {
                Text("Restore high-frequency audio on FM. US for North America, EU for Europe.")
                    .font(.caption)
            }
        }
    }
}

// ============================================================
//  CTCSS — tone-squelch (placeholder until Mac CoreModel ships
//  the wiring; see the section footer)
// ============================================================

private struct CtcssSection: View {
    var body: some View {
        Section {
            Text("CTCSS tone squelch is wired on the Linux side via the engine's tone-detection module. Mac CoreModel doesn't surface those bindings yet — coming in a follow-up.")
                .font(.caption)
                .foregroundStyle(.secondary)
        } header: {
            Text("CTCSS")
        } footer: {
            Text("Open audio only when a matching sub-audible tone is present. Useful on shared repeaters.")
                .font(.caption)
        }
    }
}
