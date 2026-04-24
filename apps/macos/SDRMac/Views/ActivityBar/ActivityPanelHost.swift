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
            ComingSoonPanel(
                activity: activity,
                followUpTicket: "#443 — General panel (band + source)"
            )
        case .radio:
            ComingSoonPanel(
                activity: activity,
                followUpTicket: "#444 — Radio panel"
            )
        case .audio:
            ComingSoonPanel(
                activity: activity,
                followUpTicket: "#445 — Audio panel + volume persistence"
            )
        case .display:
            ComingSoonPanel(
                activity: activity,
                followUpTicket: "#446 — Display panel"
            )
        case .scanner:
            ComingSoonPanel(
                activity: activity,
                followUpTicket: "#447 — Scanner panel"
            )
        case .share:
            // Share = rtl_tcp server + client + discovery. The
            // existing RtlTcpServerSection / SourceSection
            // rtl_tcp arm fills this slot once #447/#443 port
            // their content.
            ComingSoonPanel(
                activity: activity,
                followUpTicket: "#443/#447 — rtl_tcp share (server + client)"
            )
        }
    }
}

/// Right panel host — one activity during scaffolding
/// (`#442`); Bookmarks lands in `#448`.
struct RightPanelHost: View {
    let activity: RightActivity

    var body: some View {
        switch activity {
        case .transcript:
            ComingSoonPanel(
                activity: activity,
                followUpTicket: "#448 — Transcript + right activity bar"
            )
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
