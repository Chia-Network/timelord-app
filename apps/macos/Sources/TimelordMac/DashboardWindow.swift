import AppKit

/// The one dashboard window, opened by both the menu bar and Dock reopen.
@MainActor
enum DashboardWindow {
    static let id = "main"

    /// Dock reopen only needs to act after the last visible window is closed.
    static func shouldShowDashboard(hasVisibleWindows: Bool) -> Bool {
        !hasVisibleWindows
    }

    static func show(in application: NSApplication? = nil) {
        let application = application ?? .shared
        application.activate(ignoringOtherApps: true)
        application.windows.first { $0.identifier?.rawValue == id }?.makeKeyAndOrderFront(nil)
    }
}
