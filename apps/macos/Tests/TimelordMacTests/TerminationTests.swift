import AppKit
import XCTest
@testable import TimelordMac
import TimelordPresentation

@MainActor
final class TerminationTests: XCTestCase {
    private func fixture() throws -> (URL, ServiceController) {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-quit-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let helper = directory.appendingPathComponent("fixture-helper")
        let script = """
        #!/bin/sh
        case "$1" in
          serve)
            trap '/bin/sleep 0.2; printf flushed > "$0.flushed"; exit 0' TERM
            printf ready > "$0.ready"
            while :; do /bin/sleep 0.1; done ;;
          devices)
            printf scanning > "$0.scanning"
            /bin/sleep 0.2
            printf '%s' '[{"name":"Fixture","location":1,"serial":"fixture-only"}]' ;;
          configure) printf saving > "$0.saving"; /bin/sleep 0.2; exit 0 ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        let controller = ServiceController(configURL: directory.appendingPathComponent("config.toml"), logsURL: directory.appendingPathComponent("fixture.log"), helperURL: helper)
        return (directory, controller)
    }

    private func waitUntil(_ condition: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(5)
        while !condition() && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
        XCTAssertTrue(condition())
    }

    func testSystemQuitWaitsForOwnedHelperCleanupAndRepliesOnlyOnce() async throws {
        let (directory, controller) = try fixture()
        defer { controller.stop(); try? FileManager.default.removeItem(at: directory) }
        try controller.start()
        let ready = directory.appendingPathComponent("fixture-helper.ready")
        let flushed = directory.appendingPathComponent("fixture-helper.flushed")
        try await waitUntil { FileManager.default.fileExists(atPath: ready.path) }
        XCTAssertTrue(controller.preventsIdleSleep)
        let model = TimelordModel(controller: controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        var replies = [Bool]()
        let delegate = ApplicationDelegate(model: model, reply: { replies.append($0) })
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateLater)
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateLater)
        XCTAssertTrue(replies.isEmpty)
        try await waitUntil { !replies.isEmpty }
        XCTAssertEqual(replies, [true])
        XCTAssertTrue(FileManager.default.fileExists(atPath: flushed.path), "Do not exit before the helper's final flush")
        XCTAssertFalse(controller.ownsService)
        XCTAssertNil(controller.processID)
        XCTAssertFalse(controller.preventsIdleSleep)
        XCTAssertFalse(model.canStartService)
        model.startService()
        XCTAssertFalse(model.starting)
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateNow)
    }

    func testQuitDuringDiscoveryDoesNotLaunchHelperAfterCleanup() async throws {
        let (directory, controller) = try fixture()
        defer { controller.stop(); try? FileManager.default.removeItem(at: directory) }
        var configuration = AppConfiguration()
        configuration.hardware.clientBinary = "/fixture/not-executed"
        let model = TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
        model.startService()
        let scanning = directory.appendingPathComponent("fixture-helper.scanning")
        try await waitUntil { FileManager.default.fileExists(atPath: scanning.path) }
        await model.prepareForTermination()
        XCTAssertFalse(model.starting)
        XCTAssertFalse(controller.ownsService)
        XCTAssertFalse(FileManager.default.fileExists(atPath: directory.appendingPathComponent("fixture-helper.ready").path))
    }

    func testQuitDuringConfigWriteDoesNotLaunchHelperAfterCleanup() async throws {
        let (directory, controller) = try fixture()
        defer { controller.stop(); try? FileManager.default.removeItem(at: directory) }
        var configuration = AppConfiguration()
        configuration.hardware.clientBinary = "/fixture/not-executed"
        let model = TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
        model.startService()
        let saving = directory.appendingPathComponent("fixture-helper.saving")
        try await waitUntil { FileManager.default.fileExists(atPath: saving.path) }
        await model.prepareForTermination()
        XCTAssertFalse(model.starting)
        XCTAssertFalse(controller.ownsService)
        XCTAssertFalse(FileManager.default.fileExists(atPath: directory.appendingPathComponent("fixture-helper.ready").path))
    }

    func testQuittingInterfaceDoesNotStopAnUnownedProcess() async throws {
        let (directory, controller) = try fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let external = Process()
        external.executableURL = URL(fileURLWithPath: "/bin/sleep")
        external.arguments = ["30"]
        try external.run()
        defer { external.terminate(); external.waitUntilExit() }
        let model = TimelordModel(controller: controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        await model.prepareForTermination()
        XCTAssertTrue(external.isRunning)
        XCTAssertFalse(controller.preventsIdleSleep)
    }
}
