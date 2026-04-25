//
// ScannerPanelView.swift — Scanner activity panel (closes #447).
//
// Three flat Sections matching the GTK
// `crates/sdr-ui/src/sidebar/scanner_panel.rs` layout:
//
//   - Scanner   — master enable toggle
//   - Active    — Channel / State rows + lockout button (button
//                 only visible when latched)
//   - Timing    — default dwell + default hang
//
// The Channel / State row text wraps multi-line by default —
// SwiftUI `Text` doesn't truncate without an explicit
// `.lineLimit(1)`, which we deliberately don't apply. Long
// bookmark names like "KY State Police District 7 Dispatch"
// stay fully readable in the sidebar.
//
// Bookmark → `ScannerChannel` projection isn't wired yet — the
// Mac `Bookmark` model doesn't carry `scan_enabled` /
// `priority` fields the Linux side has. Until #490 lands that,
// flipping the master switch leaves the engine in `.idle` (no
// rotation to drive); the Scanner section's footer documents
// the gap so the panel reads honestly during the carve-out.

import SwiftUI
import SdrCoreKit

struct ScannerPanelView: View {
    var body: some View {
        Form {
            ScannerMasterSection()
            ActiveChannelSection()
            TimingSection()
        }
        .formStyle(.grouped)
    }
}

// ============================================================
//  Scanner master switch
// ============================================================

private struct ScannerMasterSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            Toggle("Scanner", isOn: Binding(
                get: { model.scannerEnabled },
                set: { model.setScannerEnabled($0) }
            ))
        } header: {
            Text("Scanner")
        } footer: {
            // Honest footer: the master switch is wired, but
            // there are no scan-enabled bookmarks to rotate
            // through until per-bookmark scan opt-in lands
            // (#490). The toggle still flips engine state
            // (visible via the Active section's State row); it
            // just stays in Off until #490 ships.
            Text("Sweep through bookmarked frequencies. Per-bookmark scan opt-in lands in a follow-up — until then the rotation list is empty and the scanner stays Off.")
                .font(.caption)
        }
    }
}

// ============================================================
//  Active — current channel / state / lockout
// ============================================================

private struct ActiveChannelSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            // Channel row. Latched: bookmark name + formatted
            // frequency. Idle: em-dash placeholder. Subtitle
            // wraps to multiple lines naturally — no
            // `.lineLimit(1)` so long names stay readable.
            LabeledContent("Channel") {
                Text(channelLabel)
                    .font(.callout)
                    .foregroundStyle(model.scannerActiveChannel == nil ? .secondary : .primary)
                    .multilineTextAlignment(.trailing)
            }

            // State row — tracks the engine's `ScannerState`
            // phase enum (Off / Retuning / Listening / Hang).
            LabeledContent("State") {
                Text(model.scannerState.label)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }

            // Lockout button — only visible when the scanner
            // has a channel latched. The button alone in an
            // always-visible row would leave a dangling labeled
            // strip when the scanner goes idle; the GTK panel
            // hides the whole row for the same reason.
            if model.scannerActiveChannel != nil {
                Button(role: .destructive) {
                    model.lockoutCurrentScannerChannel()
                } label: {
                    Label("Lockout this channel", systemImage: "nosign")
                }
            }
        } header: {
            Text("Active")
        } footer: {
            Text("Current channel and detector state. Lockout skips the active channel for the rest of the scanner session.")
                .font(.caption)
        }
    }

    /// Render the Channel row's right-side label. Latched:
    /// `"<bookmark name> — <freq MHz>"` to match the GTK
    /// panel's vocabulary. Idle: em-dash placeholder, same
    /// glyph the Linux side uses.
    private var channelLabel: String {
        guard let channel = model.scannerActiveChannel else {
            return "—"
        }
        let mhz = Double(channel.frequencyHz) / 1_000_000.0
        return String(format: "%@ — %.4f MHz", channel.name, mhz)
    }
}

// ============================================================
//  Timing — default dwell / hang
// ============================================================

private struct TimingSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            LabeledContent("Default dwell") {
                Stepper(
                    value: Binding(
                        get: { model.scannerDefaultDwellMs },
                        set: { model.setScannerDefaultDwellMs($0) }
                    ),
                    in: 50...500,
                    step: 10
                ) {
                    Text("\(model.scannerDefaultDwellMs) ms")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
            LabeledContent("Default hang") {
                Stepper(
                    value: Binding(
                        get: { model.scannerDefaultHangMs },
                        set: { model.setScannerDefaultHangMs($0) }
                    ),
                    in: 500...5_000,
                    step: 100
                ) {
                    Text("\(model.scannerDefaultHangMs) ms")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
        } header: {
            Text("Timing")
        } footer: {
            Text("How long the scanner lingers on each channel. Dwell is the settle window after retune; hang is the linger time after squelch closes before advancing.")
                .font(.caption)
        }
    }
}
