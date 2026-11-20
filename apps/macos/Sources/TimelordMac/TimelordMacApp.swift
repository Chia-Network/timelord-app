import AppKit
import SwiftUI
import TimelordPresentation

@main
struct TimelordMacApp: App {
    @NSApplicationDelegateAdaptor(ApplicationDelegate.self) private var delegate
    var body: some Scene { TimelordScenes(model: delegate.model) }
}

private struct TimelordScenes: Scene {
    @ObservedObject var model: TimelordModel
    var body: some Scene {
        // Menu-bar open requests should focus the single dashboard window.
        Window("Timelord", id: "main") { Overview(model: model) }
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
            Image(nsImage: MenuBarIcon.image(for: MenuBarGlyph(snapshot: model.snapshot)))
                .renderingMode(.template)
                .accessibilityLabel("Timelord, \(model.snapshot.phase.title)")
        }
            .menuBarExtraStyle(.window)
    }
}
