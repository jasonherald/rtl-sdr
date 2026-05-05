//
// RadioPanelView.swift — Radio activity panel (closes #444).
//
// Six flat Sections matching the GTK
// `crates/sdr-ui/src/sidebar/radio_panel.rs` layout:
//
//   - Bandwidth     — channel filter width
//   - Squelch       — mute below threshold
//   - Filters       — noise blanker / FM IF NR / WFM stereo /
//                     notch (collapsed-Disclosure removed —
//                     flat layout matches the GTK decision)
//   - De-emphasis   — FM-only restore for high-frequency audio
//   - Distance      — FSPL distance estimator (#486 / Mac
//                     parity for #164)
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
            DistanceEstimatorSection()
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
                HStack(spacing: 8) {
                    @Bindable var m = model
                    BandwidthEntry(
                        hz: $m.bandwidthHz,
                        mode: model.demodMode
                    ) { hz in
                        model.setBandwidth(hz)
                    }
                    // Trailing per-row reset to mode default.
                    // Enabled only when the current bandwidth
                    // differs from `demodMode.defaultBandwidthHz`
                    // — switching demod modes re-evaluates so
                    // the icon flips state automatically when
                    // the user picks a new mode whose default
                    // matches (or doesn't match) the current
                    // bandwidth. Per #488. Routes through the
                    // engine setter so the echo round-trips
                    // through `BandwidthChanged` and the
                    // observable model stays an event consumer.
                    Button {
                        model.resetBandwidthToModeDefault()
                    } label: {
                        Image(systemName: "arrow.counterclockwise")
                    }
                    .buttonStyle(.borderless)
                    .disabled(model.isBandwidthAtModeDefault)
                    .help("Reset bandwidth to \(model.demodMode.label) default")
                    .accessibilityLabel("Reset bandwidth to mode default")
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
//  Distance estimator — FSPL (#486 / #164 Mac parity)
// ============================================================

private struct DistanceEstimatorSection: View {
    @Environment(CoreModel.self) private var model

    /// User-facing input mode: dBm directly, or watts (auto-
    /// converted to dBm via `Propagation.wattsToDbm`).
    @State private var unit: PowerUnit = .watts

    /// Watts text-field buffer. Bound to a String so partial
    /// edits (e.g. "5.") don't round-trip through Double in
    /// the middle of typing. Synced from `model.fsplErpDbm`
    /// on appear and unit switch.
    @State private var wattsText: String = ""

    /// dBm text-field buffer. Same edit-buffer reasoning as
    /// `wattsText`.
    @State private var dbmText: String = ""

    private enum PowerUnit: String, CaseIterable, Identifiable {
        case watts
        case dbm
        var id: String { rawValue }
        var label: String {
            switch self {
            case .watts: return "W"
            case .dbm:   return "dBm"
            }
        }
    }

    var body: some View {
        Section {
            // ---- TX power input row -------------------------
            LabeledContent("TX power") {
                HStack(spacing: 6) {
                    if unit == .watts {
                        TextField("Watts", text: $wattsText)
                            .textFieldStyle(.roundedBorder)
                            .frame(maxWidth: 80)
                            .onSubmit(commitWatts)
                    } else {
                        TextField("dBm", text: $dbmText)
                            .textFieldStyle(.roundedBorder)
                            .frame(maxWidth: 80)
                            .onSubmit(commitDbm)
                    }
                    Picker("", selection: $unit) {
                        ForEach(PowerUnit.allCases) { u in
                            Text(u.label).tag(u)
                        }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .frame(width: 100)
                }
            }

            // ---- Received signal level (read-only) ----------
            LabeledContent("Received") {
                Text(receivedDisplay)
                    .font(.body.monospacedDigit())
                    .foregroundStyle(.secondary)
            }

            // ---- Computed distance --------------------------
            LabeledContent("Distance") {
                Text(distanceDisplay)
                    .font(.body.monospacedDigit())
            }
        } header: {
            Text("Distance estimator")
        } footer: {
            Text("Free-space path loss — assumes line-of-sight, no terrain or multipath. Read distances as an upper bound, not a ranging measurement.")
                .font(.caption)
        }
        .onAppear { syncTextFields() }
        .onChange(of: unit) { _, _ in syncTextFields() }
        .onChange(of: model.fsplErpDbm) { _, _ in syncTextFields() }
    }

    // ----------------------------------------------------------
    //  Display strings
    // ----------------------------------------------------------

    /// Received signal level — sourced from the same engine
    /// event that drives the status-bar signal meter, so this
    /// row updates live as the AGC/signal level changes. Shown
    /// as "—" when squelch is closed (signal level still
    /// reports the floor, but estimating distance from a
    /// not-currently-locked signal is misleading).
    private var receivedDisplay: String {
        if !signalIsLocked {
            return "—"
        }
        return String(format: "%.0f dBm", Double(model.signalLevelDb))
    }

    /// Computed distance — fed by `Propagation.fsplDistanceMeters`
    /// with the current ERP, received level, and tuned frequency.
    /// Returns "—" when squelch is closed (no live signal to
    /// measure against), when the math returns NaN (non-physical
    /// inputs), or zero (received ≥ TX, calibration issue).
    private var distanceDisplay: String {
        guard signalIsLocked else { return "—" }
        let d = Propagation.fsplDistanceMeters(
            erpDbm: model.fsplErpDbm,
            receivedDbm: Double(model.signalLevelDb),
            frequencyHz: model.centerFrequencyHz
        )
        if !d.isFinite || d <= 0 { return "—" }
        return Propagation.formatDistance(d)
    }

    /// True when the receiver is hearing a usable signal — used
    /// to gate the distance display so a user looking at noise
    /// doesn't see "estimated 12,000 km" on whatever the floor
    /// happens to be. With squelch on, a closed-squelch state is
    /// the obvious "no signal" indicator. With squelch off we
    /// can't tell, so we fall back to a fixed -100 dBm threshold
    /// — anything above is plausibly a real signal worth
    /// measuring.
    private var signalIsLocked: Bool {
        if model.squelchEnabled {
            return Double(model.signalLevelDb) > Double(model.squelchDb)
        }
        return Double(model.signalLevelDb) > -100
    }

    // ----------------------------------------------------------
    //  Edit buffer ↔ model sync
    // ----------------------------------------------------------

    private func syncTextFields() {
        let watts = Propagation.dbmToWatts(model.fsplErpDbm)
        wattsText = String(format: "%.2f", watts)
        dbmText = String(format: "%.1f", model.fsplErpDbm)
    }

    private func commitWatts() {
        if let w = Double(wattsText), w > 0 {
            model.setFsplErpDbm(Propagation.wattsToDbm(w))
        } else {
            // Revert on parse failure.
            wattsText = String(format: "%.2f", Propagation.dbmToWatts(model.fsplErpDbm))
        }
    }

    private func commitDbm() {
        if let d = Double(dbmText), d.isFinite {
            model.setFsplErpDbm(d)
        } else {
            dbmText = String(format: "%.1f", model.fsplErpDbm)
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
