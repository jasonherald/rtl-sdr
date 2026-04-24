//
// ActivityPanelHost.swift — renders the panel body for the
// currently-selected activity. Scaffolding only: every slot
// shows a `ComingSoonPanel` placeholder with the activity's
// label + icon + a pointer to the sub-ticket that will fill
// it in.
//
// Subsequent sub-tickets (#443–#447, #448) replace each
// placeholder's branch with the real panel view.
//
// Per epic #441 and sub-ticket #442.

import SwiftUI

/// Left panel host — switches on `LeftActivity` to pick which
/// view renders as the currently-selected left panel.
struct LeftPanelHost: View {
    let activity: LeftActivity

    var body: some View {
        switch activity {
        case .general:
            // First panel ported under the redesign (#443).
            // Hosts band presets + the existing Source section.
            GeneralPanelView()

        // Activities below: each routes directly to its
        // matching legacy section wrapped in a Form during the
        // intermediate state. Clicking the icon does what the
        // icon's label says — Radio shows radio controls,
        // Display shows display controls, etc. — instead of a
        // generic placeholder. Subsequent sub-tickets upgrade
        // each arm in place from a single-section host to a
        // proper multi-section `<X>PanelView` without changing
        // what the user sees on first click.
        case .radio:
            // #444 — five flat sections (Bandwidth / Squelch /
            // Filters / De-emphasis / CTCSS placeholder).
            RadioPanelView()
        case .audio:
            // #445 — Output / Volume / Network sink / Recording.
            AudioPanelView()
        case .display:
            // #446 — FFT / Waterfall placeholder / Levels /
            // Appearance.
            DisplayPanelView()
        case .scanner:
            // #447 deferred: the Mac CoreModel doesn't expose
            // any scanner state yet (the engine's `sdr-scanner`
            // crate has no FFI surface — the C ABI still
            // needs the matching commands + events). Until
            // that lands, clicking Scanner shows a clear
            // placeholder.
            ComingSoonPanel(
                activity: activity,
                followUpTicket: "#447 — Scanner panel (blocked on FFI surface)"
            )
        case .share:
            // Share = rtl_tcp server (and eventually client +
            // discovery). The existing server panel slots in
            // here cleanly; client UI follows in a separate
            // ticket.
            LegacySectionPanel { RtlTcpServerSection() }
        }
    }
}

/// One-section host that wraps an existing pre-redesign
/// `*Section` view inside a grouped Form. Used by
/// `LeftPanelHost` to give every activity a panel that looks
/// like the eventual rich `<X>PanelView` even when only one
/// section is wired up. Each carve-out sub-ticket replaces
/// the single-section call with a proper panel view that
/// composes multiple sections.
struct LegacySectionPanel<Content: View>: View {
    @ViewBuilder let content: () -> Content

    var body: some View {
        Form {
            content()
        }
        .formStyle(.grouped)
    }
}

/// Right panel host — landed both transcript and bookmarks
/// in `#448`. Takes an `isOpen` binding so the bookmarks
/// panel's close button can drive the activity-bar's
/// open/closed state without the panel needing to know
/// about the activity bar.
struct RightPanelHost: View {
    let activity: RightActivity
    @Binding var isOpen: Bool

    var body: some View {
        switch activity {
        case .transcript:
            // Transcription panel doesn't have an in-panel
            // close button — the right activity bar's
            // Transcript icon is the close affordance.
            TranscriptionPanel()
        case .bookmarks:
            // BookmarksPanel ships its own X close button
            // bound to `isPresented`; routing that binding
            // to `isOpen` makes the close button toggle the
            // activity bar's state directly.
            BookmarksPanel(isPresented: $isOpen)
        }
    }
}

/// Placeholder body for any activity slot whose real content
/// hasn't been ported yet. Shows the activity's icon + label
/// prominently plus a small pointer to the sub-ticket that
/// will fill it in, so anyone running the intermediate build
/// knows what's missing and why.
private struct ComingSoonPanel<Activity: ActivityEntry>: View {
    let activity: Activity
    let followUpTicket: String

    var body: some View {
        VStack(spacing: 12) {
            Spacer()
            Image(systemName: activity.systemImage)
                .font(.system(size: 48, weight: .light))
                .foregroundStyle(.secondary)
            Text(activity.label)
                .font(.title2)
                .fontWeight(.medium)
            Text("Coming in \(followUpTicket).")
                .font(.caption)
                .foregroundStyle(.tertiary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 20)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .windowBackgroundColor))
    }
}
