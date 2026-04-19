//
// RadioSection.swift — sidebar panel for demod controls.
//
// MVP: bandwidth, squelch, de-emphasis (WFM/NFM only), volume.
// The advanced controls (noise blanker, FM IF NR, WFM stereo,
// notch) are v2.

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
        }
    }
}
