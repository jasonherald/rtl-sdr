//
// SDRMacApp.swift — main app entry point.
//
// Owns the single `CoreModel` @State instance and drops it into
// the environment so every view can read/write engine state.
// Windowing is `WindowGroup` (main window) plus `Settings`
// (Cmd-, scene). Menu bar commands live in `SDRCommands`.
//
// The engine handle itself is constructed lazily in
// `ContentView.task` — see `CoreModel.bootstrap(configPath:)`.
// The app struct does not block on engine init; the UI draws
// immediately with placeholder state and fills in as events
// arrive from the dispatcher thread.

import SwiftUI

@main
struct SDRMacApp: App {
    @State private var core = CoreModel()
    @State private var bookmarks = BookmarksStore(storagePath: SDRMacApp.defaultBookmarksPath())

    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate

    var body: some Scene {
        WindowGroup("SDR") {
            ContentView()
                .environment(core)
                .environment(bookmarks)
                .frame(minWidth: 900, minHeight: 600)
                .task {
                    await core.bootstrap(configPath: Self.defaultConfigPath())
                    appDelegate.model = core
                }
        }
        .windowToolbarStyle(.unified)
        .commands { SDRCommands(core: core) }

        Settings {
            SettingsView()
                .environment(core)
        }
    }

    /// `~/Library/Application Support/SDRMac/config.json`. Created
    /// on first launch so the engine can persist through it.
    /// Exposed as `internal` (not `private`) so `SettingsView`
    /// can render the live path instead of a hardcoded string —
    /// a bundle-id or layout change would otherwise make the
    /// displayed path drift from the real one.
    static func defaultConfigPath() -> URL {
        appSupportDirectory().appendingPathComponent("config.json")
    }

    /// `~/Library/Application Support/SDRMac/bookmarks.json`.
    /// Separate file from the engine's `config.json` so bookmark
    /// mutations don't round-trip through the engine and so
    /// bookmarks can survive a config schema change. Matches the
    /// GTK side's file split.
    static func defaultBookmarksPath() -> URL {
        appSupportDirectory().appendingPathComponent("bookmarks.json")
    }

    /// Directory the app writes WAV recordings to, per #239:
    /// `~/Documents/SDRMac/Audio/`. Created lazily on first
    /// recording start so a user who never hits Record doesn't
    /// wind up with a stray empty folder.
    ///
    /// Returned as a URL, not just a path, so SwiftUI callers can
    /// `showInFinder` the destination directly.
    static func audioRecordingsDirectory() -> URL {
        let fm = FileManager.default
        let docs = (try? fm.url(
            for: .documentDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )) ?? fm.homeDirectoryForCurrentUser.appendingPathComponent("Documents")
        let dir = docs.appendingPathComponent("SDRMac/Audio")
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private static func appSupportDirectory() -> URL {
        let fm = FileManager.default
        let dir = (try? fm.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )) ?? fm.homeDirectoryForCurrentUser
        let appDir = dir.appendingPathComponent("SDRMac")
        try? fm.createDirectory(at: appDir, withIntermediateDirectories: true)
        return appDir
    }
}

/// Hooks `applicationWillTerminate` so the engine gets a clean
/// shutdown (and the config gets persisted) on Cmd-Q. `@State`-
/// owned models don't get deterministic deinit on app quit, so
/// we rely on the delegate callback to drive teardown.
final class AppDelegate: NSObject, NSApplicationDelegate {
    var model: CoreModel?

    func applicationWillTerminate(_ notification: Notification) {
        model?.shutdown()
    }
}
