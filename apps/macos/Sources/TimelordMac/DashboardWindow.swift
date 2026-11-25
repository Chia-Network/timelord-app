import AppKit
import SwiftUI

/// The one dashboard window, opened by both the menu bar and Dock reopen.
@MainActor
enum DashboardWindow {
    static let id = "main"
    private static var opener: (() -> Void)?

    /// SwiftUI's `openWindow` can create the scene after it was closed or
    /// restored closed. An NSWindow lookup cannot, so registration supplies
    /// the only way either caller shows the dashboard.
    static func register(_ open: (() -> Void)?) {
        opener = open
    }

    static func show() {
        opener?()
    }

    /// Return true even when a window is visible: AppKit uses that answer to
    /// restore a minimized dashboard. Only a hidden window needs an explicit show.
    @discardableResult
    static func reopen(hasVisibleWindows: Bool) -> Bool {
        if !hasVisibleWindows { show() }
        return true
    }
}

private struct DashboardWindowRegistration: ViewModifier {
    @Environment(\.openWindow) private var openWindow

    func body(content: Content) -> some View {
        content.onAppear {
            DashboardWindow.register {
                openWindow(id: DashboardWindow.id)
                NSApplication.shared.activate(ignoringOtherApps: true)
            }
        }
    }
}

extension View {
    /// Register SwiftUI's scene opener from a view that survives a closed dashboard.
    func registersDashboardWindow() -> some View {
        modifier(DashboardWindowRegistration())
    }
}
