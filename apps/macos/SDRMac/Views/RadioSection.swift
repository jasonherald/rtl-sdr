//
// RadioSection.swift — sidebar panel for demod controls.
//
// MVP: bandwidth, squelch, de-emphasis (WFM/NFM only), volume.
// Advanced (noise blanker, FM IF NR, WFM stereo, notch) lives
// in a collapsible DisclosureGroup at the bottom — issue #245.
//
// Mode-gating rules mirror the GTK UI:
//   - FM IF NR visible in WFM / NFM only
//   - WFM stereo visible in WFM only
//   - Noise blanker + notch are universal

import SwiftUI
import SdrCoreKit

struct RadioSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("Radio") {
            LabeledContent("Bandwidth") {
                @Bindable var m = model
                BandwidthEntry(
                    hz: $m.bandwidthHz,
                    mode: model.demodMode
                ) { hz in
                    model.setBandwidth(hz)
                }
            }

            Toggle("Squelch", isOn: Binding(
                get: { model.squelchEnabled },
                set: { model.setSquelchEnabled($0) }
            ))

            if model.squelchEnabled {
                // Auto-squelch tracks the noise floor and writes
                // the threshold. Only visible alongside the main
                // squelch toggle — the feature has no meaning
                // when squelch itself is off.
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
                            // `onEditingChanged` fires on BOTH drag
                            // start (editing=true) and drag end
                            // (editing=false). Commit only on drag
                            // end, otherwise we'd fire an engine
                            // command at the instant the user
                            // touches the slider — with the old
                            // value, before their drag has moved
                            // it — then a second one on release.
                            onEditingChanged: { editing in
                                if !editing {
                                    model.setSquelchDb(model.squelchDb)
                                }
                            }
                        )
                        // Slider is disabled while auto-squelch
                        // owns the threshold — letting the user
                        // drag against a value that re-sets at
                        // ~50 Hz is confusing UX. The label still
                        // shows the live auto-picked value.
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

            if model.demodMode == .wfm || model.demodMode == .nfm {
                Picker("De-emphasis", selection: Binding(
                    get: { model.deemphasis },
                    set: { model.setDeemphasis($0) }
                )) {
                    Text("None").tag(Deemphasis.none)
                    Text("US 75µs").tag(Deemphasis.us75)
                    Text("EU 50µs").tag(Deemphasis.eu50)
                }
                .pickerStyle(.segmented)
            }

            LabeledContent("Volume") {
                @Bindable var m = model
                Slider(
                    value: $m.volume,
                    in: 0...1,
                    onEditingChanged: { editing in
                        if !editing {
                            model.setVolume(model.volume)
                        }
                    }
                )
            }

            DisclosureGroup("Advanced") {
                AdvancedDemodControls()
            }
        }
    }
}

/// Advanced demod controls — noise blanker, FM IF NR, WFM
/// stereo, notch filter. Pulled into its own subview so
/// the RadioSection body stays under SwiftUI's expression
/// complexity budget as more controls accumulate.
private struct AdvancedDemodControls: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Toggle("Noise blanker", isOn: Binding(
            get: { model.noiseBlankerEnabled },
            set: { model.setNoiseBlankerEnabled($0) }
        ))

        if model.noiseBlankerEnabled {
            LabeledContent("NB level") {
                VStack(spacing: 2) {
                    @Bindable var m = model
                    // Range mirrors the GTK slider (1.0...10.0).
                    // Engine rejects < 1.0 via InvalidArg, so the
                    // lower bound matches — never push a value
                    // the FFI will reject.
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
        // the toggle outside WFM / NFM so the user can't arm a
        // control that the engine will silently ignore.
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
                    // Voice-band default range. The engine
                    // clamps to audio Nyquist internally; this
                    // range stays well below it for any
                    // realistic audio sample rate, so the host
                    // UI doesn't have to chase engine state.
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
    }
}
