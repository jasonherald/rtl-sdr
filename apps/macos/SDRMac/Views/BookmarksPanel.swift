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

    /// User's manual per-category expansion preference —
    /// category name (or the `uncategorizedKey` sentinel)
    /// maps to `true` for expanded, `false` for collapsed.
    /// Persisted to UserDefaults so it survives across
    /// sessions; absent keys default to expanded.
    ///
    /// Kept SEPARATE from search-driven force-open: while
    /// `filterText` is non-empty, every matching group is
    /// rendered expanded regardless of this map, and the map
    /// is NOT updated from those force-open toggles. So
    /// clearing search snaps groups back to the user's
    /// manual preference — PR #361 round 2 caught a Major
    /// regression where the Linux side conflated the two
    /// states and collapsed groups stayed stuck open after
    /// search cleared.
    @State private var manuallyExpanded: [String: Bool] = [:]

    /// Sentinel key for bookmarks without an `rrCategory`.
    /// Matches the Linux `"Uncategorized"` title; used both
    /// as the DisclosureGroup label AND as the dictionary
    /// key above. Any real category label that happens to
    /// equal this string collides — acceptable risk; the
    /// Linux side makes the same call.
    private static let uncategorizedKey = "Uncategorized"

    /// UserDefaults key for `manuallyExpanded`. JSON-encoded
    /// `[String: Bool]` so the whole map round-trips in one
    /// value. Shape matches nothing on the Linux side yet —
    /// GTK state lives in widget memory, not config — so no
    /// cross-frontend parity concern here.
    private static let expansionStateKey = "SDRMac.bookmarks.manualExpansionState"

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
        .onAppear { loadManuallyExpanded() }
    }

    // MARK: - Search

    private var searchRow: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
                .font(.caption)
            TextField("Search bookmarks", text: $filterText)
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()
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
            } else if usesCategories {
                // Grouped rendering — one DisclosureGroup per
                // unique rrCategory, plus an "Uncategorized"
                // sentinel group if any bookmark lacks a
                // category. Mirrors the Linux GTK panel's
                // AdwExpanderRow grouping (#339).
                List {
                    ForEach(groupedBookmarks(filtered), id: \.0) { (category, bms) in
                        Section {
                            DisclosureGroup(isExpanded: expansionBinding(for: category)) {
                                ForEach(bms) { bm in
                                    BookmarkListRow(bookmark: bm)
                                }
                            } label: {
                                categoryHeader(category, count: bms.count)
                            }
                        }
                    }
                }
                .listStyle(.inset)
            } else {
                // Flat list when no bookmark has a category —
                // matches the Linux `uses_categories` fallback
                // so first-time users don't see a single
                // "Uncategorized" group swallowing the whole
                // list.
                List {
                    ForEach(filtered) { bm in
                        BookmarkListRow(bookmark: bm)
                    }
                }
                .listStyle(.inset)
            }
        }
    }

    /// `true` when at least one bookmark carries an
    /// `rrCategory`. Matches the Linux `uses_categories`
    /// check — only the grouped rendering path kicks in when
    /// categorization is actually in use; a fresh install
    /// with hand-saved bookmarks stays as a flat list.
    private var usesCategories: Bool {
        store.bookmarks.contains { $0.rrCategory != nil }
    }

    /// Group bookmarks by `rrCategory`, returning sorted
    /// tuples for stable UI rendering. Nil categories bucket
    /// into the `uncategorizedKey` sentinel at the end so
    /// named categories sort alphabetically at the top and
    /// the catch-all always lands last.
    private func groupedBookmarks(
        _ bookmarks: [Bookmark]
    ) -> [(String, [Bookmark])] {
        var buckets: [String: [Bookmark]] = [:]
        for bm in bookmarks {
            let key = bm.rrCategory ?? Self.uncategorizedKey
            buckets[key, default: []].append(bm)
        }
        // Stable category order: alphabetical, sentinel last.
        let named = buckets.keys
            .filter { $0 != Self.uncategorizedKey }
            .sorted()
        var ordered: [(String, [Bookmark])] = named.map { ($0, buckets[$0] ?? []) }
        if let uncat = buckets[Self.uncategorizedKey] {
            ordered.append((Self.uncategorizedKey, uncat))
        }
        return ordered
    }

    /// Header row for a category DisclosureGroup. Shows the
    /// category name + bookmark count; count label lets the
    /// user see at a glance how large a collapsed category is.
    private func categoryHeader(_ category: String, count: Int) -> some View {
        HStack(spacing: 6) {
            Text(category)
                .font(.callout)
                .fontWeight(.medium)
            Text("(\(count))")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    // MARK: - Expansion state (manual vs. search-forced)

    /// Build the `isExpanded` binding for a category.
    ///
    /// - Read: while search is active, always `true` so
    ///   every group is visible (the list is already filtered
    ///   to matches). While search is empty, consult the
    ///   user's persisted manual preference (default: expanded).
    /// - Write: only persist when search is empty. Writes
    ///   during search would capture the force-open state as
    ///   manual intent and leave groups stuck open after
    ///   search clears — the exact regression PR #361 round
    ///   2 caught on the Linux side.
    private func expansionBinding(for category: String) -> Binding<Bool> {
        Binding(
            get: {
                if !filterText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                    return true
                }
                return manuallyExpanded[category] ?? true
            },
            set: { newValue in
                guard filterText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                    return
                }
                manuallyExpanded[category] = newValue
                persistManuallyExpanded()
            }
        )
    }

    private func loadManuallyExpanded() {
        guard let json = UserDefaults.standard.string(forKey: Self.expansionStateKey),
              let data = json.data(using: .utf8),
              let decoded = try? JSONDecoder().decode([String: Bool].self, from: data) else {
            return
        }
        manuallyExpanded = decoded
    }

    private func persistManuallyExpanded() {
        guard let data = try? JSONEncoder().encode(manuallyExpanded),
              let json = String(data: data, encoding: .utf8) else { return }
        UserDefaults.standard.set(json, forKey: Self.expansionStateKey)
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
    @Environment(BookmarksStore.self) private var store
    let bookmark: Bookmark

    var body: some View {
        HStack(spacing: 8) {
            // Tap on the leading area applies the bookmark —
            // split from the trailing delete button so the
            // "click to recall" affordance stays obvious even
            // when the delete icon is close by.
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

            // Trailing per-row delete. Icon-only so it doesn't
            // crowd the row, with both a tooltip AND an
            // accessibility label — PR #361 round 1 flagged
            // icon-only buttons missing both as a Minor
            // finding (#3120080308, #3120155767). Deletes by
            // UUID so duplicates-with-same-name only drop the
            // clicked row, not every match.
            Button {
                store.remove(id: bookmark.id)
            } label: {
                Image(systemName: "trash")
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.borderless)
            .help("Delete bookmark")
            .accessibilityLabel("Delete bookmark")
        }
        // Right-click context menu — second surface for
        // delete, matches the pre-refactor sidebar behavior.
        .contextMenu {
            Button(role: .destructive) {
                store.remove(id: bookmark.id)
            } label: {
                Label("Delete", systemImage: "trash")
            }
        }
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
