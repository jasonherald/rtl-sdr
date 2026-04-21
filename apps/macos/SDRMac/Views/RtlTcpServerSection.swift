//
// RtlTcpServerSection.swift — sidebar panel for sharing a
// locally-connected RTL-SDR dongle as an rtl_tcp server so
// other SDR clients on the LAN (GQRX, SDR++, another sdr-rs
// instance, …) can tune it (#353).
//
// The panel is visible whenever a dongle is detected; the
// "Share over network" toggle itself is disabled whenever the
// engine is currently running on that same dongle, since the
// two paths can't share exclusive USB access. When the server
// is running the form stays read-only (the Rust side applies
// the initial state once on dongle open) but the status rows
// below light up with live client info + an activity log.

import SwiftUI
import SdrCoreKit

/// Sample rates the RTL-SDR tuner supports. Duplicated from
/// `SourceSection.rtlSdrSampleRates` because the original is
/// `private`; a follow-up could hoist it to a shared
/// `SupportedSampleRates` constant if more views need it.
private let rtlSdrSampleRates: [UInt32] = [
    250_000, 1_024_000, 1_536_000, 1_792_000, 1_920_000,
    2_048_000, 2_160_000, 2_400_000, 2_560_000, 2_880_000,
    3_200_000,
]

/// Max lines to render in the activity log — the Rust side's
/// `recent_commands` ring is bounded at 50 too.
private let activityLogMaxRows = 50

