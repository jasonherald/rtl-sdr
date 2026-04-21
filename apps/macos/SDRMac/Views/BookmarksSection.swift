//
// BookmarksSection.swift — sidebar "quick add" affordance for
// saving the current tuning as a bookmark (#339 / #240).
//
// Previously hosted the full bookmark list with delete,
// reorder, and apply actions. Those moved to the right-side
// `BookmarksPanel` flyout (toggled from the header toolbar /
// ⌘B) in the #339 retool so the sidebar stays focused on
// setup/config and the right flyout handles browse/manage.
//
// What remains here is just the quick-save entry point:
// a "Save current" button that opens a sheet prompting for
// a name. The button stays in the sidebar — not moved into
// the flyout — so users who keep the flyout closed don't
// need to open it just to stash a frequency. Matches the
// Linux GTK split where the sidebar's Navigation panel keeps
// the name-entry + Add button while the flyout is the
// browse surface (bookmarks_panel.rs header comment).

import SwiftUI
import SdrCoreKit

struct BookmarksSection: View {
    @Environment(CoreModel.self) private var model
    @Environment(BookmarksStore.self) private var store

    @State private var showingAddSheet = false

    var body: some View {
        Section("Bookmarks") {
            Button {
                showingAddSheet = true
            } label: {
                Label("Save current", systemImage: "plus.circle")
            }

            // Tiny caption so first-time users know where the
            // list went. Only shown when nothing's saved yet,
            // so the sidebar stays terse once the user has
            // stashed a frequency or two.
            if store.bookmarks.isEmpty {
                Text("Open the Bookmarks panel (⌘B) to browse saved frequencies.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .sheet(isPresented: $showingAddSheet) {
            AddBookmarkSheet { name in
                let bm = model.snapshotBookmark(name: name)
                store.add(bm)
            }
        }
    }
}

// ============================================================
//  AddBookmarkSheet
// ============================================================
//
//  Tiny modal: just a name field pre-filled with the current
//  formatted frequency. Submit saves; Cancel dismisses. Kept
//  in-file (vs. hoisted to its own file) because it's tightly
//  coupled to this section's "Save current" button — the only
//  caller.

private struct AddBookmarkSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Environment(CoreModel.self) private var model

    let onSave: (String) -> Void

    @State private var name: String = ""
    @FocusState private var nameFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Save bookmark")
                .font(.headline)

            TextField("Name", text: $name)
                .textFieldStyle(.roundedBorder)
                .focused($nameFocused)
                .onSubmit(submit)

            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Save", action: submit)
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(minWidth: 340)
        .onAppear {
            // Default name: current formatted frequency.
            name = formatRate(model.centerFrequencyHz)
            nameFocused = true
        }
    }

    private func submit() {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        onSave(trimmed)
        dismiss()
    }
}
