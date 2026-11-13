import Foundation
import XCTest
@testable import TimelordMac
import TimelordPresentation

@MainActor
final class StartupBehaviorTests: XCTestCase {
    private func fixture(automatic: Bool = true) throws -> (URL, URL, ServiceController, AppConfiguration) {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-startup-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let helper = directory.appendingPathComponent("supervisor")
        let commands = directory.appendingPathComponent("commands")
        let script = """
        #!/bin/sh
        echo "$1" >> '\(commands.path)'
        if [ "$1" = status ] && [ -f '\(directory.path)/offline' ]; then exit 1; fi
        mode=paused; phase=paused
        if [ "$1" = resume ]; then mode=running; phase=starting; fi
        printf '{"api_version":1,"supervisor_pid":123,"worker_pid":null,"mode":"%s","revision":1,"worker_generation":0,"restarts":0,"phase":"%s","error":null}' "$mode" "$phase"
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        let controller = ServiceController(configURL: directory.appendingPathComponent("config.toml"), helperURL: helper, supervisorURL: helper)
        var configuration = AppConfiguration()
        configuration.service.startAutomatically = automatic
        configuration.node.certificate = "/fixture/client.crt"
        configuration.node.privateKey = "/fixture/client.key"
        configuration.node.caCertificate = "/fixture/ca.crt"
        configuration.hardware.clientBinary = "/fixture/native"
        configuration.hardware.boards = [ConfiguredBoard(id: "fixture", name: "Fixture", port: 18110, location: 1)]
        return (directory, commands, controller, configuration)
    }

    func testStartupEnsuresSupervisorOnceWithoutResumingSavedPause() async throws {
        let (directory, commands, controller, configuration) = try fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let model = TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
        await model.restoreStartupIntent()
        await model.restoreStartupIntent()
        XCTAssertEqual(controller.supervisor?.mode, .paused)
        XCTAssertEqual(try String(contentsOf: commands), "ensure-started\n")
    }

    func testRunStartsMissingSupervisorAndResumesWithoutOpeningSettings() async throws {
        let (directory, commands, controller, configuration) = try fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let model = TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
        model.primaryAction()
        for _ in 0..<200 {
            if !model.isBusy { break }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertFalse(model.isBusy)
        XCTAssertNil(model.sheet)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(controller.supervisor?.mode, .running)
        XCTAssertEqual(try String(contentsOf: commands).split(separator: "\n"), ["ensure-started", "status", "resume"])
    }

    func testDisabledAutomaticStartupDoesNotStartSupervisor() async throws {
        let (directory, commands, controller, configuration) = try fixture(automatic: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let model = TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
        await model.restoreStartupIntent()
        XCTAssertFalse(FileManager.default.fileExists(atPath: commands.path))
        XCTAssertNil(controller.supervisor)
    }

    func testMissingSupervisorIsRetriedWithoutOverridingPause() async throws {
        let (directory, commands, controller, configuration) = try fixture()
        defer { try? FileManager.default.removeItem(at: directory) }
        let model = TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
        let first = Date()
        await model.restoreStartupIntent(now: first)
        try Data().write(to: directory.appendingPathComponent("offline"))
        await controller.refresh()
        XCTAssertNil(controller.supervisor)
        await model.restoreStartupIntent(now: first.addingTimeInterval(10))
        await model.restoreStartupIntent(now: first.addingTimeInterval(31))
        XCTAssertEqual(controller.supervisor?.mode, .paused)
        let calls = try String(contentsOf: commands).split(separator: "\n")
        XCTAssertEqual(calls.filter { $0 == "ensure-started" }.count, 2)
        XCTAssertFalse(calls.contains("resume"))
    }
}
