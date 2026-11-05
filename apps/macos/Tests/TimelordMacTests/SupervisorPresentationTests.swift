import Foundation
import XCTest
@testable import TimelordMac
import TimelordPresentation

@MainActor
final class SupervisorPresentationTests: XCTestCase {
    func testMissingSupervisorOffersSetupButLossAfterContactRemainsFailure() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        var configuration = AppConfiguration()
        configuration.node.certificate = "/fixture/cert"
        configuration.node.privateKey = "/fixture/key"
        configuration.node.caCertificate = "/fixture/ca"
        configuration.hardware.clientBinary = "/fixture/client"
        configuration.hardware.boards = [ConfiguredBoard(id: "fixture", name: "Fixture", port: 18110, location: 1)]
        let model = fixture.model(configuration)
        await model.refresh()
        XCTAssertEqual(model.snapshot.phase, .setupRequired)
        XCTAssertEqual(model.primaryActionTitle, "Enable background service")
        XCTAssertFalse(model.canControlService)
        XCTAssertNotNil(fixture.controller.errorMessage, "Keep transport diagnostics without blocking setup")
        model.primaryAction()
        XCTAssertEqual(model.sheet?.id, "settings")

        try fixture.publish(mode: "paused", phase: "paused")
        await model.refresh()
        XCTAssertEqual(model.snapshot.phase, .paused)
        try FileManager.default.removeItem(at: fixture.response)
        await model.refresh()
        XCTAssertEqual(model.snapshot.phase, .serviceUnavailable)
        XCTAssertFalse(model.canControlService)
    }

    func testRunningIntentDuringExternalRestartStillAllowsExplicitPause() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let model = fixture.model(AppConfiguration())
        for error in [nil, "cleanup failed"] as [String?] {
            try fixture.publish(mode: "running", phase: "stopping", error: error, workerPID: 123)
            await model.refresh()
            XCTAssertEqual(model.snapshot.phase, .restarting)
            XCTAssertEqual(model.snapshot.timelord.title, "Restarting")
            XCTAssertTrue(model.canPause)
            XCTAssertEqual(model.runControlTitle, "Pause")
            if let error { XCTAssertEqual(model.snapshot.explanation, error) }
            model.stopService()
            let deadline = Date().addingTimeInterval(3)
            while model.isBusy && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
            XCTAssertFalse(model.isBusy)
            XCTAssertEqual(fixture.controller.supervisor?.mode, .paused)
            XCTAssertEqual(fixture.controller.supervisor?.phase, .paused)
            XCTAssertEqual(model.snapshot.phase, .paused)
        }
    }

    func testQuitCompletesRunAcceptedBeforeItsControlTaskStarts() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        try fixture.publish(mode: "paused", phase: "paused")
        await fixture.controller.refresh()
        let model = fixture.model(AppConfiguration())
        model.startService()
        XCTAssertEqual(model.controlOperation, .start)
        // No suspension between accepting Run and beginning Quit: the control
        // Task has not had a chance to send Resume yet.
        await model.prepareForTermination()
        XCTAssertFalse(model.isBusy)
        XCTAssertEqual(fixture.controller.supervisor?.mode, .running)
        await fixture.controller.refresh()
        XCTAssertEqual(fixture.controller.supervisor?.mode, .running, "Run remains durable after interface exit")
        XCTAssertEqual(fixture.controller.supervisor?.revision, 2)
        model.startService()
        XCTAssertFalse(model.isBusy, "Quit still rejects new intent")
    }

    func testCleanupRetainsWorkerPresenceAndDiagnosticUntilConfirmedPaused() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let model = fixture.model(AppConfiguration())
        for error in [nil, "cleanup failed"] as [String?] {
            try fixture.publish(mode: "paused", phase: "stopping", error: error, workerPID: 123)
            await model.refresh()
            XCTAssertEqual(model.snapshot.phase, .pausing)
            XCTAssertEqual(model.snapshot.timelord.title, "Stopping")
            XCTAssertEqual(model.runControlTitle, "Pausing…")
            XCTAssertFalse(model.canStartService)
            XCTAssertFalse(model.canInspectConnections)
            if let error {
                XCTAssertEqual(model.snapshot.explanation, error)
                XCTAssertEqual(model.snapshot.timelord.detail, error)
            } else {
                XCTAssertEqual(model.snapshot.explanation, "Pausing Timelord; waiting for the worker to stop.")
                XCTAssertEqual(model.snapshot.timelord.detail, "On this Mac")
            }
        }
        try fixture.publish(mode: "paused", phase: "paused")
        await model.refresh()
        XCTAssertEqual(model.snapshot.phase, .paused)
        XCTAssertEqual(model.snapshot.timelord.title, "Stopped")
        XCTAssertTrue(model.canStartService)
        XCTAssertTrue(model.canInspectConnections)
    }

    func testRestartDuringPauseCleanupKeepsPausedPresentation() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let model = fixture.model(AppConfiguration())
        try fixture.publish(mode: "paused", phase: "stopping", workerPID: 123)
        await model.refresh()
        let reload = Task { await model.restartService() }
        while !model.isBusy { await Task.yield() }
        XCTAssertEqual(model.controlOperation, .reload)
        XCTAssertEqual(model.snapshot.phase, .pausing, "Reload returns to Paused, so it must not claim Run")
        XCTAssertEqual(model.snapshot.timelord.title, "Stopping")
        XCTAssertEqual(model.runControlTitle, "Pausing…")
        await reload.value
        XCTAssertFalse(model.isBusy)
        XCTAssertEqual(fixture.controller.supervisor?.mode, .paused)
        XCTAssertEqual(model.snapshot.phase, .paused)
    }

    func testDiscoveryCannotEraseANonTransientWorkerStatusFailure() async throws {
        let fixture = try StatusFixture(endpoint: URL(string: "timelord-fixture://status")!)
        defer { fixture.close() }
        var configuration = AppConfiguration()
        configuration.hardware.clientBinary = "/fixture/client"
        configuration.hardware.boards = [ConfiguredBoard(id: "fixture", name: "Fixture", port: 18110, location: 1)]
        let model = fixture.model(configuration)
        try fixture.publish(mode: "running", phase: "worker_started", workerPID: 456)
        await model.refresh()
        XCTAssertFalse(model.discoveredDevices.isEmpty, "Exercise discovery's snapshot update after the failed HTTP poll")
        XCTAssertEqual(model.snapshot.phase, .serviceUnavailable)
        XCTAssertNotEqual(model.snapshot.explanation, "Starting Timelord; waiting for the service to respond.")
    }

    func testWorkerPIDBeforeFirstHTTPResponseStillShowsStartup() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let model = fixture.model(AppConfiguration())
        try fixture.publish(mode: "running", phase: "worker_started", workerPID: 456)
        await model.refresh() // Port zero cannot be a live service fixture.
        XCTAssertEqual(model.snapshot.phase, .starting)
        XCTAssertEqual(model.snapshot.explanation, "Starting Timelord; waiting for the service to respond.")
        XCTAssertTrue(model.canPause)
        XCTAssertEqual(model.runControlTitle, "Pause")
    }

    func testWorkerAbsenceUsesSupervisorLifecycleWithoutBecomingAHardFailure() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let model = fixture.model(AppConfiguration())
        for phase in ["starting", "recovering"] {
            try fixture.publish(mode: "running", phase: phase, error: phase == "recovering" ? "Worker spawn failed: executable is not accessible" : nil)
            await model.refresh()
            XCTAssertEqual(model.snapshot.phase, .starting, phase)
            XCTAssertTrue(model.canPause)
            XCTAssertNil(model.controlOperation, "Observed startup does not fabricate a pending UI command")
            XCTAssertNil(fixture.controller.processID)
        }
        XCTAssertEqual(fixture.controller.errorMessage, "Worker spawn failed: executable is not accessible", "Preserve recovery diagnostics")
        XCTAssertEqual(model.snapshot.explanation, "Worker spawn failed: executable is not accessible", "Recovery failures must be visible while the supervisor retries")
        XCTAssertEqual(model.snapshot.phase, .starting)
        XCTAssertEqual(model.runControlTitle, "Pause")
        try fixture.publish(mode: "paused", phase: "paused")
        await model.refresh()
        XCTAssertEqual(model.snapshot.phase, .paused)
        try fixture.publish(mode: "paused", phase: "state_error", error: "Invalid persisted intent")
        await model.refresh()
        XCTAssertEqual(model.snapshot.phase, .serviceUnavailable)
        XCTAssertEqual(model.snapshot.explanation, "Invalid persisted intent")
    }

    func testPauseFailureStillObservesPersistedIntent() async throws {
        let fixture = try StatusFixture(pauseFails: true)
        defer { fixture.close() }
        try fixture.publish(mode: "running", phase: "worker_started", workerPID: 123)
        await fixture.controller.refresh()
        XCTAssertEqual(fixture.controller.supervisor?.mode, .running)
        try? await fixture.controller.stop()
        XCTAssertEqual(fixture.controller.supervisor?.mode, .paused)
        XCTAssertEqual(fixture.controller.supervisor?.phase, .paused)
        XCTAssertNotNil(fixture.controller.errorMessage)
    }
}

