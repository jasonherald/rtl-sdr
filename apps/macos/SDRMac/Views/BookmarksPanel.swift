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

    /// Live search filter. Case-insensitive substring match
    /// against `name`, the formatted frequency subtitle, and
    /// `rrCategory`. Mirrors the Linux predicate in
    /// `navigation_panel.rs::bookmark_matches_filter` — PR
    /// #361 round 1 flagged category-missing as a Major
    /// finding; landing it from day one here.
    @State private var filterText: String = ""

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            searchRow
            Divider()
            body_list
        }
        .frame(width: BookmarksPanel.width)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    // MARK: - Search

    private var searchRow: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
                .font(.caption)
            TextField("Search bookmarks", text: $filterText)
                .textFieldStyle(.roundedBorder)
                .disableAutocorrection(true)
            if !filterText.isEmpty {
                Button {
                    filterText = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(.borderless)
                .help("Clear search")
                .accessibilityLabel("Clear search")
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    /// Filtered list reflecting `filterText`. Case-insensitive
    /// substring match against name, subtitle string, and
    /// `rrCategory`. Empty `filterText` passes everything
    /// through untouched.
    private var filteredBookmarks: [Bookmark] {
        let needle = filterText
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()
        guard !needle.isEmpty else { return store.bookmarks }
        return store.bookmarks.filter { Self.matchesFilter($0, needle: needle) }
    }

    /// Shared filter predicate — exposed as a `static` so
    /// future category-header renderers (next commit) can
    /// reuse the same match rules without diverging. Matches
    /// the Linux `bookmark_matches_filter` intent.
    static func matchesFilter(_ bm: Bookmark, needle: String) -> Bool {
        if needle.isEmpty { return true }
        if bm.name.lowercased().contains(needle) { return true }
        if Self.subtitleFor(bm).lowercased().contains(needle) { return true }
        if let cat = bm.rrCategory?.lowercased(), cat.contains(needle) { return true }
        return false
    }

    /// Formatted subtitle used by the filter predicate AND by
    /// `BookmarkListRow` below. Single source of truth so a
    /// row that visually matches "145.7 MHz · NFM" is also
    /// findable by searching that string.
    static func subtitleFor(_ bm: Bookmark) -> String {
        var parts: [String] = []
        if let hz = bm.centerFrequencyHz {
            parts.append(formatRate(hz))
        }
        if let mode = bm.demodMode {
            parts.append(mode.label)
        }
        return parts.joined(separator: " · ")
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
            emptyState(.empty)
        } else {
            let filtered = filteredBookmarks
            if filtered.isEmpty {
                emptyState(.noMatches)
            } else {
                // Flat list for this commit; category grouping
                // via DisclosureGroup lands in the next.
                List {
                    ForEach(filtered) { bm in
                        BookmarkListRow(bookmark: bm)
                    }
                }
                .listStyle(.inset)
            }
        }
    }

    /// Two flavors of empty state so the user gets a helpful
    /// prompt rather than a mysterious blank pane.
    private enum EmptyStateKind {
        case empty       // Zero bookmarks saved
        case noMatches   // Filter matches nothing
    }

    @ViewBuilder
    private func emptyState(_ kind: EmptyStateKind) -> some View {
        VStack(spacing: 8) {
            Image(systemName: kind == .empty ? "bookmark" : "magnifyingglass")
                .font(.largeTitle)
                .foregroundStyle(.tertiary)
            Text(kind == .empty ? "No bookmarks yet" : "No matches")
                .font(.headline)
                .foregroundStyle(.secondary)
            Text(
                kind == .empty
                    ? "Save the current tuning from the sidebar to start a list."
                    : "No bookmarks match \"\(filterText)\"."
            )
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

    /// Use the panel's shared subtitle formatter so the
    /// displayed string and the filter predicate always see
    /// the same text (a row visually labelled "145.7 MHz ·
    /// NFM" is findable by searching that string).
    private var subtitle: String {
        BookmarksPanel.subtitleFor(bookmark)
    }
}
