import Foundation
import XCTest
@testable import TimelordMac
import TimelordPresentation

final class ConfigurationClientTests: XCTestCase {
    func testActualHelperConfigurationRoundTripAndInvalidEditPreserveSavedSettings() async throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let helper = root.appendingPathComponent("build/rust/debug/timelord-service")
        guard FileManager.default.isExecutableFile(atPath: helper.path) else { throw XCTSkip("Build the native helper with scripts/check.sh first.") }
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-swift-cli-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let config = directory.appendingPathComponent("config.toml")
        try Data("schema_version = 1\n".utf8).write(to: config)
        let client = ConfigurationClient(helper: helper, config: config)
        var value = try await client.load()
        XCTAssertEqual(value.node.host, "localhost")
        value.node.host = "fixture.invalid"; value.node.port = 18444; value.node.network = "testnet11"
        value.hardware.boards = (0..<3).map { index in
            var board = ConfiguredBoard(id: "b\(index)", name: "Board \(index)", port: 18110 + index, location: 1 + index)
            board.serial = "serial-\(index)"; return board
        }
        value.hardware.chainAssignments = ChainAssignments(challenge: "serial-2", reward: "serial-0", infusedChallenge: "serial-1")
        value.hardware.boards[0].automatic.retestAfterSeconds = 0
        try await client.save(value)
        let saved = try await client.load()
        XCTAssertEqual(saved.node.host, "fixture.invalid")
        XCTAssertEqual(saved.node.port, 18444)
        XCTAssertEqual(saved.hardware.chainAssignments, value.hardware.chainAssignments)
        XCTAssertEqual(saved.hardware.boards[0].automatic.retestAfterSeconds, 0)
        value.node.host = ""
        do { try await client.save(value); XCTFail("Invalid config was saved") } catch { XCTAssertFalse(error.localizedDescription.isEmpty) }
        let preserved = try await client.load()
        XCTAssertEqual(preserved.node.host, "fixture.invalid")
        // Missing credentials fail before any node socket is opened.
        do { _ = try await client.testNode(); XCTFail("Unauthenticated configuration passed") } catch { XCTAssertFalse(error.localizedDescription.isEmpty) }
    }
    @MainActor
    func testDonePersistsAutomaticSelectionWithoutStartingHardwareAndKeepsInvalidEditsOpen() async throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let helper = root.appendingPathComponent("build/rust/debug/timelord-service")
        guard FileManager.default.isExecutableFile(atPath: helper.path) else { throw XCTSkip("Build the native helper first.") }
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-settings-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let config = directory.appendingPathComponent("config.toml")
        let client = ConfigurationClient(helper: helper, config: config)
        var saved = AppConfiguration()
        saved.service.startAutomatically = false
        var board = ConfiguredBoard(id: "fixture", name: "Fixture", port: 18110, location: 1)
        board.serial = "fixture-only"; board.profile = "manual"; board.frequencyMhz = 1500
        saved.hardware.boards = [board]
        try await client.save(saved)
        let controller = ServiceController(configURL: config, logsURL: directory.appendingPathComponent("service.log"), helperURL: helper)
        let model = TimelordModel(controller: controller, initialConfiguration: saved, monitoringEnabled: false)
        XCTAssertFalse(model.hasUnsavedSettings)
        model.configuration.hardware.boards[0].profile = "automatic"
        model.configuration.hardware.boards[0].automatic.maxFrequencyMhz = 2201
        XCTAssertTrue(model.hasUnsavedSettings)
        let invalidDismiss = await model.finishSettings()
        XCTAssertFalse(invalidDismiss, "Validation failure must keep the sheet open")
        XCTAssertNotNil(model.errorMessage)
        XCTAssertFalse(model.canControlService)
        let unchanged = try await client.load()
        XCTAssertEqual(unchanged.hardware.boards[0].profile, "manual")
        XCTAssertEqual(model.configuration.hardware.boards[0].profile, "automatic", "Preserve the user's draft for correction")
        model.configuration.hardware.boards[0].automatic.maxFrequencyMhz = 2200
        let dismiss = await model.finishSettings()
        XCTAssertTrue(dismiss)
        XCTAssertFalse(model.hasUnsavedSettings)
        XCTAssertFalse(model.canControlService, "Selecting Automatic must not implicitly start hardware")
        XCTAssertEqual(model.settingsDoneTitle, "Done")
        XCTAssertTrue(model.settingsCompletionHint.contains("Enable the background service"))
        let updated = try await client.load()
        XCTAssertEqual(updated.hardware.boards[0].profile, "automatic")
        XCTAssertEqual(updated.hardware.boards[0].automatic.startFrequencyMhz, 1450)
        XCTAssertEqual(updated.hardware.boards[0].automatic.startVoltageV, 1)
        let secondDismiss = await model.finishSettings()
        XCTAssertTrue(secondDismiss)
        XCTAssertFalse(model.configurationNeedsRestart)
    }

    @MainActor
    func testRunAppliesSettingsSavedWhilePausedWithoutRestartingAgain() async throws {
        let fixture = try SupervisedFixture()
        defer { fixture.close() }
        let saved = AppConfiguration()
        try await fixture.launch(saved)
        try await fixture.controller.stop()
        let model = TimelordModel(controller: fixture.controller, initialConfiguration: saved, monitoringEnabled: false)
        model.configuration.service.statusListen = "127.0.0.1:18190"
        await model.saveConfiguration()
        XCTAssertTrue(model.configurationNeedsRestart)

        model.startService()
        try await fixture.waitForWorker()
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        while model.isBusy && ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertFalse(model.isBusy)
        XCTAssertFalse(model.configurationNeedsRestart)
        XCTAssertEqual(model.statusURL.absoluteString, "http://127.0.0.1:18190/v1/status")
        let generation = try XCTUnwrap(fixture.controller.supervisor?.workerGeneration)
        let dismissed = await model.finishSettings()
        XCTAssertTrue(dismissed)
        await fixture.controller.refresh()
        XCTAssertEqual(fixture.controller.supervisor?.workerGeneration, generation)
    }

    @MainActor
    func testSaveAndRestartPersistsBeforeReplacingOnlyTheOwnedFixtureProcess() async throws {
        let fixture = try SupervisedFixture()
        defer { fixture.close() }
        let client = fixture.client
        let nativeHelper = fixture.nativeHelper
        let helper = fixture.helper
        let controller = fixture.controller
        var saved = AppConfiguration()
        saved.service.statusListen = "127.0.0.1:18190"
        saved.hardware.clientBinary = "/fixture/never-executed"
        var board = ConfiguredBoard(id: "fixture", name: "Fixture", port: 18110, location: 1)
        board.serial = "fixture-only"; board.profile = "manual"
        saved.hardware.boards = [board]
        try await fixture.launch(saved)
        let model = TimelordModel(controller: controller, initialConfiguration: saved, monitoringEnabled: false)
        let oldPid = try XCTUnwrap(controller.processID)
        XCTAssertEqual(model.settingsDoneTitle, "Done")
        model.configuration.hardware.boards[0].profile = "automatic"
        model.configuration.hardware.boards[0].automatic.maxFrequencyMhz = 2201
        XCTAssertEqual(model.settingsDoneTitle, "Save & Restart")
        let invalidDismiss = await model.finishSettings()
        XCTAssertFalse(invalidDismiss)
        XCTAssertTrue(controller.canControlService, "Invalid edits must not stop the running service")
        XCTAssertEqual(controller.processID, oldPid)
        model.configuration.hardware.boards[0].automatic.maxFrequencyMhz = 2200
        let dismiss = await model.finishSettings()
        XCTAssertTrue(dismiss)
        try await fixture.waitForWorker(excluding: oldPid)
        let deadline = Date().addingTimeInterval(5)
        XCTAssertTrue(controller.canControlService)
        let newPid = try XCTUnwrap(controller.processID)
        XCTAssertNotEqual(newPid, oldPid)
        XCTAssertEqual(model.statusURL.absoluteString, "http://127.0.0.1:18190/v1/status")
        let started = URL(fileURLWithPath: helper.path + ".started.\(newPid)")
        while !FileManager.default.fileExists(atPath: started.path) && Date() < deadline {
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        let applied = try await ConfigurationClient(helper: nativeHelper, config: started).load()
        XCTAssertEqual(applied.hardware.boards[0].profile, "automatic")
        XCTAssertFalse(model.hasUnsavedSettings)
        XCTAssertFalse(model.configurationNeedsRestart)
        try await controller.stop()
        XCTAssertTrue(controller.canControlService, "Pause retains independent control")
        XCTAssertNil(controller.processID)
    }

    @MainActor
    func testGuiRelaunchHonorsDurablePauseAndDoesNotOwnCrashRecovery() async throws {
        let fixture = try SupervisedFixture()
        defer { fixture.close() }
        try await fixture.launch()
        let controller = fixture.controller
        let firstPid = try XCTUnwrap(controller.processID)
        let model = TimelordModel(controller: controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        XCTAssertTrue(model.canPause)
        await model.prepareForTermination()
        XCTAssertEqual(controller.processID, firstPid, "Quitting the interface does not terminate productive work")
        kill(firstPid, SIGKILL)
        try await fixture.waitForWorker(excluding: firstPid)
        try await controller.stop()
        let revision = try XCTUnwrap(controller.supervisor?.revision)
        let relaunched = TimelordModel(controller: controller, initialConfiguration: AppConfiguration(), monitoringEnabled: false)
        await relaunched.refresh()
        XCTAssertFalse(relaunched.canPause)
        XCTAssertTrue(relaunched.canStartService)
        XCTAssertEqual(controller.supervisor?.revision, revision, "Polling and relaunch cannot erase Pause")
        XCTAssertNil(controller.processID)
        relaunched.startService()
        let deadline = Date().addingTimeInterval(5)
        while relaunched.isBusy && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
        try await fixture.waitForWorker()
        XCTAssertTrue(relaunched.canPause)
    }

    func testMissingHelperReportsAnActionableError() async {
        let client = ConfigurationClient(helper: URL(fileURLWithPath: "/missing-timelord-fixture"), config: URL(fileURLWithPath: "/unused-fixture.toml"))
        do { _ = try await client.load(); XCTFail("Missing helper succeeded") }
        catch { XCTAssertTrue(error.localizedDescription.contains("helper")) }
    }
}
