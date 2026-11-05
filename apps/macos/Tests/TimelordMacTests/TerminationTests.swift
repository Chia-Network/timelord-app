import AppKit
import Darwin
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
        let controller = ServiceController(configURL: directory.appendingPathComponent("config.toml"), logsURL: directory.appendingPathComponent("fixture.log"), helperURL: helper, endpoint: URL(string: "http://127.0.0.1:0/v1/status")!)
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

    func testUnresponsiveOwnedHelperCancelsQuitAndCanBeRetried() async throws {
        let (directory, controller) = try fixture()
        let helper = directory.appendingPathComponent("fixture-helper")
        let script = """
        #!/bin/sh
        trap '' TERM
        trap 'exit 0' USR1
        printf ready > "$0.ready"
        while :; do /bin/sleep 0.05; done
        """
        try Data(script.utf8).write(to: helper)
        defer { try? FileManager.default.removeItem(at: directory) }
        try controller.start()
        try await waitUntil { FileManager.default.fileExists(atPath: helper.path + ".ready") }
        let pid = try XCTUnwrap(controller.processID)
        defer { if controller.processID == pid && controller.ownsService { _ = kill(pid, SIGUSR1) } }
        // Pause may request termination, but retains ownership until actual exit.
        controller.stop()
        XCTAssertTrue(controller.ownsService)
        let model = TimelordModel(controller: controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        var replies = [Bool]()
        let delegate = ApplicationDelegate(model: model, shutdownTimeout: 0.1, reply: { replies.append($0) })
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateLater)
        try await waitUntil { !replies.isEmpty }
        XCTAssertEqual(replies, [false])
        XCTAssertEqual(controller.processID, pid)
        XCTAssertTrue(controller.ownsService)
        XCTAssertTrue(controller.preventsIdleSleep)
        XCTAssertNotNil(model.errorMessage)
        XCTAssertFalse(model.canStartService)
        // The fixture has an explicit exit signal; no force-kill or unrelated process.
        XCTAssertEqual(kill(pid, SIGUSR1), 0)
        try await waitUntil { !controller.ownsService }
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateLater)
        try await waitUntil { replies.count == 2 }
        XCTAssertEqual(replies, [false, true])
        XCTAssertEqual(delegate.applicationShouldTerminate(NSApplication.shared), .terminateNow)
        XCTAssertNil(controller.processID)
        XCTAssertFalse(controller.preventsIdleSleep)
    }

    func testTimedOutQuitNeverRevivesTheCancelledStartup() async throws {
        let (directory, controller) = try fixture()
        let helper = directory.appendingPathComponent("fixture-helper")
        let release = directory.appendingPathComponent("fixture-helper.release")
        let script = """
        #!/bin/sh
        case "$1" in
          devices) printf '%s' '[{"name":"Fixture","location":1,"serial":"fixture-only"}]' ;;
          configure)
            trap '' TERM
            printf saving > "$0.saving"
            while [ ! -f "$0.release" ]; do /bin/sleep 0.02; done ;;
          serve) printf ready > "$0.ready"; exit 0 ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        defer {
            try? Data().write(to: release)
            controller.stop()
        }
        var configuration = AppConfiguration()
        configuration.hardware.clientBinary = "/fixture/not-executed"
        let model = TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
        model.startService()
        try await waitUntil { FileManager.default.fileExists(atPath: helper.path + ".saving") }
        let stopped = await model.prepareForTermination(timeout: 0.05)
        XCTAssertFalse(stopped)
        XCTAssertTrue(model.starting)
        try Data().write(to: release)
        try await waitUntil { !model.starting }
        XCTAssertFalse(controller.ownsService)
        XCTAssertFalse(FileManager.default.fileExists(atPath: helper.path + ".ready"))
        XCTAssertTrue(model.canStartService, "A new explicit Run remains available after cancelled setup drains")
        try FileManager.default.removeItem(at: directory)
    }

    func testLaunchedServiceCanBePausedWhileNodePollingIsBlocked() async throws {
        let (directory, controller) = try fixture()
        let helper = directory.appendingPathComponent("fixture-helper")
        let release = directory.appendingPathComponent("fixture-helper.release")
        let script = """
        #!/bin/sh
        case "$1" in
          devices) printf '%s' '[{"name":"Fixture","location":1,"serial":"fixture-only"}]' ;;
          configure) exit 0 ;;
          node-state)
            printf polling > "$0.polling"
            while [ ! -f "$0.release" ]; do /bin/sleep 0.02; done
            exit 1 ;;
          serve) exec /bin/sleep 30 ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        defer { try? Data().write(to: release); controller.stop() }
        var configuration = AppConfiguration()
        configuration.service.startAutomatically = false
        configuration.service.statusListen = "127.0.0.1:0"
        configuration.hardware.clientBinary = "/fixture/not-executed"
        configuration.node.rpc = try JSONDecoder().decode(NodeRPCConfiguration.self, from: Data(
            #"{"host":"fixture","port":1,"serverName":"chia.net","certificate":"unused","privateKey":"unused","caCertificate":"unused"}"#.utf8))
        let model = TimelordModel(controller: controller, initialConfiguration: configuration)
        try await waitUntil { FileManager.default.fileExists(atPath: helper.path + ".polling") }
        model.startService()
        try await waitUntil { controller.ownsService && !model.starting }
        XCTAssertEqual(model.snapshot.phase, .starting, "The owned worker has not served status yet")
        XCTAssertTrue(model.canPause)
        XCTAssertTrue(model.canFinishSettings)
        XCTAssertEqual(model.runControlTitle, "Pause")
        XCTAssertFalse(FileManager.default.fileExists(atPath: release.path))
        try Data().write(to: release)
        let stopped = await model.prepareForTermination()
        XCTAssertTrue(stopped)
        try FileManager.default.removeItem(at: directory)
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
