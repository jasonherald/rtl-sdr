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

    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate

    var body: some Scene {
        WindowGroup("SDR") {
            ContentView()
                .environment(core)
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
    private static func defaultConfigPath() -> URL {
        let fm = FileManager.default
        let dir = (try? fm.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )) ?? fm.homeDirectoryForCurrentUser
        let appDir = dir.appendingPathComponent("SDRMac")
        try? fm.createDirectory(at: appDir, withIntermediateDirectories: true)
        return appDir.appendingPathComponent("config.json")
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
