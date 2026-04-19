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
                        Text("\(Int(model.squelchDb)) dB")
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
