//
// SourceSection.swift — sidebar panel for source/tuner controls.
//
// Device picker at top (RTL-SDR / Network IQ / File playback /
// RTL-TCP) followed by per-source forms: RTL-SDR exposes the
// USB tuner's sample rate / gain / AGC / PPM; Network exposes
// a host/port/protocol triple with an Apply button; File
// exposes a path text field with a "Choose WAV…" button;
// RTL-TCP shows a client picker (discovered servers from mDNS,
// favorites, manual entry) and a live connection-state row.
// Per issue #326.
//
// Advanced controls (DC blocking, IQ inversion, IQ correction,
// decimation) live in a collapsible "Advanced" DisclosureGroup
// at the bottom, default-collapsed so the layout stays clean.
// They apply to every source type because they sit in the IQ
// frontend, not in the source itself. Mirrors GTK's "Advanced"
// expander in its Source panel (issues #235, #236, #246).

import SwiftUI
import SdrCoreKit
import UniformTypeIdentifiers

/// RTL-SDR supported sample rates (in Hz). Matches
/// `crates/sdr-rtlsdr::RATE_OPTIONS`.
private let rtlSdrSampleRates: [Double] = [
    250_000, 1_024_000, 1_536_000, 1_792_000, 1_920_000,
    2_048_000, 2_160_000, 2_400_000, 2_560_000, 2_880_000,
    3_200_000,
]

/// Allowed decimation factors for the Advanced group. Must be
/// powers of two — the engine's `SetDecimation` handler rejects
/// non-power-of-two values. Mirrors GTK's
/// `sdr-ui::sidebar::source_panel::DECIMATION_FACTORS`.
private let decimationFactors: [UInt32] = [1, 2, 4, 8, 16]

/// Source types offered in the top-of-section picker. `.rtlTcp`
/// is included; its form renders a discovered-server list +
/// favorites + manual-entry path + connection-state row, not
/// the generic per-command controls. Per issue #326.
private let supportedSourceTypes: [SourceType] = [.rtlSdr, .network, .file, .rtlTcp]

struct SourceSection: View {
    @Environment(CoreModel.self) private var model

    /// User's currently-visible-but-possibly-uncommitted source
    /// selection. Seeded from `model.sourceType` on first appear
    /// and updated by the picker. Distinct from
    /// `model.sourceType` so the picker can show the intended
    /// type while the user is still configuring its endpoint —
    /// switching to `.network` or `.file` **without** first
    /// knowing a valid host/port or file path would tear down
    /// the current source and leave the engine on a broken one
    /// until the user manually fixed it. `.rtlSdr` commits
    /// immediately since it needs no config. Per `CodeRabbit`
    /// round 1 on PR #358.
    @State private var pendingType: SourceType = .rtlSdr

    /// Local edit buffer for the network host. Mirrors the
    /// pattern from `SettingsView.AudioPane` — TextField edits
    /// shouldn't rebuild the connection per keystroke.
    @State private var hostEdit: String = ""

    /// Local edit buffer for the network port.
    @State private var portEdit: String = ""

    /// Local edit buffers for the rtl_tcp client manual-entry
    /// form. Kept separate from `hostEdit`/`portEdit` so the
    /// two source types don't clobber each other's in-flight
    /// edits even though both write to the same engine-side
    /// storage on Connect.
    @State private var rtlTcpHostEdit: String = ""
    @State private var rtlTcpPortEdit: String = ""

    /// One-shot latch so the initial `.onAppear` seeds the
    /// local edit buffers from the model without clobbering
    /// in-progress user edits when SwiftUI re-fires `.onAppear`
    /// on sibling state changes. Same pattern as the audio
    /// pane.
    @State private var didPrefill: Bool = false

    /// Backing state for the `fileImporter` sheet.
    @State private var fileImporterPresented: Bool = false

