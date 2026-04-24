//
// GeneralPanelView.swift — General activity panel.
//
// Lands behind the General icon in the left activity bar.
// First real activity panel under epic #441 (#443). Composes
// two flat sections — Band quick-tune and Source — into a
// scrollable Form.
//
// The Source section reuses the existing `SourceSection` view
// verbatim. Keeping that view as the source-of-truth for
// device / sample-rate / gain / IQ / decimation / PPM
// controls means subsequent migrations don't need to re-port
// per-control state plumbing — they just relocate the Section
// wrapper to a different panel host. Eventually `SourceSection`
// trims down to just RTL-SDR + the Type picker, and the other
// source types (network / file / rtl_tcp) move to their own
// activity panels (Share for rtl_tcp, etc).
//
// Per sub-ticket #443.

import SwiftUI
// `DemodMode.label` lives in SdrCoreKit — used by the band-
// preset row to render the demod tag badge.
import SdrCoreKit

struct GeneralPanelView: View {
    var body: some View {
        Form {
            // Band quick-tune — landed in #443.
            BandPresetsSection()

            // Source — the configure-this-radio core. Reused
            // from the pre-redesign sidebar; eventually only
            // RTL-SDR + the type picker stays here while
            // network / file / rtl_tcp move to dedicated
            // activity panels (Share for rtl_tcp, etc).
            SourceSection()
        }
        .formStyle(.grouped)
    }
}

// ============================================================
//  Band presets — quick-tune to common channels
// ============================================================

/// Single Picker row for quick-tuning to a common band.
/// Matches the GTK `ComboRow`-based preset row in
/// `navigation_panel.rs`. The picked preset lives on
/// `CoreModel.lastSelectedBandPresetID` (not `@State` here)
/// so the selection survives panel close + activity swap +
/// app relaunch — the Mac panel host rebuilds this view on
/// every reopen, which would clear local view state. Manual
/// tunes don't auto-clear the dropdown (same behavior as
/// Linux). Per `CodeRabbit` round 1 on PR #493.
private struct BandPresetsSection: View {
    @Environment(CoreModel.self) private var model

    var body: some View {
        Section {
            LabeledContent("Preset") {
                Picker(selection: Binding<BandPreset?>(
                    get: { model.lastSelectedBandPreset },
                    set: { model.setLastSelectedBandPreset($0) }
                )) {
                    // Placeholder slot for the "no preset
                    // selected" state — the dropdown opens
                    // displaying this until the user makes a
                    // first pick.
                    Text("Choose…").tag(BandPreset?.none)
                    ForEach(bandPresets) { preset in
                        Text(preset.name).tag(BandPreset?.some(preset))
                    }
                } label: {
                    EmptyView()
                }
                .labelsHidden()
            }
        } header: {
            Text("Band")
        } footer: {
            Text("Quick-tune to a common band. Picking sets frequency, demod mode, and channel bandwidth.")
                .font(.caption)
        }
    }
}
