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
            // Scaffolding compromise per `CodeRabbit` round 2
            // on PR #491: the legacy sidebar Form (Source +
            // Radio + Display + Recording + Bookmarks +
            // RtlTcpServer sections) hangs off the General
            // slot during scaffolding so the user retains
            // access to every existing control. Subsequent
            // sub-tickets carve sections OUT of this Form
            // and into their dedicated activity panels:
            //
            //  #443 → Source moves out (General panel proper)
            //  #444 → Radio moves out
            //  #445 → Audio (Recording) moves out
            //  #446 → Display moves out
            //  #447 → Scanner panel
            //
            // After all five land, this case will only carry
            // the band presets + the trimmed Source content.
            // Mirrors the Linux scaffolding decision in
            // `crates/sdr-ui/src/window.rs`.
            LegacySidebarPanel()
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

/// Scaffolding-only stand-in for the General slot — keeps the
/// pre-redesign sidebar Form (with every existing section)
/// reachable while subsequent sub-tickets carve sections out
/// into dedicated activity panels. Verbatim copy of the
/// pre-#442 `SidebarView` body so the user loses nothing
/// during the redesign transition.
///
/// Each carve-out sub-ticket (#443–#447) deletes the
/// corresponding `*Section()` line from this body and stands
/// up the matching panel in `LeftPanelHost`. When all five
/// have landed, this struct can be deleted entirely.
struct LegacySidebarPanel: View {
    var body: some View {
        Form {
            SourceSection()
            RadioSection()
            DisplaySection()
            RecordingSection()
            BookmarksSection()
            // `RtlTcpServerSection` is visible only when a
            // local RTL-SDR dongle is detected — the section
            // itself is always included in the form, but the
            // body collapses to a single "no dongle" caption
            // otherwise so it doesn't clutter the sidebar on
            // a network/file source setup.
            RtlTcpServerSection()
        }
        .formStyle(.grouped)
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
