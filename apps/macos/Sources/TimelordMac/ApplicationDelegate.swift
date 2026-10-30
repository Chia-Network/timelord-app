import AppKit

/// All normal quit requests, including Dock Quit and logout, use one shutdown path.
@MainActor
final class ApplicationDelegate: NSObject, NSApplicationDelegate {
    let model: TimelordModel
    private let reply: (Bool) -> Void
    private var terminating = false
    private var readyToTerminate = false
    private let shutdownTimeout: TimeInterval

    override init() {
        model = TimelordModel()
        shutdownTimeout = 30
        reply = { NSApplication.shared.reply(toApplicationShouldTerminate: $0) }
        super.init()
    }

    init(model: TimelordModel, shutdownTimeout: TimeInterval = 30, reply: @escaping (Bool) -> Void) {
        self.model = model
        self.shutdownTimeout = shutdownTimeout
        self.reply = reply
        super.init()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if readyToTerminate { return .terminateNow }
        guard !terminating else { return .terminateLater }
        terminating = true
        Task {
            let stopped = await model.prepareForTermination(timeout: shutdownTimeout)
            readyToTerminate = stopped
            terminating = false
            reply(stopped)
        }
        return .terminateLater
    }
}
