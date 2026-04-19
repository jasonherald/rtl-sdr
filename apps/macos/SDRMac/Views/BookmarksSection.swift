//
// BookmarksSection.swift — sidebar panel listing saved
// tuning profiles (#240).
//
// Simple CRUD on top of `BookmarksStore`:
// - "Add current" snapshots the model's tuning state.
// - Tapping a row applies that bookmark to the model.
// - Context menu / swipe offers delete.
// - Drag-to-reorder via SwiftUI's native `onMove` support.
//
// v1 intentionally excludes editing an existing bookmark's
// tuning fields in place — users can delete + re-save. An
// "Edit" sheet could come as a follow-up if users want name /
// field edits without re-tuning.

import SwiftUI
// `DemodMode.label` lives in SdrCoreKit — the enum is re-used
// by SwiftUI-side Bookmark rendering.
import SdrCoreKit

struct BookmarksSection: View {
    @Environment(CoreModel.self) private var model
    @Environment(BookmarksStore.self) private var store

    @State private var showingAddSheet = false

    var body: some View {
        Section("Bookmarks") {
            if store.bookmarks.isEmpty {
                Text("No saved frequencies")
                    .foregroundStyle(.secondary)
                    .font(.caption)
            } else {
                ForEach(store.bookmarks) { bm in
                    // Button (rather than `onTapGesture`) so
                    // keyboard / VoiceOver / switch-control
                    // activation works the same as a mouse
                    // click. `.plain` keeps the row from picking
                    // up button chrome.
                    Button {
                        model.apply(bm)
                    } label: {
                        BookmarkRow(bookmark: bm)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .contextMenu {
                        Button(role: .destructive) {
                            store.remove(id: bm.id)
                        } label: {
                            Label("Delete", systemImage: "trash")
                        }
                    }
                }
                .onMove { source, destination in
                    store.move(fromOffsets: source, toOffset: destination)
                }
            }

            Button {
                showingAddSheet = true
            } label: {
                Label("Save current", systemImage: "plus.circle")
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
//  BookmarkRow
// ============================================================

private struct BookmarkRow: View {
    let bookmark: Bookmark

    var body: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 2) {
                Text(bookmark.name)
                    .lineLimit(1)
                if let hz = bookmark.centerFrequencyHz {
                    HStack(spacing: 6) {
                        Text(formatRate(hz))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        if let mode = bookmark.demodMode {
                            Text(mode.label)
                                .font(.caption2)
                                .padding(.horizontal, 4)
                                .padding(.vertical, 1)
                                .background(
                                    RoundedRectangle(cornerRadius: 3)
                                        .fill(Color.secondary.opacity(0.15))
                                )
                        }
                    }
                }
            }
            Spacer()
        }
        .padding(.vertical, 2)
    }
}

// ============================================================
//  AddBookmarkSheet
// ============================================================
//
//  Tiny modal: just a name field pre-filled with the current
//  formatted frequency. Submit saves; Cancel dismisses. Full
//  editable-profile form is a v2 polish item.

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