    var body: some View {
        Section("Source") {
            LabeledContent("Type") {
                Picker("", selection: Binding(
                    get: { pendingType },
                    set: { newType in
                        pendingType = newType
                        // `.rtlSdr` needs no per-source config,
                        // so commit immediately — matches user
                        // expectation that "I picked RTL-SDR"
                        // means the engine switches now.
                        // `.network`, `.file`, and `.rtlTcp`
                        // defer to their respective Apply /
                        // Choose / Connect action buttons.
                        if newType == .rtlSdr {
                            model.setSourceType(.rtlSdr)
                        }
                    }
                )) {
                    ForEach(supportedSourceTypes, id: \.self) { t in
                        // Disable the `.rtlSdr` entry while the
                        // rtl_tcp server owns the local dongle.
                        // Mutual exclusivity — the two paths
                        // can't share USB access. Per #353.
                        Text(t.label)
                            .tag(t)
                            .disabled(
                                t == .rtlSdr && model.rtlTcpServerHoldsDongle
                            )
                    }
                }
                .labelsHidden()
            }

            if model.rtlTcpServerHoldsDongle && pendingType == .rtlSdr {
                Text(
                    "Local dongle is being shared over the network. " +
                    "Stop the rtl_tcp server to use it here."
                )
                .font(.caption)
                .foregroundStyle(.secondary)
            }

            // Per-source content follows the **pending** type so
            // the user can configure before commit. Only the
            // active type's fields render; switching sources
            // collapses the rest. `model.sourceType` still
            // determines what the engine is actually running,
            // and a small caption below flags an uncommitted
            // change so the picker doesn't feel like a no-op
            // while the user is mid-configure.
            switch pendingType {
            case .rtlSdr: rtlSdrControls
            case .network: networkControls
            case .file: fileControls
            case .rtlTcp: rtlTcpControls
            }

            // "Advanced" group applies to every source — lives
            // in the IQ frontend, not the source itself.
            DisclosureGroup("Advanced") {
                Toggle("DC blocking", isOn: Binding(
                    get: { model.dcBlockingEnabled },
                    set: { model.setDcBlocking($0) }
                ))

                Toggle("IQ inversion", isOn: Binding(
                    get: { model.iqInversionEnabled },
                    set: { model.setIqInversion($0) }
                ))

                Toggle("IQ correction", isOn: Binding(
                    get: { model.iqCorrectionEnabled },
                    set: { model.setIqCorrection($0) }
                ))

                LabeledContent("Decimation") {
                    Picker("", selection: Binding(
                        get: { model.decimationFactor },
                        set: { model.setDecimation($0) }
                    )) {
                        ForEach(decimationFactors, id: \.self) { f in
                            Text(f == 1 ? "None" : "1/\(f)").tag(f)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.segmented)
                }
            }
        }
        .onAppear {
            guard !didPrefill else { return }
            didPrefill = true
            pendingType = model.sourceType
            hostEdit = model.networkSourceHost
            portEdit = String(model.networkSourcePort)
            // Seed the rtl_tcp manual-entry fields from the
            // persisted last-connected snapshot so the user's
            // most recent server pre-fills on next launch.
            // Falls back to the network-source host/port when
            // no snapshot exists (first-time use). Per #326.
            if let last = model.rtlTcpLastConnected {
                rtlTcpHostEdit = last.host
                rtlTcpPortEdit = String(last.port)
            } else {
                rtlTcpHostEdit = model.networkSourceHost
                rtlTcpPortEdit = String(model.networkSourcePort)
            }
        }
        .onChange(of: model.sourceType) { _, new in
            // Sync the picker back to engine-side changes
            // (bookmark apply, programmatic updates, etc.)
            // without clobbering an in-progress user selection:
            // only track if the pending choice matches the
            // previous committed value.
            if pendingType != new { pendingType = new }
        }
        .fileImporter(
            isPresented: $fileImporterPresented,
            // WAV-only. The playback path is documented as
            // two-channel (I/Q) WAV; widening to `.audio` would
            // let the user pick files that only fail at engine
            // open time. Per `CodeRabbit` round 1 on PR #358.
            allowedContentTypes: [.wav],
            allowsMultipleSelection: false
        ) { result in
            if case .success(let urls) = result, let url = urls.first {
                // Path first, then commit the source-type
                // switch. `setFilePath` populates
                // `model.filePath`; only then does
                // `setSourceType(.file)`'s guard pass. Same
                // engine-command ordering is important for
                // correctness — the mpsc channel preserves
                // order, so the engine sees the path update
                // before the source-type change.
                model.setFilePath(url.path)
                model.setSourceType(.file)
            }
        }
    }

    /// Parse `portEdit` into a `UInt16` in the legal range.
    /// Returns `nil` on empty / non-numeric / out-of-range input.
    /// Matches the audio-pane helper shape.
    private func portValue() -> UInt16? {
        guard let raw = Int(portEdit.trimmingCharacters(in: .whitespaces)),
              (1...Int(UInt16.max)).contains(raw) else {
            return nil
        }
        return UInt16(raw)
    }

    /// Host with leading/trailing whitespace stripped — single
    /// source of truth for the normalization step.
    private var normalizedHost: String {
        hostEdit.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var applyButtonDisabled: Bool {
        normalizedHost.isEmpty || portValue() == nil
    }

    // ----------------------------------------------------------
    //  Per-source form fragments
    // ----------------------------------------------------------

    @ViewBuilder
    private var rtlSdrControls: some View {
        LabeledContent("Device") {
            Text(model.deviceInfo.isEmpty ? "—" : model.deviceInfo)
                .foregroundStyle(.secondary)
        }

        LabeledContent("Sample rate") {
            Picker("", selection: Binding(
                get: { model.sourceSampleRateHz },
                set: { model.setSampleRate($0) }
            )) {
                ForEach(rtlSdrSampleRates, id: \.self) {
                    Text(formatRate($0)).tag($0)
                }
            }
            .labelsHidden()
        }

        LabeledContent("Gain") {
            if model.agcEnabled {
                Text("AGC").foregroundStyle(.secondary)
            } else if model.availableGains.isEmpty {
                Text("—").foregroundStyle(.secondary)
            } else {
                GainSlider(
                    steps: model.availableGains,
                    value: model.gainDb,
                    commit: { model.setGain($0) }
                )
            }
        }

        Toggle("AGC", isOn: Binding(
            get: { model.agcEnabled },
            set: { model.setAgc($0) }
        ))

        LabeledContent("PPM") {
            Stepper(value: Binding(
                get: { model.ppmCorrection },
                set: { model.setPpm($0) }
            ), in: -100...100) {
                Text("\(model.ppmCorrection)")
            }
        }
    }

    @ViewBuilder
    private var networkControls: some View {
        TextField("Host", text: $hostEdit)
            .textFieldStyle(.roundedBorder)
            .disableAutocorrection(true)
        TextField("Port", text: $portEdit)
            .textFieldStyle(.roundedBorder)
        Picker("Protocol", selection: Binding(
            get: { model.networkSourceProtocol },
            set: { proto in
                // Protocol changes count as endpoint changes —
                // push through immediately if host/port parse.
                // The picker is disabled below when that's not
                // true, so this branch is always reachable, but
                // the guard stays as defence against a future
                // regression that loosens the `.disabled`.
                let host = normalizedHost
                if !host.isEmpty, let port = portValue() {
                    model.applyNetworkSourceConfig(
                        host: host,
                        port: port,
                        protocol: proto
                    )
                }
            }
        )) {
            ForEach(NetworkSourceProtocol.allCases, id: \.self) { p in
                Text(p.label).tag(p)
            }
        }
        // Same validity gate as the Apply button — without
        // this the picker visually flips on bad host/port and
        // snaps back on the next SwiftUI pass, which reads as a
        // flaky control. Per `CodeRabbit` round 2 on PR #358.
        .disabled(applyButtonDisabled)

        HStack {
            Button {
                let host = normalizedHost
                guard !host.isEmpty, let port = portValue() else { return }
                model.applyNetworkSourceConfig(
                    host: host,
                    port: port,
                    protocol: model.networkSourceProtocol
                )
                // Commit the source-type switch only AFTER the
                // endpoint landed — avoids tearing down the
                // current source before the new one has a
                // valid address. When `.network` is already the
                // active source this reduces to a no-op on the
                // type but still triggers an engine-side
                // reopen via the stored config. Per `CodeRabbit`
                // round 1 on PR #358.
                model.setSourceType(.network)
            } label: {
                Label(model.sourceType == .network ? "Apply" : "Use this source", systemImage: "arrow.up.circle")
            }
            .disabled(applyButtonDisabled)
        }

        if pendingType != model.sourceType {
            Text("Source switches to Network IQ when you hit Apply.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }

        Text(
            "TCP dials outbound to a remote IQ server. UDP binds locally and receives datagrams."
        )
        .font(.caption)
        .foregroundStyle(.secondary)
    }

    @ViewBuilder
    private var fileControls: some View {
        LabeledContent("File") {
            // Path is read-only text; edits go through
            // `fileImporter` below so we can round-trip a
            // sandboxed URL if the app ever gets sandboxed.
            Text(model.filePath.isEmpty ? "—" : (model.filePath as NSString).lastPathComponent)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
        }
        Button {
            fileImporterPresented = true
        } label: {
            Label(model.sourceType == .file ? "Choose another WAV…" : "Choose WAV…", systemImage: "folder")
        }
        if !model.filePath.isEmpty {
            Text(model.filePath)
                .font(.caption2)
                .foregroundStyle(.tertiary)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
        }
        if pendingType != model.sourceType {
            Text("Source switches to File playback after you pick a WAV.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        Text("Plays back a two-channel (I/Q) WAV file. Sample rate is read from the file header.")
            .font(.caption)
            .foregroundStyle(.secondary)
    }

    // ----------------------------------------------------------
    //  rtl_tcp client form — issue #326
    // ----------------------------------------------------------

    @ViewBuilder
    private var rtlTcpControls: some View {
        // Connection-state status row. Always visible when the
        // .rtlTcp arm renders so the user sees the current
        // engine state — `Not connected` before the first
        // Connect click, `Connecting…` / `Connected` /
        // `Retrying in N s` / `Failed — …` after. Subtitle
        // format matches Linux `format_rtl_tcp_state()`.
        LabeledContent("Status") {
            Text(CoreModel.formatRtlTcpConnectionState(model.rtlTcpConnectionState))
                .foregroundStyle(rtlTcpStatusColor)
                .font(.callout)
                .lineLimit(2)
                .multilineTextAlignment(.trailing)
        }

        // Disconnect + Retry row. Only rendered when the source
        // type is actually .rtlTcp so the buttons don't appear
        // on a picker that's mid-configure (the engine would
        // log and drop the commands anyway, but showing the
        // controls is misleading). Per issue #326.
        if model.sourceType == .rtlTcp {
            HStack(spacing: 8) {
                Button {
                    model.disconnectRtlTcp()
                } label: {
                    Label("Disconnect", systemImage: "xmark.circle")
                }
                .disabled(disconnectDisabled)
                Button {
                    model.retryRtlTcpNow()
                } label: {
                    Label("Retry now", systemImage: "arrow.clockwise")
                }
                .disabled(retryDisabled)
            }
        }

        // Tuner control surface — only meaningful when actually
        // connected (the 0x02-0x0e wire commands need an open
        // socket). Hidden otherwise so the form doesn't imply
        // "you can fiddle these pre-connection and they'll
        // take effect later" (they would be silently dropped).
        if case .connected(_, let gainCount) = model.rtlTcpConnectionState {
            rtlTcpControlSurface(gainCount: gainCount)
        }

        // Favorites — user-pinned servers that persist across
        // sessions. Shown whenever non-empty so the user can
        // one-tap recall even before any mDNS discovery lands.
        if !model.rtlTcpFavorites.isEmpty {
            DisclosureGroup("Known servers") {
                ForEach(model.rtlTcpFavorites) { fav in
                    rtlTcpFavoriteRow(fav)
                }
            }
        }

        // Nearby servers — live mDNS announcements. Collapsed
        // when empty so the form stays compact on a LAN with
        // no rtl_tcp peers. Otherwise default-expanded so the
        // list is visible without an extra tap.
        DisclosureGroup(
            "Nearby servers\(model.rtlTcpDiscoveredServers.isEmpty ? "" : " (\(model.rtlTcpDiscoveredServers.count))")"
        ) {
            if model.rtlTcpDiscoveredServers.isEmpty {
                Text("No rtl_tcp servers advertising on this network.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(model.rtlTcpDiscoveredServers, id: \.instanceName) { server in
                    rtlTcpDiscoveredRow(server)
                }
            }
        }

        // Manual-entry host + port + Connect.
        TextField("Host", text: $rtlTcpHostEdit)
            .textFieldStyle(.roundedBorder)
            .disableAutocorrection(true)
            .textContentType(.URL)
        LabeledContent("Port") {
            TextField("1234", text: $rtlTcpPortEdit)
                .textFieldStyle(.roundedBorder)
                .multilineTextAlignment(.trailing)
                .frame(maxWidth: 90)
        }
        Button {
            commitRtlTcpConnect()
        } label: {
            Label(
                model.sourceType == .rtlTcp ? "Reconnect" : "Connect",
                systemImage: "antenna.radiowaves.left.and.right"
            )
        }
        .disabled(rtlTcpConnectDisabled)

        if pendingType != model.sourceType {
            Text("Source switches to RTL-TCP after you connect.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        if let err = model.lastError, !err.isEmpty {
            Text(err)
                .font(.caption)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }
    }

    /// Color hint for the status row subtitle — red on failure,
    /// yellow on in-flight states, secondary otherwise.
    private var rtlTcpStatusColor: Color {
        switch model.rtlTcpConnectionState {
        case .failed: return .red
        case .retrying, .connecting: return .orange
        case .connected: return .secondary
        case .disconnected: return .secondary
        }
    }

    /// Parse `rtlTcpPortEdit` into a `UInt16` in `1…65535`.
    /// Returns `nil` on empty / non-numeric / out-of-range.
    private func rtlTcpPortValue() -> UInt16? {
        guard let raw = Int(rtlTcpPortEdit.trimmingCharacters(in: .whitespaces)),
              (1...Int(UInt16.max)).contains(raw) else {
            return nil
        }
        return UInt16(raw)
    }

    private var rtlTcpConnectDisabled: Bool {
        rtlTcpHostEdit.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || rtlTcpPortValue() == nil
    }

    private func commitRtlTcpConnect() {
        guard let port = rtlTcpPortValue() else { return }
        let host = rtlTcpHostEdit.trimmingCharacters(in: .whitespacesAndNewlines)
        // Derive a nickname from a matching mDNS announce if one
        // exists for this host:port, otherwise fall back to the
        // raw host:port string. The model applies the same
        // fallback but checking here lets us pass a better label
        // through to persistence on first connect.
        let nickname = discoveredNickname(host: host, port: port)
        model.connectToRtlTcp(host: host, port: port, nickname: nickname)
    }

    /// Look up the nickname for an endpoint from the live mDNS
    /// discovery list. Returns `""` if there's no match — the
    /// model defaults to `host:port` in that case.
    private func discoveredNickname(host: String, port: UInt16) -> String {
        if let ds = model.rtlTcpDiscoveredServers.first(where: {
            $0.hostname == host && $0.port == port
        }) {
            return ds.nickname.isEmpty ? ds.instanceName : ds.nickname
        }
        return ""
    }

    // ----------------------------------------------------------
    //  rtl_tcp row builders — favorites + nearby lists
    // ----------------------------------------------------------

    /// A row in the "Known servers" (favorites) list. Shows
    /// nickname + tuner / gain-count / host:port subtitle,
    /// with a trailing unstar button. Tap the row body to
    /// prefill the manual-entry fields with this favorite's
    /// host/port.
    @ViewBuilder
    private func rtlTcpFavoriteRow(_ fav: RtlTcpClientFavorite) -> some View {
        HStack(spacing: 8) {
            Image(systemName: "star.fill")
                .foregroundStyle(.yellow)
                .font(.caption)
            VStack(alignment: .leading, spacing: 2) {
                Text(fav.nickname)
                    .font(.callout)
                    .lineLimit(1)
                Text(favoriteSubtitle(fav))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            Button {
                model.removeRtlTcpFavorite(key: fav.key)
            } label: {
                Image(systemName: "star.slash")
            }
            .buttonStyle(.borderless)
            .help("Remove from favorites")
        }
        .contentShape(Rectangle())
        .onTapGesture {
            guard let (h, p) = fav.parsedEndpoint else { return }
            rtlTcpHostEdit = h
            rtlTcpPortEdit = String(p)
        }
    }

    /// Subtitle for a favorite row. Prefers tuner + gain-count
    /// when available (populated from a live announce), falls
    /// back to the stable `host:port` key when offline and no
    /// cached metadata is present.
    private func favoriteSubtitle(_ fav: RtlTcpClientFavorite) -> String {
        if let tuner = fav.tunerName, let gains = fav.gainCount {
            return "\(fav.key) · \(tuner) (\(gains) gains)"
        } else if let tuner = fav.tunerName {
            return "\(fav.key) · \(tuner)"
        } else {
            return fav.key
        }
    }

    /// A row in the "Nearby servers" (live mDNS) list. Shows
    /// nickname + tuner / gain-count / host:port subtitle,
    /// with a trailing star button to pin as a favorite. Tap
    /// the row body to prefill the manual-entry fields.
    @ViewBuilder
    private func rtlTcpDiscoveredRow(_ server: SdrRtlTcpBrowser.DiscoveredServer) -> some View {
        let key = "\(server.hostname):\(server.port)"
        let isFavorited = model.rtlTcpFavorites.contains { $0.key == key }
        HStack(spacing: 8) {
            Image(systemName: "antenna.radiowaves.left.and.right")
                .foregroundStyle(.secondary)
                .font(.caption)
            VStack(alignment: .leading, spacing: 2) {
                Text(discoveredDisplayName(server))
                    .font(.callout)
                    .lineLimit(1)
                Text(discoveredSubtitle(server))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            Button {
                if isFavorited {
                    model.removeRtlTcpFavorite(key: key)
                } else {
                    model.addRtlTcpFavorite(RtlTcpClientFavorite(from: server))
                }
            } label: {
                Image(systemName: isFavorited ? "star.fill" : "star")
                    .foregroundStyle(isFavorited ? .yellow : .secondary)
            }
            .buttonStyle(.borderless)
            .help(isFavorited ? "Remove from favorites" : "Add to favorites")
        }
        .contentShape(Rectangle())
        .onTapGesture {
            rtlTcpHostEdit = server.hostname
            rtlTcpPortEdit = String(server.port)
        }
    }

    private func discoveredDisplayName(_ s: SdrRtlTcpBrowser.DiscoveredServer) -> String {
        s.nickname.isEmpty ? s.instanceName : s.nickname
    }

    private func discoveredSubtitle(_ s: SdrRtlTcpBrowser.DiscoveredServer) -> String {
        let endpoint = "\(s.hostname):\(s.port)"
        if !s.tuner.isEmpty && s.gains != 0 {
            return "\(endpoint) · \(s.tuner) (\(s.gains) gains)"
        } else if !s.tuner.isEmpty {
            return "\(endpoint) · \(s.tuner)"
        } else {
            return endpoint
        }
    }

    // ----------------------------------------------------------
    //  Disconnect / Retry enablement
    // ----------------------------------------------------------

    /// Disconnect is only actionable when there's something to
    /// disconnect from — .connected, .connecting, or .retrying.
    private var disconnectDisabled: Bool {
        switch model.rtlTcpConnectionState {
        case .connected, .connecting, .retrying: return false
        case .disconnected, .failed: return true
        }
    }

    /// Retry-now is only useful when we're between attempts —
    /// .retrying (skip the backoff sleep), .failed (manual
    /// restart), or .disconnected (reopen after an explicit
    /// disconnect). Active .connected / .connecting states
    /// have nothing to retry.
    private var retryDisabled: Bool {
        switch model.rtlTcpConnectionState {
        case .retrying, .failed, .disconnected: return false
        case .connecting, .connected: return true
        }
    }

    // ----------------------------------------------------------
    //  Tuner control surface — issue #326
    // ----------------------------------------------------------

    /// Wire-protocol command controls exposed when the rtl_tcp
    /// client is connected: sample rate, gain-by-index, PPM,
    /// bias-T, offset tuning, direct sampling, RTL (digital)
    /// AGC, tuner (analog) AGC. Mirrors the Linux GTK panel's
    /// control set; see the rtl_tcp protocol's 0x01-0x0e
    /// command map in `crates/sdr-source-network/src/rtl_tcp.rs`.
    @ViewBuilder
    private func rtlTcpControlSurface(gainCount: UInt32) -> some View {
        DisclosureGroup("Tuner controls") {
            LabeledContent("Sample rate") {
                Picker("", selection: Binding(
                    get: { model.sourceSampleRateHz },
                    set: { model.setSampleRate($0) }
                )) {
                    ForEach(rtlSdrSampleRates, id: \.self) {
                        Text(formatRate($0)).tag($0)
                    }
                }
                .labelsHidden()
            }

            // Gain — rtl_tcp clients address the server's gain
            // table by index (the server doesn't publish dB
            // values over the wire, only a count). Linux shows
            // this as a slider; matching here with a Stepper +
            // read-out so the discrete selections feel
            // concrete. Hidden when gainCount == 0 (rare, but
            // some servers publish no tuner).
            if gainCount > 0 {
                LabeledContent("Gain") {
                    VStack(alignment: .trailing, spacing: 2) {
                        Stepper(
                            value: Binding(
                                get: { Int(model.rtlTcpGainIndex) },
                                set: {
                                    let clamped = min(max($0, 0), Int(gainCount - 1))
                                    model.setRtlTcpGainIndex(UInt32(clamped))
                                }
                            ),
                            in: 0...Int(gainCount - 1)
                        ) {
                            Text("#\(model.rtlTcpGainIndex + 1) of \(gainCount)")
                                .monospacedDigit()
                        }
                    }
                }
            }

            LabeledContent("PPM") {
                Stepper(
                    value: Binding(
                        get: { model.ppmCorrection },
                        set: { model.setPpm(Int($0)) }
                    ),
                    in: -200...200
                ) {
                    Text("\(model.ppmCorrection) ppm").monospacedDigit()
                }
            }

            Toggle("Bias-T", isOn: Binding(
                get: { model.biasTeeEnabled },
                set: { model.setBiasTee($0) }
            ))

            Toggle("Offset tuning", isOn: Binding(
                get: { model.offsetTuningEnabled },
                set: { model.setOffsetTuning($0) }
            ))

            LabeledContent("Direct sampling") {
                Picker("", selection: Binding(
                    get: { model.directSamplingMode },
                    set: { model.setDirectSampling($0) }
                )) {
                    ForEach(SdrCore.DirectSamplingMode.allCases, id: \.self) { m in
                        Text(m.label).tag(m)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
            }

            Toggle("RTL AGC", isOn: Binding(
                get: { model.rtlAgcEnabled },
                set: { model.setRtlAgc($0) }
            ))

            Toggle("Tuner AGC", isOn: Binding(
                get: { model.agcEnabled },
                set: { model.setAgc($0) }
            ))
        }
    }
}

/// RTL-SDR exposes a discrete set of gain values (not a continuous
/// range). This slider snaps to the nearest entry in `steps`.
private struct GainSlider: View {
    let steps: [Double]
    let value: Double
    let commit: (Double) -> Void

    @State private var index: Double = 0

    var body: some View {
        VStack(spacing: 2) {
            Slider(
                value: $index,
                in: 0...Double(max(steps.count - 1, 0)),
                step: 1,
                onEditingChanged: { editing in
                    guard !editing else { return }
                    let i = Int(index.rounded())
                    if steps.indices.contains(i) { commit(steps[i]) }
                }
            )
            Text(String(format: "%.1f dB", currentDb))
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .onAppear { index = Double(closestIndex(for: value)) }
        .onChange(of: value) { _, new in
            index = Double(closestIndex(for: new))
        }
    }

    private var currentDb: Double {
        let i = Int(index.rounded())
        return steps.indices.contains(i) ? steps[i] : value
    }

    private func closestIndex(for v: Double) -> Int {
        guard !steps.isEmpty else { return 0 }
        var best = 0
        var bestDiff = abs(steps[0] - v)
        for (i, s) in steps.enumerated().dropFirst() {
            let d = abs(s - v)
            if d < bestDiff { bestDiff = d; best = i }
        }
        return best
    }
}

// `formatRate` moved to `Formatters.swift` so StatusBar and any
// future view can share the same rendering without a dangling
// forward reference between sibling files.
