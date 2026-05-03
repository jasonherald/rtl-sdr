//
// BookmarksStore.swift — observable store for the sidebar
// bookmarks list. Persists to a standalone JSON file in the
// app support directory (not round-tripped through sdr-config's
// main config; matches the GTK side's file split).
//
// File path: ~/Library/Application Support/SDRMac/bookmarks.json
//
// Storage is pull / push in one: `load()` at bootstrap, `save()`
// after every mutation. JSON is small (≤ a few KB for typical
// ham-radio bookmark counts) so there's no need for an async
// diff / incremental save pattern.

import Foundation
import Observation
import OSLog
// `Array.move(fromOffsets:toOffset:)` lives in SwiftUI — it's
// the helper SwiftUI's `onMove` reorder action expects.
import SwiftUI

private let bookmarksLog = Logger(subsystem: "com.sdr.rs", category: "bookmarks")

@MainActor
@Observable
final class BookmarksStore {
    /// User's saved bookmarks, ordered by the user's arrangement.
    /// Mutations persist to disk via `save()` after the in-memory
    /// list is updated.
    private(set) var bookmarks: [Bookmark] = []

    /// File path under Application Support. Resolved lazily so a
    /// test harness can override; production uses
    /// `SDRMacApp.defaultBookmarksPath()`.
    private let storagePath: URL

    /// Fires after every successful `save()` (i.e., after any
    /// add / remove / update / move / scan-toggle / priority-
    /// toggle). The scene wires this to
    /// `CoreModel.refreshScannerChannels()` so the engine's
    /// scanner rotation tracks the bookmark list automatically.
    /// Per #490.
    ///
    /// Stored as a closure rather than reaching into `CoreModel`
    /// directly so this store stays decoupled from the engine —
    /// makes the unit tests on the JSON round-trip / mutation
    /// paths trivial without needing a `CoreModel` fixture.
    var onChanged: (() -> Void)?

    init(storagePath: URL) {
        self.storagePath = storagePath
        load()
    }

    // ----------------------------------------------------------
    //  Mutations — each one saves to disk
    // ----------------------------------------------------------

    func add(_ bookmark: Bookmark) {
        bookmarks.append(bookmark)
        save()
    }

    func remove(id: UUID) {
        bookmarks.removeAll { $0.id == id }
        save()
    }

    func update(_ bookmark: Bookmark) {
        guard let i = bookmarks.firstIndex(where: { $0.id == bookmark.id }) else { return }
        var updated = bookmark
        updated.updatedAt = Date()
        bookmarks[i] = updated
        save()
    }

    func move(fromOffsets source: IndexSet, toOffset destination: Int) {
        bookmarks.move(fromOffsets: source, toOffset: destination)
        save()
    }

    /// Toggle a bookmark's `scanEnabled` flag in place. Doesn't
    /// touch `updatedAt` — the timestamp tracks tuning-state
    /// edits (frequency, demod, etc.), and a scanner-membership
    /// flip isn't that. Avoids gratuitous re-sorts on the
    /// flyout's "recent" view. Per #490.
    func setScanEnabled(id: UUID, enabled: Bool) {
        guard let i = bookmarks.firstIndex(where: { $0.id == id }) else { return }
        bookmarks[i].scanEnabled = enabled
        save()
    }

    /// Toggle a bookmark's priority tier — `true` writes `1`,
    /// `false` clears to `nil` (so the JSON omits the key for
    /// never-promoted bookmarks, matching the Linux serde
    /// default shape). Same `updatedAt`-untouched policy as
    /// `setScanEnabled` above. Per #490.
    func setPriorityEnabled(id: UUID, enabled: Bool) {
        guard let i = bookmarks.firstIndex(where: { $0.id == id }) else { return }
        bookmarks[i].priorityEnabled = enabled
        save()
    }

    // ----------------------------------------------------------
    //  Disk I/O
    // ----------------------------------------------------------

    private func load() {
        guard FileManager.default.fileExists(atPath: storagePath.path) else {
            // First launch — start empty, no error.
            return
        }
        do {
            let data = try Data(contentsOf: storagePath)
            let decoder = JSONDecoder()
            decoder.dateDecodingStrategy = .iso8601
            bookmarks = try decoder.decode([Bookmark].self, from: data)
        } catch {
            // Persist failure — don't crash the app; log and
            // start empty. A future launch with a hand-fixed
            // file takes precedence.
            // `localizedDescription` can include the absolute file
            // path (e.g. `/Users/<name>/Library/...`). Leave the
            // default `.private` privacy so the path doesn't leak
            // to unified logs.
            bookmarksLog.error("Failed to load bookmarks: \(error.localizedDescription)")
        }
    }

    private func save() {
        do {
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            encoder.dateEncodingStrategy = .iso8601
            let data = try encoder.encode(bookmarks)
            // Ensure the parent directory exists; app support
            // subfolder is created at app-init but be safe if
            // someone hand-deletes it.
            try FileManager.default.createDirectory(
                at: storagePath.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            try data.write(to: storagePath, options: .atomic)
        } catch {
            bookmarksLog.error("Failed to save bookmarks: \(error.localizedDescription)")
            // Fall through — `onChanged` still fires below
            // because the in-memory state did update; the
            // on-disk state diverging on an IO error is a
            // separate problem the logger has already
            // surfaced.
        }
        onChanged?()
    }
}
