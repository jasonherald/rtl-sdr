//
// RadioReferenceDialog.swift — modal sheet for the RadioReference
// frequency lookup (issue #241).
//
// Mirrors the GTK UI's mount point: a 700×600 modal dialog
// triggered by a header-bar button (see
// `RadioReferenceToolbarButton`). The button is only visible
// when credentials are saved, so this view assumes the keyring
// has them by the time it renders.
//
// Flow (unchanged from the earlier sidebar version — only the
// chrome moved):
//   1. ZIP entry + Search
//   2. Async detached search via `SdrCore.searchRadioReference`
//   3. Category + Agency filters populated from the result set
//   4. Checkbox-select rows → "Import Selected (N)" bulk-adds
//      bookmarks with mapped demod mode + bandwidth
//   5. On successful import, the sheet auto-closes (matches GTK)
//
// The engine-side crate (`sdr-radioreference`) does the HTTP;
// we surface it through three handle-free FFI functions
// documented in `SdrCoreRadioReference.swift`.

import SwiftUI
import SdrCoreKit

struct RadioReferenceDialog: View {
    @Environment(CoreModel.self) private var model
    @Environment(BookmarksStore.self) private var bookmarksStore
    @Environment(\.dismiss) private var dismiss

    @State private var zipInput: String = ""
    @State private var isSearching: Bool = false
    @State private var searchResult: SdrCore.RadioReferenceSearchResult?
    @State private var statusMessage: String = ""
    @State private var statusIsError: Bool = false

    /// Rows the user has checked for import. Keyed by
    /// `RadioReferenceFrequency.id` — stable per row within a
    /// single search, reset on every new search.
    @State private var selectedIds: Set<String> = []

    /// User-chosen category filter; `""` means "All".
    @State private var categoryFilter: String = ""

