//
// BookmarksPanel.swift — right-side slide-out panel listing
// saved tuning profiles (#339).
//
// Mirrors the Linux GTK `bookmarks_panel.rs` layout: a 360pt
// wide column with a header (title + close button), a search
// row, a scrolling list of bookmark rows grouped by category
// via `DisclosureGroup`, and an empty-state caption when no
// bookmarks are saved. The sidebar's `BookmarksSection` keeps
// only the quick-add affordance (name entry + Save button) —
// browse / search / manage all live here.
//
// Toggled from the header toolbar bookmark button or `⌘B`.
// Mutual-exclusive with the transcription panel (the sidebar
// has room for one right-side flyout at a time). Open/closed
// state persists across launches via `UserDefaults`.
//
// This commit ships the shell + empty state + header chrome.
// Search row, category grouping, and per-row actions land in
// the next two commits.

import SwiftUI
// `DemodMode.label` lives in SdrCoreKit — used by the row
// subtitle renderer below.
import SdrCoreKit

struct BookmarksPanel: View {
    @Environment(CoreModel.self) private var model
    @Environment(BookmarksStore.self) private var store

    /// Close-button binding — hands control back to the
    /// header toolbar / keyboard shortcut state that owns the
    /// open/closed flag in `ContentView`.
    @Binding var isPresented: Bool

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            body_list
        }
        .frame(width: BookmarksPanel.width)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    /// Panel width. Matches the Linux flyout's 360 px — wide
    /// enough for a long nickname + tuner/gain subtitle + the
    /// apply/save/delete affordances without wrapping.
    static let width: CGFloat = 360

    // MARK: - Header

    private var header: some View {
        HStack(spacing: 6) {
            Text("Bookmarks")
                .font(.headline)
            Spacer()
            Button {
                isPresented = false
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.borderless)
            .help("Close Bookmarks Panel")
            .accessibilityLabel("Close Bookmarks Panel")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    // MARK: - List

    @ViewBuilder
    private var body_list: some View {
        if store.bookmarks.isEmpty {
            emptyState
        } else {
            // Placeholder pending search + category grouping
            // in the next commits. Flat List of names so the
            // shell is visually complete while we land the
            // richer behavior in pieces.
            List {
                ForEach(store.bookmarks) { bm in
                    BookmarkListRow(bookmark: bm)
                }
            }
            .listStyle(.inset)
        }
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Image(systemName: "bookmark")
                .font(.largeTitle)
                .foregroundStyle(.tertiary)
            Text("No bookmarks yet")
                .font(.headline)
                .foregroundStyle(.secondary)
            Text("Save the current tuning from the sidebar to start a list.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 24)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// ============================================================
//  BookmarkListRow — placeholder shell
// ============================================================
//
//  Minimal row for the shell commit. Displays name + frequency
//  subtitle + an "active" checkmark when this bookmark matches
//  the engine's current `activeBookmarkId`. Tap applies. Real
//  per-row actions (save-over, delete, context menu) land in a
//  follow-up commit.

struct BookmarkListRow: View {
    @Environment(CoreModel.self) private var model
    let bookmark: Bookmark

    var body: some View {
        Button {
            model.apply(bookmark)
        } label: {
            HStack(spacing: 8) {
                if isActive {
                    Image(systemName: "checkmark.circle.fill")
                        .foregroundStyle(Color.accentColor)
                        .font(.caption)
                } else {
                    Image(systemName: "bookmark")
                        .foregroundStyle(.secondary)
                        .font(.caption)
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(bookmark.name)
                        .lineLimit(1)
                        .fontWeight(isActive ? .semibold : .regular)
                    Text(subtitle)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private var isActive: Bool {
        model.activeBookmarkId == bookmark.id
    }

    /// Format used by the frequency column. Matches the
    /// sidebar's pre-refactor format (`formatRate(_:)` in
    /// SourceSection) so users see identical strings across
    /// both surfaces. Falls back to the demod-mode label
    /// alone if the bookmark has no center frequency.
    private var subtitle: String {
        var parts: [String] = []
        if let hz = bookmark.centerFrequencyHz {
            parts.append(formatRate(hz))
        }
        if let mode = bookmark.demodMode {
            parts.append(mode.label)
        }
        return parts.joined(separator: " · ")
    }
}
