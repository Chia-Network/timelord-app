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
        XCTAssertFalse(model.isBusy)
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateNow)
    }

    func testQuitWithNoSupervisorCannotLaunchAWorker() async throws {
        let fixture = try SupervisedFixture()
        defer { fixture.close() }
        let model = TimelordModel(controller: fixture.controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        await model.prepareForTermination()
        model.startService()
        XCTAssertFalse(model.isBusy)
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
    func testPauseShowsCleanupAndSerializesControlsUntilAcknowledged() async throws {
        let fixture = try SupervisedFixture(workerBody: """
        trap 'sleep 1; exit 0' TERM
        touch "$0.ready"
        while :; do sleep 1; done
        """)
        defer { fixture.close() }
        try await fixture.launch()
        let ready = fixture.helper.path + ".ready"
        let deadline = Date().addingTimeInterval(5)
        while !FileManager.default.fileExists(atPath: ready) && Date() < deadline {
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: ready))
        let model = TimelordModel(controller: fixture.controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        let revision = try XCTUnwrap(fixture.controller.supervisor?.revision)
        model.stopService()
        XCTAssertTrue(model.isBusy)
        XCTAssertEqual(model.controlOperation, .pause)
        XCTAssertEqual(model.runControlTitle, "Pausing…")
        XCTAssertEqual(model.primaryActionTitle, "Pausing…")
        XCTAssertEqual(model.snapshot.phase, .pausing)
        XCTAssertEqual(model.snapshot.timelord.title, "Stopping")
        XCTAssertFalse(model.canPause)
        XCTAssertFalse(model.canStartService)
        model.toggleRun()
        model.startService()
        model.stopService()
        await model.restartService()
        XCTAssertEqual(model.controlOperation, .pause)
        let pauseDeadline = Date().addingTimeInterval(5)
        while model.isBusy && Date() < pauseDeadline {
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertFalse(model.isBusy)
        XCTAssertNil(model.controlOperation)
        XCTAssertEqual(fixture.controller.supervisor?.mode, .paused)
        XCTAssertEqual(fixture.controller.supervisor?.revision, revision + 1)
        XCTAssertNil(fixture.controller.processID)
        XCTAssertEqual(model.snapshot.phase, .paused)
        XCTAssertEqual(model.runControlTitle, "Run")
    }

}