/// A CLI-only supervisor fixture: no worker, launchd, hardware, or TCP listener.
@MainActor
private final class StatusFixture {
    let directory: URL
    let response: URL
    let controller: ServiceController
    init(endpoint: URL = URL(string: "http://127.0.0.1:0/v1/status")!, pauseFails: Bool = false) throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("supervisor-presentation-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        response = directory.appendingPathComponent("status.json")
        let helper = directory.appendingPathComponent("helper")
        let pauseResult = pauseFails ? "exit 1" : "cat '\(response.path)'"
        let script = """
        #!/bin/sh
        case "$1" in
          status) cat '\(response.path)' ;;
          pause)
            printf '%s' '{"api_version":1,"supervisor_pid":123,"mode":"paused","revision":2,"worker_generation":1,"restarts":0,"phase":"paused"}' > '\(response.path)'
            \(pauseResult) ;;
          resume)
            printf '%s' '{"api_version":1,"supervisor_pid":123,"mode":"running","revision":2,"worker_generation":1,"restarts":0,"phase":"starting"}' > '\(response.path)'
            cat '\(response.path)' ;;
          reload)
            sleep 1
            printf '%s' '{"api_version":1,"supervisor_pid":123,"mode":"paused","revision":2,"worker_generation":2,"restarts":0,"phase":"paused"}' > '\(response.path)'
            cat '\(response.path)' ;;
          devices) printf '%s' '[{"name":"Fixture","location":1}]' ;;
          *) exit 1 ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        controller = ServiceController(configURL: directory.appendingPathComponent("config.toml"), logsURL: directory.appendingPathComponent("log"), helperURL: helper, endpoint: endpoint, supervisorURL: helper)
    }
    func model(_ configuration: AppConfiguration) -> TimelordModel {
        TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
    }
    func publish(mode: String, phase: String, error: String? = nil, workerPID: Int? = nil) throws {
        var status: [String: Any] = ["api_version": 1, "supervisor_pid": 123, "mode": mode, "revision": 1, "worker_generation": 1, "restarts": 0, "phase": phase]
        if let error { status["error"] = error }
        if let workerPID { status["worker_pid"] = workerPID }
        try JSONSerialization.data(withJSONObject: status).write(to: response)
    }
    func close() { try? FileManager.default.removeItem(at: directory) }
}
