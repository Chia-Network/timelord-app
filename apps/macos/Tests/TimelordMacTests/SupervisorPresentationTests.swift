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
}

/// A CLI-only supervisor fixture: no worker, launchd, hardware, or TCP listener.
@MainActor
private final class StatusFixture {
    let directory: URL
    let response: URL
    let controller: ServiceController
    init() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("supervisor-presentation-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        response = directory.appendingPathComponent("status.json")
        let helper = directory.appendingPathComponent("helper")
        let script = """
        #!/bin/sh
        case "$1" in
          status) cat '\(response.path)' ;;
          devices) printf '%s' '[{"name":"Fixture","location":1}]' ;;
          *) exit 1 ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        controller = ServiceController(configURL: directory.appendingPathComponent("config.toml"), logsURL: directory.appendingPathComponent("log"), helperURL: helper, endpoint: URL(string: "http://127.0.0.1:0/v1/status")!, supervisorURL: helper)
    }
    func model(_ configuration: AppConfiguration) -> TimelordModel {
        TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
    }
    func publish(mode: String, phase: String, error: String? = nil) throws {
        var status: [String: Any] = ["api_version": 1, "supervisor_pid": 123, "mode": mode, "revision": 1, "worker_generation": 1, "restarts": 0, "phase": phase]
        if let error { status["error"] = error }
        try JSONSerialization.data(withJSONObject: status).write(to: response)
    }
    func close() { try? FileManager.default.removeItem(at: directory) }
}
