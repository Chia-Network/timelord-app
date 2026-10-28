import AppKit
import XCTest
@testable import TimelordMac
import TimelordPresentation

@MainActor
final class TerminationTests: XCTestCase {
    func testSystemQuitRepliesOnceAndLeavesIndependentWorkerRunning() async throws {
        let fixture = try SupervisedFixture()
        defer { fixture.close() }
        try await fixture.launch()
        let pid = try XCTUnwrap(fixture.controller.processID)
        let model = TimelordModel(controller: fixture.controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        XCTAssertEqual(model.quitLabel, "Quit interface")
        var replies = [Bool]()
        let delegate = ApplicationDelegate(model: model, reply: { replies.append($0) })
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateLater)
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateLater)
        let deadline = Date().addingTimeInterval(5)
        while replies.isEmpty && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
        XCTAssertEqual(replies, [true])
        await fixture.controller.refresh()
        XCTAssertEqual(fixture.controller.processID, pid)
        XCTAssertEqual(kill(pid, 0), 0)
        XCTAssertFalse(model.canStartService)
        model.startService()
        XCTAssertFalse(model.starting)
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateNow)
    }

    func testQuitWithNoSupervisorCannotLaunchAWorker() async throws {
        let fixture = try SupervisedFixture()
        defer { fixture.close() }
        let model = TimelordModel(controller: fixture.controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        await model.prepareForTermination()
        model.startService()
        XCTAssertFalse(model.starting)
        XCTAssertFalse(fixture.controller.canControlService)
        XCTAssertFalse(fixture.process.isRunning)
    }

    func testReloadWhilePausedDoesNotTurnRunIntentOn() async throws {
        let fixture = try SupervisedFixture()
        defer { fixture.close() }
        try await fixture.launch()
        try await fixture.controller.stop()
        let model = TimelordModel(controller: fixture.controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        await model.restartService()
        XCTAssertNil(fixture.controller.processID)
        XCTAssertEqual(fixture.controller.supervisor?.mode, .paused)
        await model.prepareForTermination()
        XCTAssertEqual(fixture.controller.supervisor?.mode, .paused)
    }
}
