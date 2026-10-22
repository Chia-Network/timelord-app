import AppKit

/// All normal quit requests, including Dock Quit and logout, use one shutdown path.
@MainActor
final class ApplicationDelegate: NSObject, NSApplicationDelegate {
    let model: TimelordModel
    private let reply: (Bool) -> Void
    private var terminating = false
    private var readyToTerminate = false

    override init() {
        model = TimelordModel()
        reply = { NSApplication.shared.reply(toApplicationShouldTerminate: $0) }
        super.init()
    }

    init(model: TimelordModel, reply: @escaping (Bool) -> Void) {
        self.model = model
        self.reply = reply
        super.init()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if readyToTerminate { return .terminateNow }
        guard !terminating else { return .terminateLater }
        terminating = true
        Task {
            await model.prepareForTermination()
            readyToTerminate = true
            reply(true)
        }
        return .terminateLater
    }
}