    /// User-chosen agency (alpha_tag) filter; `""` means "All".
    @State private var agencyFilter: String = ""

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            searchBar
            if isSearching {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Searching RadioReference…")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
            }
            if !statusMessage.isEmpty {
                Text(statusMessage)
                    .font(.callout)
                    .foregroundStyle(statusIsError ? .red : .secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 6)
            }
            if let result = searchResult {
                filtersBar(for: result)
                Divider()
                resultsList(for: result)
                Divider()
                footer(for: result)
            } else {
                // Pin the header/search bar to the top in the
                // empty / searching states. Without this,
                // `VStack` centers its content vertically in
                // the fixed 600-pt frame and the header
                // visibly jumps downward while loading, then
                // snaps back up once the results fill in.
                Spacer()
            }
        }
        .frame(width: 700, height: 600, alignment: .top)
    }

    // MARK: - Subviews

    private var header: some View {
        HStack {
            Label("RadioReference", systemImage: "antenna.radiowaves.left.and.right")
                .font(.headline)
            Spacer()
            Button("Close") { dismiss() }
                .keyboardShortcut(.cancelAction)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private var searchBar: some View {
        HStack(spacing: 8) {
            TextField("ZIP code (e.g. 90210)", text: $zipInput)
                .textFieldStyle(.roundedBorder)
                .disabled(isSearching)
                .onSubmit { triggerSearch() }
                .frame(maxWidth: 200)
            Button {
                triggerSearch()
            } label: {
                Label("Search", systemImage: "magnifyingglass")
            }
            .keyboardShortcut(.defaultAction)
            .disabled(!zipIsValid || isSearching)
            Spacer()
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }

    private func filtersBar(for result: SdrCore.RadioReferenceSearchResult) -> some View {
        HStack(spacing: 12) {
            Picker("Category", selection: $categoryFilter) {
                Text("All").tag("")
                ForEach(uniqueCategories(in: result), id: \.self) { category in
                    Text(category).tag(category)
                }
            }
            .frame(maxWidth: 260)

            Picker("Agency", selection: $agencyFilter) {
                Text("All").tag("")
                ForEach(uniqueAgencies(in: result), id: \.self) { agency in
                    Text(agency).tag(agency)
                }
            }
            .frame(maxWidth: 260)

            Spacer()

            let filtered = filteredFrequencies(in: result)
            Text("\(filtered.count) / \(result.frequencies.count)")
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
    }

    private func resultsList(for result: SdrCore.RadioReferenceSearchResult) -> some View {
        let filtered = filteredFrequencies(in: result)
        return ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(filtered) { row in
                    RadioReferenceRow(
                        frequency: row,
                        isSelected: selectedIds.contains(row.id),
                        alreadyBookmarked: alreadyBookmarked(row),
                        toggle: {
                            if selectedIds.contains(row.id) {
                                selectedIds.remove(row.id)
                            } else {
                                selectedIds.insert(row.id)
                            }
                        }
                    )
                    Divider()
                }
            }
        }
    }

    private func footer(for result: SdrCore.RadioReferenceSearchResult) -> some View {
        let filtered = filteredFrequencies(in: result)
        let importable = filtered.filter {
            selectedIds.contains($0.id) && !alreadyBookmarked($0)
        }
        return HStack {
            Text("\(result.city), \(result.countyName)")
                .font(.callout)
                .foregroundStyle(.secondary)
            Spacer()
            Button {
                importSelected(importable)
            } label: {
                if importable.isEmpty {
                    Text("Import Selected")
                } else {
                    Text("Import Selected (\(importable.count))")
                }
            }
            .buttonStyle(.borderedProminent)
            .disabled(importable.isEmpty)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }

    // MARK: - Actions

    private func triggerSearch() {
        guard zipIsValid else { return }
        // Clear the previous result set before kicking off the
        // next search. Without this, the old rows + their
        // import action stay live while the spinner runs — a
        // user could import stale bookmarks from the previous
        // ZIP mid-search. Per CodeRabbit round 1 on PR #346.
        selectedIds = []
        categoryFilter = ""
        agencyFilter = ""
        statusMessage = ""
        statusIsError = false
        searchResult = nil
        isSearching = true

        let zip = zipInput
        Task.detached(priority: .userInitiated) {
            // Resolve credentials. Three outcomes:
            //   - stored OK       → proceed
            //   - not stored      → "open Settings → RadioReference"
            //   - keychain threw  → surface the underlying error
            let creds: (user: String, password: String)
            do {
                guard let pair = try SdrCore.loadRadioReferenceCredentials() else {
                    await MainActor.run {
                        self.isSearching = false
                        self.searchResult = nil
                        self.statusMessage =
                            "No stored credentials. Open Settings → RadioReference to add them."
                        self.statusIsError = true
                    }
                    return
                }
                creds = pair
            } catch let err as SdrCoreError {
                await MainActor.run {
                    self.isSearching = false
                    self.searchResult = nil
                    self.statusMessage = "Couldn't read keychain: \(err.message)"
                    self.statusIsError = true
                }
                return
            } catch {
                await MainActor.run {
                    self.isSearching = false
                    self.searchResult = nil
                    self.statusMessage = "Unexpected keychain error: \(error.localizedDescription)"
                    self.statusIsError = true
                }
                return
            }


            do {
                let result = try SdrCore.searchRadioReference(
                    user: creds.user,
                    password: creds.password,
                    zip: zip
                )
                await MainActor.run {
                    self.isSearching = false
                    self.searchResult = result
                    if result.frequencies.isEmpty {
                        self.statusMessage = "No frequencies found for \(result.city), \(result.countyName)."
                        self.statusIsError = true
                    } else {
                        self.statusMessage = ""
                    }
                }
            } catch let err as SdrCoreError {
                await MainActor.run {
                    self.isSearching = false
                    self.searchResult = nil
                    self.statusMessage = Self.friendlyMessage(for: err)
                    self.statusIsError = true
                }
            } catch {
                await MainActor.run {
                    self.isSearching = false
                    self.searchResult = nil
                    self.statusMessage = "Search failed: \(error.localizedDescription)"
                    self.statusIsError = true
                }
            }
        }
    }

    private func importSelected(_ rows: [SdrCore.RadioReferenceFrequency]) {
        for row in rows {
            let bookmark = Self.makeBookmark(from: row)
            bookmarksStore.add(bookmark)
        }
        // GTK auto-closes the dialog after import — match that so
        // the user lands back on the tuner and can click the
        // freshly-created bookmark.
        dismiss()
    }

    // MARK: - Helpers

    private var zipIsValid: Bool {
        zipInput.count == 5 && zipInput.allSatisfy(\.isNumber)
    }

    private func uniqueCategories(in result: SdrCore.RadioReferenceSearchResult) -> [String] {
        let all = result.frequencies.map(\.category).filter { !$0.isEmpty }
        return Array(Set(all)).sorted()
    }

    private func uniqueAgencies(in result: SdrCore.RadioReferenceSearchResult) -> [String] {
        let all = result.frequencies.map(\.alphaTag).filter { !$0.isEmpty }
        return Array(Set(all)).sorted()
    }

    private func filteredFrequencies(
        in result: SdrCore.RadioReferenceSearchResult
    ) -> [SdrCore.RadioReferenceFrequency] {
        result.frequencies.filter { row in
            (categoryFilter.isEmpty || row.category == categoryFilter)
                && (agencyFilter.isEmpty || row.alphaTag == agencyFilter)
        }
    }

    /// Matches on freq + demod mode — two RR rows at the same
    /// frequency but different modes (rare but possible for
    /// AM/USB overlays) stay distinct.
    private func alreadyBookmarked(_ row: SdrCore.RadioReferenceFrequency) -> Bool {
        let rowHz = Double(row.freqHz)
        let mode = DemodMode(engineLabel: row.demodMode)
        return bookmarksStore.bookmarks.contains { b in
            b.centerFrequencyHz == rowHz && b.demodMode == mode
        }
    }

    private static func makeBookmark(from row: SdrCore.RadioReferenceFrequency) -> Bookmark {
        let name: String
        if row.alphaTag.isEmpty {
            name = row.description.isEmpty
                ? String(format: "%.4f MHz", Double(row.freqHz) / 1_000_000)
                : row.description
        } else if row.description.isEmpty {
            name = row.alphaTag
        } else {
            name = "\(row.alphaTag) — \(row.description)"
        }

        let demod = DemodMode(engineLabel: row.demodMode)
        return Bookmark(
            name: name,
            centerFrequencyHz: Double(row.freqHz),
            demodMode: demod,
            bandwidthHz: row.bandwidthHz,
            squelchEnabled: nil,
            autoSquelchEnabled: nil,
            squelchDb: nil,
            gainDb: nil,
            agcEnabled: nil,
            volume: nil,
            deemphasis: nil
        )
    }

    private static func friendlyMessage(for err: SdrCoreError) -> String {
        switch err.code {
        case .auth:
            return "Invalid credentials — update them in Settings → RadioReference."
        case .io:
            return "Network error: \(err.message)"
        case .invalidArg:
            return "Invalid input: \(err.message)"
        default:
            return "Search failed: \(err.message)"
        }
    }
}

/// One row in the results list. Bookmarked rows render with a
/// checkmark and are not toggleable — matches GTK where already-
/// imported rows are disabled.
private struct RadioReferenceRow: View {
    let frequency: SdrCore.RadioReferenceFrequency
    let isSelected: Bool
    let alreadyBookmarked: Bool
    let toggle: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            if alreadyBookmarked {
                Image(systemName: "checkmark.circle.fill")
                    .foregroundStyle(.green)
                    .frame(width: 18)
                    .accessibilityLabel("Already bookmarked")
            } else {
                Button(action: toggle) {
                    Image(systemName: isSelected ? "checkmark.square.fill" : "square")
                        .foregroundStyle(isSelected ? Color.accentColor : Color.secondary)
                        .frame(width: 18)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(isSelected ? "Deselect" : "Select for import")
            }

            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.callout)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 6)
        .padding(.horizontal, 16)
        .contentShape(Rectangle())
        .onTapGesture {
            if !alreadyBookmarked { toggle() }
        }
        .opacity(alreadyBookmarked ? 0.55 : 1.0)
    }

    private var title: String {
        frequency.alphaTag.isEmpty ? frequency.description : frequency.alphaTag
    }

    private var subtitle: String {
        var parts: [String] = []
        let mhz = Double(frequency.freqHz) / 1_000_000
        parts.append(String(format: "%.4f MHz", mhz))
        parts.append(frequency.demodMode)
        if let tone = frequency.toneHz {
            parts.append(String(format: "PL %.1f", tone))
        }
        if !frequency.description.isEmpty && !frequency.alphaTag.isEmpty {
            parts.append(frequency.description)
        }
        return parts.joined(separator: "  ·  ")
    }
}
