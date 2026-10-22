import AppKit
import SwiftUI
import TimelordPresentation

@main
struct TimelordMacApp: App {
    @StateObject private var model = TimelordModel()
    var body: some Scene {
        WindowGroup("Timelord", id: "main") { Overview(model: model) }
            .defaultSize(width: 1180, height: 820)
            .windowResizability(.contentMinSize)
            .commands {
                CommandGroup(replacing: .appTermination) {
                    Button(model.quitLabel) { model.stopAndQuit() }.keyboardShortcut("q")
                }
                CommandGroup(replacing: .appSettings) {
                    Button("Settings…") { model.sheet = .settings }.keyboardShortcut(",")
                }
            }
        MenuBarExtra {
            MenuStatus(model: model)
        } label: {
            Image(nsImage: MenuBarIcon.image(for: MenuBarGlyph(phase: model.snapshot.phase)))
                .renderingMode(.template)
                .accessibilityLabel("Timelord, \(model.snapshot.phase.title)")
        }
            .menuBarExtraStyle(.window)
    }
}
