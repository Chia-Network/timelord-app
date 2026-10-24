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
        value.service.observabilityListen = "0.0.0.0:9258"
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
        XCTAssertEqual(saved.service.observabilityListen, "0.0.0.0:9258")
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
        XCTAssertFalse(model.ownsService)
        let unchanged = try await client.load()
        XCTAssertEqual(unchanged.hardware.boards[0].profile, "manual")
        XCTAssertEqual(model.configuration.hardware.boards[0].profile, "automatic", "Preserve the user's draft for correction")
        model.configuration.hardware.boards[0].automatic.maxFrequencyMhz = 2200
        let dismiss = await model.finishSettings()
        XCTAssertTrue(dismiss)
        XCTAssertFalse(model.hasUnsavedSettings)
        XCTAssertFalse(model.ownsService, "Selecting Automatic must not implicitly start hardware")
        XCTAssertEqual(model.settingsDoneTitle, "Done")
        XCTAssertTrue(model.settingsCompletionHint.contains("Press Run"))
        let updated = try await client.load()
        XCTAssertEqual(updated.hardware.boards[0].profile, "automatic")
        XCTAssertEqual(updated.hardware.boards[0].automatic.startFrequencyMhz, 1450)
        XCTAssertEqual(updated.hardware.boards[0].automatic.startVoltageV, 1)
        let secondDismiss = await model.finishSettings()
        XCTAssertTrue(secondDismiss)
        XCTAssertFalse(model.configurationNeedsRestart)
    }

    @MainActor
    func testSaveAndRestartPersistsBeforeReplacingOnlyTheOwnedFixtureProcess() async throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let nativeHelper = root.appendingPathComponent("build/rust/debug/timelord-service")
        guard FileManager.default.isExecutableFile(atPath: nativeHelper.path) else { throw XCTSkip("Build the native helper first.") }
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-restart-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let helper = directory.appendingPathComponent("fixture-helper")
        let script = """
        #!/bin/sh
        case "$1" in
          serve) cp "$3" "$0.started.$$"; exec /bin/sleep 30 ;;
          devices) printf '%s' '[{"name":"Fixture","location":1,"serial":"fixture-only"}]' ;;
          *) exec '\(nativeHelper.path)' "$@" ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        let config = directory.appendingPathComponent("config.toml")
        let client = ConfigurationClient(helper: helper, config: config)
        var saved = AppConfiguration()
        saved.service.statusListen = "127.0.0.1:18190"
        saved.hardware.clientBinary = "/fixture/never-executed"
        var board = ConfiguredBoard(id: "fixture", name: "Fixture", port: 18110, location: 1)
        board.serial = "fixture-only"; board.profile = "manual"
        saved.hardware.boards = [board]
        try await client.save(saved)
        let controller = ServiceController(configURL: config, logsURL: directory.appendingPathComponent("fixture.log"), helperURL: helper, endpoint: URL(string:"http://127.0.0.1:0/v1/status")!)
        let model = TimelordModel(controller: controller, initialConfiguration: saved, monitoringEnabled: false)
        try controller.start()
        defer { controller.stop() }
        XCTAssertTrue(controller.preventsIdleSleep)
        let oldPid = try XCTUnwrap(controller.processID)
        XCTAssertEqual(model.settingsDoneTitle, "Done")
        model.configuration.hardware.boards[0].profile = "automatic"
        model.configuration.hardware.boards[0].automatic.maxFrequencyMhz = 2201
        XCTAssertEqual(model.settingsDoneTitle, "Save & Restart")
        let invalidDismiss = await model.finishSettings()
        XCTAssertFalse(invalidDismiss)
        XCTAssertTrue(controller.ownsService, "Invalid edits must not stop the running service")
        XCTAssertEqual(controller.processID, oldPid)
        model.configuration.hardware.boards[0].automatic.maxFrequencyMhz = 2200
        let dismiss = await model.finishSettings()
        XCTAssertTrue(dismiss)
        let deadline = Date().addingTimeInterval(5)
        while (model.starting || !controller.ownsService || controller.processID == oldPid) && Date() < deadline {
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertTrue(controller.ownsService)
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
        controller.stop()
        while controller.ownsService && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
        XCTAssertFalse(controller.ownsService)
        XCTAssertFalse(controller.preventsIdleSleep)
    }

    @MainActor
    func testAutomaticStartupWaitsForReadinessHonorsOptOutAndDoesNotRetryAfterFailureOrPause() async throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let nativeHelper = root.appendingPathComponent("build/rust/debug/timelord-service")
        guard FileManager.default.isExecutableFile(atPath: nativeHelper.path) else { throw XCTSkip("Build the native helper first.") }
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-autostart-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let helper = directory.appendingPathComponent("fixture-helper")
        let script = """
        #!/bin/sh
        case "$1" in
          serve) exec /bin/sleep 30 ;;
          devices) printf '%s' '[{"name":"Fixture","location":1,"serial":"fixture-only"}]' ;;
          *) exec '\(nativeHelper.path)' "$@" ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        let config = directory.appendingPathComponent("config.toml")
        let client = ConfigurationClient(helper: helper, config: config)
        var saved = AppConfiguration()
        saved.service.statusListen = "127.0.0.1:18190"
        saved.hardware.clientBinary = "/fixture/never-executed"
        saved.node.certificate = "/fixture/cert"; saved.node.privateKey = "/fixture/key"; saved.node.caCertificate = "/fixture/ca"
        var board = ConfiguredBoard(id: "fixture", name: "Fixture", port: 18110, location: 1)
        board.serial = "fixture-only"
        saved.hardware.boards = [board]
        try await client.save(saved)
        let controller = ServiceController(configURL: config, logsURL: directory.appendingPathComponent("fixture.log"), helperURL: helper)
        let model = TimelordModel(controller: controller, initialConfiguration: saved, monitoringEnabled: false)
        defer { controller.stop() }
        model.considerAutomaticStart(serviceAbsent: true)
        XCTAssertFalse(model.starting, "Configured boards are not evidence of detected hardware")
        await model.discoverBoards()
        model.considerAutomaticStart(serviceAbsent: false)
        XCTAssertFalse(model.starting, "Do not start until the status listener is confirmed absent")
        model.sheet = .settings
        model.considerAutomaticStart(serviceAbsent: true)
        XCTAssertFalse(model.starting, "Do not start while settings are being edited")
        model.sheet = nil
        model.configuration.hardware.boards[0].name = "Unsaved draft"
        model.considerAutomaticStart(serviceAbsent: true)
        XCTAssertFalse(model.starting)
        model.configuration.hardware.boards[0].name = "Fixture"
        model.configuration.service.startAutomatically = false
        let disabledSaved = await model.finishSettings()
        XCTAssertTrue(disabledSaved)
        model.considerAutomaticStart(serviceAbsent: true)
        XCTAssertFalse(model.starting)
        model.configuration.service.startAutomatically = true
        let enabledSaved = await model.finishSettings()
        XCTAssertTrue(enabledSaved)
        model.considerAutomaticStart(serviceAbsent: true)
        XCTAssertTrue(model.starting)
        var deadline = Date().addingTimeInterval(5)
        while model.starting && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
        XCTAssertTrue(controller.ownsService)
        let originalPid = controller.processID
        model.configuration.service.startAutomatically = false
        XCTAssertEqual(model.settingsDoneTitle, "Done", "App startup preference does not require a service restart")
        let preferenceSaved = await model.finishSettings()
        XCTAssertTrue(preferenceSaved)
        XCTAssertEqual(controller.processID, originalPid)
        model.configuration.service.startAutomatically = true
        let preferenceRestored = await model.finishSettings()
        XCTAssertTrue(preferenceRestored)
        // Simulate a child failure without a user Pause. The launch intent is consumed.
        controller.stop()
        while controller.ownsService && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
        model.considerAutomaticStart(serviceAbsent: true)
        XCTAssertFalse(model.starting, "Automatic startup must not become a hardware restart loop")
        XCTAssertFalse(controller.ownsService)
        model.startService()
        deadline = Date().addingTimeInterval(5)
        while model.starting && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
        XCTAssertTrue(controller.ownsService)
        model.stopService()
        while controller.ownsService && Date() < deadline { try await Task.sleep(nanoseconds: 10_000_000) }
        model.considerAutomaticStart(serviceAbsent: true)
        XCTAssertFalse(model.starting, "Polling must respect Pause for the rest of the session")
        XCTAssertFalse(controller.ownsService)
        let persisted = try await client.load()
        XCTAssertEqual(persisted.service.startAutomatically, true)
    }

    func testMissingHelperReportsAnActionableError() async {
        let client = ConfigurationClient(helper: URL(fileURLWithPath: "/missing-timelord-fixture"), config: URL(fileURLWithPath: "/unused-fixture.toml"))
        do { _ = try await client.load(); XCTFail("Missing helper succeeded") }
        catch { XCTAssertTrue(error.localizedDescription.contains("helper")) }
    }
}
