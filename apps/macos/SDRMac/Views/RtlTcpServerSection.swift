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

// Pre-#391 the panel rendered a single-client status block
// with peer address, current freq/rate/gain, and a recent-
// commands log. The post-#391 multi-client surface replaces
// that with a per-client list (one row per connected client)
// alongside the aggregate-only stats. Per-client recent-
// commands drilldown is a separate follow-up. Issue #401.

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

        // Compression picker — wires through to both the server
        // config (`has_compression` / `compression`) and the
        // mDNS advertise options (`has_codecs` / `codecs`) so a
        // discovering client sees the same story over the
        // network and at handshake. Issue #417.
        LabeledContent("Compression") {
            Picker("", selection: Binding(
                get: { model.rtlTcpServerCompression },
                set: {
                    model.rtlTcpServerCompression = $0
                    model.persistRtlTcpServerConfig()
                }
            )) {
                ForEach(SdrRtlTcpServer.Compression.allCases, id: \.self) { c in
                    Text(c.label).tag(c)
                }
            }
            .labelsHidden()
        }
        .disabled(model.rtlTcpServerRunning)
        .help("Stream-codec mask the server advertises. None keeps every client on uncompressed IQ; LZ4 negotiates compression with capable clients.")

        // Auth-required mDNS toggle (#417).
        //
        // The actual auth-key enforcement on the rtl_tcp server
        // (forwarding `auth_key` / `auth_key_len` through
        // `SdrRtlTcpServerConfig`, validating the client's
        // `RTLX SetAuthKey` packet) isn't wired on the Mac side
        // yet. This toggle currently only governs the mDNS
        // advertisement so clients can stage a credential
        // prompt before connecting; the server itself still
        // accepts every connection. Tracked in #623 — shipping
        // the toggle now keeps the schema stable for the
        // auth-key plumbing follow-up.
        Toggle("Advertise auth required", isOn: Binding(
            get: { model.rtlTcpServerAuthRequired },
            set: {
                model.rtlTcpServerAuthRequired = $0
                model.persistRtlTcpServerConfig()
            }
        ))
        .disabled(model.rtlTcpServerRunning)
        .help("Sets the mDNS auth-required TXT bit so discovering clients can prompt for a key. Auth-key enforcement on the server itself is a separate follow-up (#623).")

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
            // Per-client activity log is gone for now — the
            // server's recent-commands ring is per-client (post-
            // #391) and re-surfacing it requires the multi-
            // client list path landing on the Mac side. Tracked
            // in #496.
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

    /// Status rows: server-wide aggregates plus a per-client
    /// list rendering one row per connected client (peer
    /// address, codec, freq/rate/gain, bytes sent, drops).
    /// Issue #401.
    @ViewBuilder
    private var statusRows: some View {
        let stats = model.rtlTcpServerStats
        LabeledContent("Clients") {
            if let stats {
                if stats.connectedCount == 0 {
                    Text("Waiting for client")
                        .foregroundStyle(.secondary)
                } else {
                    Text("\(stats.connectedCount) connected")
                        .foregroundStyle(.green)
                }
            } else {
                Text("—")
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
            LabeledContent("Lifetime accepted") {
                Text("\(stats.lifetimeAccepted)")
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            LabeledContent("Total bytes sent") {
                Text(formatBytes(stats.totalBytesSent))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            if stats.totalBuffersDropped > 0 {
                LabeledContent("Dropped (lifetime)") {
                    Text("\(stats.totalBuffersDropped) buffer(s)")
                        .foregroundStyle(.orange)
                }
            }
        }

        // Per-client list — one expandable row per connected
        // client. Sourced from `clientList()` on the same poll
        // tick as `stats` so the count above and the rows below
        // describe the same snapshot. Issue #401.
        ForEach(model.rtlTcpServerClients) { client in
            ClientRow(client: client)
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

    /// Format a byte count as KiB / MiB / GiB. The lifetime
    /// total grows fast at typical RTL-SDR rates (~16 Mbps for
    /// 2 Msps × 8 bits) so a plain "bytes" label gets unreadable
    /// in minutes.
    private func formatBytes(_ bytes: UInt64) -> String {
        let kib: UInt64 = 1_024
        let mib: UInt64 = kib * 1_024
        let gib: UInt64 = mib * 1_024
        if bytes >= gib {
            return String(format: "%.2f GiB", Double(bytes) / Double(gib))
        }
        if bytes >= mib {
            return String(format: "%.2f MiB", Double(bytes) / Double(mib))
        }
        if bytes >= kib {
            return String(format: "%.2f KiB", Double(bytes) / Double(kib))
        }
        return "\(bytes) B"
    }
}

// ============================================================
//  ClientRow — one expandable row per connected client
//
//  Renders the per-client snapshot returned by
//  `SdrRtlTcpServer.clientList()` (#401). Header line shows
//  the peer address + role badge + codec + uptime; the
//  expandable disclosure body adds bytes sent, drops, the
//  client's most recent tuning state, and the recent-command
//  hint.
//
//  Identifiable on `ClientInfo.id` so SwiftUI preserves the
//  expand/collapse state of an individual row across poll
//  ticks even when the list reorders.
// ============================================================

private struct ClientRow: View {
    let client: SdrRtlTcpServer.ClientInfo

    @State private var expanded = false

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            // ---- Body: per-client detail rows ----
            LabeledContent("Bytes sent") {
                Text(formatBytes(client.bytesSent))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            if client.buffersDropped > 0 {
                LabeledContent("Dropped") {
                    Text("\(client.buffersDropped) buffer(s)")
                        .foregroundStyle(.orange)
                }
            }
            LabeledContent("Frequency") {
                if let hz = client.currentFreqHz {
                    Text(String(format: "%.3f MHz", Double(hz) / 1_000_000.0))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                } else {
                    Text("—")
                        .foregroundStyle(.secondary)
                }
            }
            LabeledContent("Sample rate") {
                if let hz = client.currentSampleRateHz {
                    Text(String(format: "%.3f Msps", Double(hz) / 1_000_000.0))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                } else {
                    Text("—")
                        .foregroundStyle(.secondary)
                }
            }
            LabeledContent("Gain") {
                Text(gainDisplay)
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            if client.recentCommandsCount > 0 {
                LabeledContent("Recent commands") {
                    Text(commandsDisplay)
                        .foregroundStyle(.secondary)
                }
            }
        } label: {
            // ---- Header: peer address + role + codec + uptime
            HStack(spacing: 8) {
                Text(client.peerAddress.isEmpty ? "—" : client.peerAddress)
                    .font(.body.monospaced())
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer()
                if client.role == .listener {
                    Text(client.role.label)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .padding(.horizontal, 4)
                        .padding(.vertical, 1)
                        .overlay(
                            RoundedRectangle(cornerRadius: 3)
                                .stroke(.secondary.opacity(0.4))
                        )
                }
                if client.codec == .noneAndLz4 {
                    Text("LZ4")
                        .font(.caption2)
                        .foregroundStyle(.green)
                }
                Text(formatUptime(client.uptimeSecs))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
    }

    // ----------------------------------------------------------
    //  Display helpers
    // ----------------------------------------------------------

    private var gainDisplay: String {
        if client.currentGainAuto == true {
            return "Auto"
        }
        if let tenths = client.currentGainTenthsDb {
            return String(format: "%.1f dB", Double(tenths) / 10.0)
        }
        return "—"
    }

    private var commandsDisplay: String {
        let count = client.recentCommandsCount
        let countLabel = "\(count) command\(count == 1 ? "" : "s")"
        if let age = client.lastCommandAgeSecs {
            return "\(countLabel) · last \(formatAge(age)) ago"
        }
        return countLabel
    }

    private func formatUptime(_ secs: Double) -> String {
        let total = Int(secs.rounded())
        let h = total / 3600
        let m = (total % 3600) / 60
        let s = total % 60
        if h > 0 { return String(format: "%dh %02dm", h, m) }
        if m > 0 { return String(format: "%dm %02ds", m, s) }
        return "\(s)s"
    }

    private func formatAge(_ secs: Double) -> String {
        if secs < 1 { return "<1s" }
        let total = Int(secs.rounded())
        if total < 60 { return "\(total)s" }
        let m = total / 60
        let s = total % 60
        return String(format: "%dm %02ds", m, s)
    }

    private func formatBytes(_ bytes: UInt64) -> String {
        let kib: UInt64 = 1_024
        let mib: UInt64 = kib * 1_024
        let gib: UInt64 = mib * 1_024
        if bytes >= gib {
            return String(format: "%.2f GiB", Double(bytes) / Double(gib))
        }
        if bytes >= mib {
            return String(format: "%.2f MiB", Double(bytes) / Double(mib))
        }
        if bytes >= kib {
            return String(format: "%.2f KiB", Double(bytes) / Double(kib))
        }
        return "\(bytes) B"
    }
}