struct RtlTcpServerSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section("RTL-TCP server") {
            // Device-presence gate driven by the dedicated
            // `hasLocalRtlSdr` flag on `CoreModel` — the
            // earlier version parsed `deviceInfo`'s wording,
            // which bounced on each post-Play `.deviceInfo`
            // engine event and left the section rendering the
            // full form on first paint (before the probe). Per
            // `CodeRabbit` round 1 on PR #362.
            if model.hasLocalRtlSdr {
                mainControls
            } else {
                Text("No local RTL-SDR dongle detected.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder
    private var mainControls: some View {
        @Bindable var m = model

        // Master toggle. Kicks the async lifecycle methods via
        // a `Task` — the methods flip observable state
        // optimistically and do the blocking USB / accept-
        // thread work off-main, so the sidebar stays
        // responsive through a 100-500 ms start window. Per
        // `CodeRabbit` round 1 on PR #362.
        Toggle("Share over network", isOn: Binding(
            get: { model.rtlTcpServerRunning },
            set: { newValue in
                Task {
                    if newValue {
                        await model.startRtlTcpServer()
                    } else {
                        await model.stopRtlTcpServer()
                    }
                }
            }
        ))
        .disabled(localDongleClaimedByEngine)

        if localDongleClaimedByEngine {
            Text(
                "Local dongle is in use by the engine. Stop the engine or " +
                "switch the source away from RTL-SDR to share it on the network."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
        }

        // The form fields — nickname, port, bind, mDNS toggle.
        // Editable only while the server is stopped; the Rust
        // side applies state on dongle open so flipping them
        // mid-session wouldn't take effect until restart.
        TextField("Nickname", text: $m.rtlTcpServerNickname)
            .textFieldStyle(.roundedBorder)
            .disabled(model.rtlTcpServerRunning)
            .onChange(of: model.rtlTcpServerNickname) { _, _ in
                model.persistRtlTcpServerConfig()
            }

        LabeledContent("Port") {
            Stepper(value: Binding(
                get: { Int(model.rtlTcpServerPort) },
                set: {
                    // Stepper keeps us within 1024..=65535;
                    // bound belt-and-suspenders in case the
                    // binding is driven programmatically.
                    let clamped = min(max($0, 1024), 65535)
                    model.rtlTcpServerPort = UInt16(clamped)
                    model.persistRtlTcpServerConfig()
                }
            ), in: 1024...65535) {
                Text("\(model.rtlTcpServerPort)")
                    .monospacedDigit()
            }
        }
        .disabled(model.rtlTcpServerRunning)

        LabeledContent("Bind") {
            Picker("", selection: Binding(
                get: { model.rtlTcpServerBindAddress },
                set: {
                    model.rtlTcpServerBindAddress = $0
                    model.persistRtlTcpServerConfig()
                }
            )) {
                ForEach(
                    SdrRtlTcpServer.Config.BindAddress.allCases,
                    id: \.self
                ) { ba in
                    Text(ba.label).tag(ba)
                }
            }
            .labelsHidden()
        }
        .disabled(model.rtlTcpServerRunning)

        Toggle("Announce via mDNS", isOn: Binding(
            get: { model.rtlTcpServerMdnsEnabled },
            set: {
                model.rtlTcpServerMdnsEnabled = $0
                model.persistRtlTcpServerConfig()
            }
        ))
        .disabled(model.rtlTcpServerRunning)

        // Collapsible device-defaults group. Most users keep
        // the defaults; expanding exposes the initial state the
        // server applies on dongle open.
        DisclosureGroup("Device defaults") {
            deviceDefaults
        }

        if let error = model.rtlTcpServerError {
            Text(error)
                .font(.caption)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }

        if model.rtlTcpServerRunning {
            statusRows
            DisclosureGroup("Activity log") {
                activityLog
            }
        }
    }

    // ----------------------------------------------------------
    //  Device defaults form fragment
    // ----------------------------------------------------------

    @ViewBuilder
    private var deviceDefaults: some View {
        LabeledContent("Frequency") {
            TextField(
                "MHz",
                value: Binding(
                    get: { Double(model.rtlTcpServerInitialFreqHz) / 1_000_000.0 },
                    set: {
                        // Clamp to the u32 range in Hz, accepting the
                        // MHz-denominated TextField value.
                        let hz = ($0 * 1_000_000.0).rounded()
                        let clamped = min(max(hz, 0), Double(UInt32.max))
                        model.rtlTcpServerInitialFreqHz = UInt32(clamped)
                        model.persistRtlTcpServerConfig()
                    }
                ),
                format: .number.precision(.fractionLength(0...3))
            )
            .textFieldStyle(.roundedBorder)
            .multilineTextAlignment(.trailing)
            Text("MHz").foregroundStyle(.secondary)
        }
        .disabled(model.rtlTcpServerRunning)

        LabeledContent("Sample rate") {
            Picker("", selection: Binding(
                get: { model.rtlTcpServerInitialSampleRateHz },
                set: {
                    model.rtlTcpServerInitialSampleRateHz = $0
                    model.persistRtlTcpServerConfig()
                }
            )) {
                ForEach(rtlSdrSampleRates, id: \.self) { r in
                    Text("\(Double(r) / 1_000_000.0, specifier: "%.3f") Msps")
                        .tag(r)
                }
            }
            .labelsHidden()
        }
        .disabled(model.rtlTcpServerRunning)

        LabeledContent("Gain") {
            Stepper(value: Binding(
                get: { Int(model.rtlTcpServerInitialGainTenthsDb) },
                set: {
                    model.rtlTcpServerInitialGainTenthsDb = Int32($0)
                    model.persistRtlTcpServerConfig()
                }
            ), in: 0...500, step: 5) {
                if model.rtlTcpServerInitialGainTenthsDb == 0 {
                    Text("Auto")
                        .foregroundStyle(.secondary)
                } else {
                    Text(
                        "\(Double(model.rtlTcpServerInitialGainTenthsDb) / 10.0, specifier: "%.1f") dB"
                    )
                    .monospacedDigit()
                }
            }
        }
        .disabled(model.rtlTcpServerRunning)

        LabeledContent("PPM") {
            Stepper(value: Binding(
                get: { Int(model.rtlTcpServerInitialPpm) },
                set: {
                    model.rtlTcpServerInitialPpm = Int32($0)
                    model.persistRtlTcpServerConfig()
                }
            ), in: -100...100) {
                Text("\(model.rtlTcpServerInitialPpm)")
                    .monospacedDigit()
            }
        }
        .disabled(model.rtlTcpServerRunning)

        Toggle("Bias tee", isOn: Binding(
            get: { model.rtlTcpServerInitialBiasTee },
            set: {
                model.rtlTcpServerInitialBiasTee = $0
                model.persistRtlTcpServerConfig()
            }
        ))
        .disabled(model.rtlTcpServerRunning)

        LabeledContent("Direct sampling") {
            Picker("", selection: Binding(
                get: { model.rtlTcpServerInitialDirectSampling },
                set: {
                    model.rtlTcpServerInitialDirectSampling = $0
                    model.persistRtlTcpServerConfig()
                }
            )) {
                ForEach(SdrCore.DirectSamplingMode.allCases, id: \.self) { m in
                    Text(m.label).tag(m)
                }
            }
            .labelsHidden()
        }
        .disabled(model.rtlTcpServerRunning)
    }

    // ----------------------------------------------------------
    //  Status + activity log
    // ----------------------------------------------------------

    @ViewBuilder
    private var statusRows: some View {
        let stats = model.rtlTcpServerStats
        LabeledContent("Status") {
            if let stats, stats.hasClient {
                Text(stats.connectedClientAddr)
                    .foregroundStyle(.green)
                    .textSelection(.enabled)
            } else {
                Text("Waiting for client")
                    .foregroundStyle(.secondary)
            }
        }
        if let stats {
            LabeledContent("Tuner") {
                Text(
                    stats.tunerName.isEmpty
                        ? "—"
                        : "\(stats.tunerName) (\(stats.gainCount) gains)"
                )
                .foregroundStyle(.secondary)
            }
            LabeledContent("Uptime") {
                Text(formatUptime(stats.uptimeSecs))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            LabeledContent("Data rate") {
                Text(formatRate(bytesSent: stats.bytesSent, uptimeSecs: stats.uptimeSecs))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            if stats.currentFreqHz > 0 {
                LabeledContent("Client freq") {
                    Text(
                        "\(Double(stats.currentFreqHz) / 1_000_000.0, specifier: "%.3f") MHz"
                    )
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                }
            }
            if stats.currentSampleRateHz > 0 {
                LabeledContent("Client rate") {
                    Text(
                        "\(Double(stats.currentSampleRateHz) / 1_000_000.0, specifier: "%.3f") Msps"
                    )
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                }
            }
            if stats.hasCurrentGainMode {
                LabeledContent("Client gain mode") {
                    Text(stats.currentGainAuto ? "Auto" : "Manual")
                        .foregroundStyle(.secondary)
                }
            }
            if stats.hasCurrentGainValue {
                LabeledContent("Client gain") {
                    Text(
                        "\(Double(stats.currentGainTenthsDb) / 10.0, specifier: "%.1f") dB"
                    )
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                }
            }
            if stats.buffersDropped > 0 {
                LabeledContent("Dropped") {
                    Text("\(stats.buffersDropped) buffer(s)")
                        .foregroundStyle(.orange)
                }
            }
        }
    }

    @ViewBuilder
    private var activityLog: some View {
        if model.rtlTcpRecentCommands.isEmpty {
            Text("No commands yet.")
                .font(.caption)
                .foregroundStyle(.secondary)
        } else {
            // Newest-first. The Rust ring is oldest-first;
            // reversing on render keeps the controller cheap.
            ForEach(
                Array(model.rtlTcpRecentCommands.reversed().prefix(activityLogMaxRows).enumerated()),
                id: \.offset
            ) { _, cmd in
                HStack {
                    Text(cmd.op)
                        .font(.caption.monospaced())
                    Spacer()
                    Text("\(cmd.secondsAgo, specifier: "%.1f")s ago")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    // ----------------------------------------------------------
    //  Derived helpers
    // ----------------------------------------------------------

    /// `true` when the engine is currently running with
    /// `.rtlSdr` as its active source. In that case the server
    /// can't start (the dongle is held exclusively by the
    /// engine); the toggle and config fields are disabled and a
    /// caption explains why.
    private var localDongleClaimedByEngine: Bool {
        model.isRunning && model.sourceType == .rtlSdr
    }

    /// Render seconds-since-connect as `HH:MM:SS`.
    private func formatUptime(_ secs: Double) -> String {
        guard secs > 0, secs.isFinite else { return "—" }
        let whole = Int(secs)
        let h = whole / 3600
        let m = (whole % 3600) / 60
        let s = whole % 60
        return String(format: "%02d:%02d:%02d", h, m, s)
    }

    /// Average stream rate in Mbps. Session-cumulative — real
    /// rolling rate (delta over a short window) is a follow-up
    /// if the average turns out to be misleading.
    private func formatRate(bytesSent: UInt64, uptimeSecs: Double) -> String {
        guard uptimeSecs > 0.25 else { return "—" }
        let mbps = Double(bytesSent) * 8.0 / uptimeSecs / 1_000_000.0
        return String(format: "%.2f Mbps", mbps)
    }
}
