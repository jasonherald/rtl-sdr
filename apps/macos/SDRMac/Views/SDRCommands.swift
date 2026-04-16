//
// SDRCommands.swift — menu-bar commands attached to the main
// scene via `.commands { ... }`.
//
// Replaces the default File > New (we don't have a document model)
// and adds Radio (start/stop, tune nudge) and View menus.

import SwiftUI

struct SDRCommands: Commands {
    let core: CoreModel

    var body: some Commands {
        CommandGroup(replacing: .newItem) {}

        CommandMenu("Radio") {
            Button("Start") { core.start() }
                .keyboardShortcut("r", modifiers: .command)
                .disabled(core.isRunning)
            Button("Stop") { core.stop() }
                .keyboardShortcut(".", modifiers: .command)
                .disabled(!core.isRunning)
            Divider()
            Button("Tune Up 100 kHz") {
                core.setCenter(core.centerFrequencyHz + 100_000)
            }
            .keyboardShortcut(.upArrow, modifiers: .command)
            Button("Tune Down 100 kHz") {
                core.setCenter(core.centerFrequencyHz - 100_000)
            }
            .keyboardShortcut(.downArrow, modifiers: .command)
        }

        CommandGroup(after: .toolbar) {
            Button("Toggle Sidebar") {
                NSApp.keyWindow?.firstResponder?.tryToPerform(
                    #selector(NSSplitViewController.toggleSidebar(_:)),
                    with: nil
                )
            }
            .keyboardShortcut("s", modifiers: [.command, .control])
        }
    }
}
