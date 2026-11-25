import Foundation
import XCTest
@testable import TimelordMac
import TimelordPresentation

@MainActor
final class SupervisorPresentationTests: XCTestCase {
    func testMissingSupervisorAllowsRunButLossAfterContactRemainsFailure() async throws {
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
        XCTAssertEqual(model.primaryActionTitle, "Start Timelord")
        XCTAssertTrue(model.canStartService)
        XCTAssertFalse(model.canControlService)
        XCTAssertNotNil(fixture.controller.errorMessage, "Keep transport diagnostics without blocking setup")

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

    func testPausedSettingsDescribeApplyingChangesWithoutResuming() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let model = fixture.model(AppConfiguration())
        try fixture.publish(mode: "paused", phase: "paused")
        await model.refresh()
        XCTAssertEqual(model.settingsDoneTitle, "Done")
        XCTAssertEqual(model.settingsCompletionHint, "Your settings are saved. Timelord is paused.")
        model.configuration.node.host = "fixture.invalid"
        XCTAssertEqual(model.settingsDoneTitle, "Save & Apply")
        XCTAssertTrue(model.settingsCompletionHint.contains("Timelord stays paused."))
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

    func testStatusFailureSurfacesStderrAndDoesNotRetry() async throws {
        // A failed supervisor poll is deliberately fail-fast: surface the latest
        // diagnostic instead of spending the refresh cycle on hidden retries.
        let fixture = try StatusFixture(statusFails: true)
        defer { fixture.close() }
        await fixture.controller.refresh()
        XCTAssertEqual(fixture.controller.errorMessage,
            "Background supervision unavailable: status fixture failed")
        XCTAssertEqual(try fixture.loggedCommands().filter { $0.hasPrefix("status") }.count, 1,
            "A failed status poll must fail fast instead of hiding the diagnostic behind retries")
    }

    func testSuccessfulSaveKeepsUnresolvedStartupErrorVisible() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let configuration = runnableConfiguration()
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        try fixture.setStartupFailure(true)
        await model.restoreStartupIntent()
        XCTAssertEqual(model.errorMessage, "startup fixture failed")

        let saved = await model.saveConfiguration()
        XCTAssertTrue(saved, "A persistent startup failure is not a save failure")
        XCTAssertEqual(model.errorMessage, "startup fixture failed", "A successful save must not hide startup failure")
        let finished = await model.finishSettings()
        XCTAssertTrue(finished, "The startup error must not block settings")
        XCTAssertEqual(model.errorMessage, "startup fixture failed")

        await model.testNode()
        XCTAssertEqual(model.connectionResult, "Authenticated fixture node.")
        XCTAssertEqual(model.errorMessage, "startup fixture failed", "The startup error must not block a connection test")
        await model.discoverBoards()
        XCTAssertFalse(model.discoveredDevices.isEmpty)
        XCTAssertEqual(model.errorMessage, "startup fixture failed", "The startup error must not block board discovery")

        try fixture.setStartupFailure(false)
        try fixture.publish(mode: "paused", phase: "paused")
        await model.refresh()
        XCTAssertTrue(model.canControlService)
        XCTAssertNil(model.errorMessage, "Contact with the supervisor resolves the startup error")
    }

    func testFailedSaveKeepsPersistedStateAndPendingRestart() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let configuration = runnableConfiguration(host: "first.fixture")
        try fixture.writeConfiguration(configuration)
        try fixture.publish(mode: "running", phase: "worker_started")
        let model = fixture.model(configuration)
        await model.refresh()
        XCTAssertTrue(model.canControlService)

        model.configuration.node.host = "second.fixture"
        let firstSave = await model.saveConfiguration()
        XCTAssertTrue(firstSave)
        XCTAssertTrue(model.configurationNeedsRestart)
        let persisted = model.savedConfiguration
        XCTAssertEqual(persisted.node.host, "second.fixture")

        model.configuration.node.host = "third.fixture"
        try fixture.setSaveFailure(true)
        let failedSave = await model.saveConfiguration()
        XCTAssertFalse(failedSave)
        XCTAssertEqual(model.errorMessage, "configuration fixture failed")
        XCTAssertEqual(model.savedConfiguration, persisted, "A failed save must not adopt the rejected draft")
        XCTAssertTrue(model.configurationNeedsRestart, "A failed save must not clear the pending restart")
        XCTAssertTrue(model.hasUnsavedSettings)
        XCTAssertEqual(model.settingsCompletionHint,
            "Save & Restart switches the full-node connection. ASIC boards keep running.",
            "A failed save must not turn a node-only change into a full restart")
    }

    func testProfileSwitchAndActiveProfileEditRequireRestart() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let main = runnableConfiguration(host: "main.fixture").node
        var other = runnableConfiguration(host: "other.fixture").node
        other.certificate = "/fixture/other.crt"
        var profiles = ConnectionProfiles(); profiles.activeId = "main"
        profiles.profiles = [
            ConnectionProfile(id: "main", name: "Main", node: main),
            ConnectionProfile(id: "other", name: "Other", node: other),
        ]
        var configuration = AppConfiguration(); configuration.connections = profiles
        try fixture.writeConfiguration(configuration)
        try fixture.publish(mode: "running", phase: "worker_started")
        let model = fixture.model(configuration)
        await model.refresh()
        XCTAssertTrue(model.canControlService)
        XCTAssertEqual(model.settingsDoneTitle, "Done")

        model.selectConnectionProfile("other")
        XCTAssertTrue(model.hasUnsavedSettings)
        XCTAssertEqual(model.settingsDoneTitle, "Save & Restart")
        let switched = await model.finishSettings()
        XCTAssertTrue(switched)
        XCTAssertTrue(try fixture.loggedCommands().contains { $0.hasPrefix("reload ") },
            "Switching the active profile must reload the running service")

        try fixture.publish(mode: "paused", phase: "paused")
        await model.refresh()
        XCTAssertEqual(model.settingsDoneTitle, "Done")
        model.configuration.node.host = "edited-other.fixture"
        XCTAssertEqual(model.settingsDoneTitle, "Save & Apply")
        let reloads = try fixture.loggedCommands().filter { $0.hasPrefix("reload ") }.count
        let applied = await model.finishSettings()
        XCTAssertTrue(applied)
        XCTAssertEqual(try fixture.loggedCommands().filter { $0.hasPrefix("reload ") }.count, reloads + 1,
            "Editing the active profile must apply the saved configuration to a paused service")
    }

    func testConnectionTestKeepsIncompleteRPCDraftInTheForm() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let first = runnableConfiguration(host: "first.fixture").node
        var second = runnableConfiguration(host: "second.fixture").node
        second.certificate = "/fixture/second.crt"
        var profiles = ConnectionProfiles(); profiles.activeId = "first"
        profiles.profiles = [
            ConnectionProfile(id: "first", name: "First", node: first),
            ConnectionProfile(id: "second", name: "Second", node: second),
        ]
        var configuration = AppConfiguration(); configuration.connections = profiles
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        let draft = NodeRPCConfiguration(
            host: "rpc.first.fixture", port: 8555, serverName: "chia.net",
            certificate: "/fixture/rpc.crt", privateKey: "/fixture/rpc.key")
        model.configuration.node.rpc = draft

        await model.testNode()
        XCTAssertEqual(model.connectionResult, "Authenticated fixture node.")
        XCTAssertEqual(model.configuration.node.rpc, draft,
            "Saving for a connection test must not clear the half-filled RPC form")
        XCTAssertNil(model.errorMessage, "An implicit save must not report dropping the draft")
        XCTAssertNil(model.notice, "An implicit save must not report dropping the draft")
        XCTAssertFalse(model.hasUnsavedSettings,
            "An incomplete RPC draft alone is not an unsaved persisted setting")

        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let persisted = try decoder.decode(AppConfiguration.self, from: Data(contentsOf: fixture.configurationURL))
        XCTAssertNil(persisted.connections?.profiles[0].node.rpc,
            "The incomplete draft must not be persisted")

        await model.discoverBoards()
        XCTAssertEqual(model.configuration.node.rpc, draft,
            "Saving for a board refresh must not clear the half-filled RPC form")
        XCTAssertNil(model.errorMessage, "A board refresh must not report dropping the draft")
        XCTAssertNil(model.notice, "A board refresh must not report dropping the draft")
    }

    func testIncompleteRPCDraftKeepsLastSavedRPCInSingleNodeMode() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let rpc = completeRPC(host: "rpc.single.fixture")
        var configuration = runnableConfiguration(host: "single.fixture")
        configuration.node.rpc = rpc
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        var node = model.configuration.node
        node.rpc?.host = ""
        model.configuration.node = node

        await model.testNode()
        XCTAssertEqual(model.connectionResult, "Authenticated fixture node.")
        XCTAssertEqual(model.configuration.node.rpc?.host, "",
            "The form keeps the blank-host draft")
        let persisted = try decode(fixture.configurationURL)
        XCTAssertEqual(persisted.node.rpc, rpc,
            "An incomplete draft must not erase already-saved RPC settings")
    }

    func testIncompleteRPCDraftKeepsLastSavedRPCPerProfile() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let savedRPC = completeRPC(host: "rpc.first.fixture")
        var first = runnableConfiguration(host: "first.fixture").node
        first.rpc = savedRPC
        var second = runnableConfiguration(host: "second.fixture").node
        second.certificate = "/fixture/second.crt"
        var profiles = ConnectionProfiles(); profiles.activeId = "first"
        profiles.profiles = [
            ConnectionProfile(id: "first", name: "First", node: first),
            ConnectionProfile(id: "second", name: "Second", node: second),
        ]
        var configuration = AppConfiguration(); configuration.connections = profiles
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        var node = model.configuration.node
        node.rpc?.host = ""
        model.configuration.node = node

        await model.testNode()
        XCTAssertEqual(model.connectionResult, "Authenticated fixture node.")
        XCTAssertEqual(model.configuration.node.rpc?.host, "",
            "The form keeps the blank-host draft")
        let persisted = try decode(fixture.configurationURL)
        XCTAssertEqual(persisted.connections?.profiles[0].node.rpc, savedRPC,
            "An incomplete draft must not erase the last saved RPC for its profile")
        XCTAssertNil(persisted.connections?.profiles[1].node.rpc,
            "A profile without saved RPC settings remains without them")
    }

    func testIncompleteRPCDraftDoesNotHideSavedNodeSync() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let rpc = completeRPC(host: "rpc.sync.fixture")
        var configuration = runnableConfiguration(host: "sync.fixture")
        configuration.node.rpc = rpc
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        var node = model.configuration.node
        node.rpc?.host = ""
        model.configuration.node = node

        await model.testNode()
        try fixture.publish(mode: "paused", phase: "paused")
        await model.refresh()
        XCTAssertEqual(model.snapshot.node.detail, "Synced",
            "A draft that will fall back to the saved RPC must not clear live sync data")
        XCTAssertEqual(model.snapshot.node.height, 123456)
    }

    func testFinishingSettingsResolvesIncompleteRPCDraft() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let rpc = completeRPC(host: "rpc.done.fixture")
        var configuration = runnableConfiguration(host: "done.fixture")
        configuration.node.rpc = rpc
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        model.sheet = .node
        var node = model.configuration.node
        node.rpc?.host = ""
        model.configuration.node = node

        let finished = await model.finishSettings()
        XCTAssertTrue(finished)
        XCTAssertEqual(model.configuration.node.rpc, rpc,
            "Done resolves the invalid draft back to the saved RPC settings")
        XCTAssertEqual(model.notice,
            "RPC changes not saved: the RPC settings are incomplete.")
        XCTAssertNil(model.errorMessage)
        XCTAssertFalse(model.hasUnsavedSettings)
        let persisted = try decode(fixture.configurationURL)
        XCTAssertEqual(persisted.node.rpc, rpc)
    }

    func testFinishingSettingsReportsDiscardedIncompleteRPCDraft() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let configuration = runnableConfiguration(host: "done.fixture")
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        model.sheet = .node
        model.configuration.node.rpc = NodeRPCConfiguration(host: "rpc.draft.fixture")

        let finished = await model.finishSettings()

        XCTAssertTrue(finished)
        XCTAssertEqual(model.notice,
            "RPC changes not saved: the RPC settings are incomplete.")
        XCTAssertNil(model.errorMessage)
        XCTAssertNil(model.configuration.node.rpc)
        XCTAssertFalse(model.hasUnsavedSettings)
        let persisted = try decode(fixture.configurationURL)
        XCTAssertNil(persisted.node.rpc, "Done must not persist the incomplete draft")
    }

    func testFinishingSettingsWithCompleteProfilesReportsNoNotice() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        var first = runnableConfiguration(host: "first.fixture").node
        first.rpc = completeRPC(host: "rpc.saved.fixture")
        var second = runnableConfiguration(host: "second.fixture").node
        second.certificate = "/fixture/second.crt"
        var profiles = ConnectionProfiles(); profiles.activeId = "first"
        profiles.profiles = [
            ConnectionProfile(id: "first", name: "First", node: first),
            ConnectionProfile(id: "second", name: "Second", node: second),
        ]
        var configuration = AppConfiguration(); configuration.connections = profiles
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        model.sheet = .node
        let edited = completeRPC(host: "rpc.edited.fixture")
        model.configuration.node.rpc = edited

        let finished = await model.finishSettings()

        XCTAssertTrue(finished)
        XCTAssertNil(model.notice)
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.configuration.node.rpc, edited)
        let persisted = try decode(fixture.configurationURL)
        XCTAssertEqual(persisted.connections?.profiles[0].node.rpc, edited)
    }

    func testFinishingSettingsDoesNotReportAnInactiveProfileDraft() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let first = runnableConfiguration(host: "first.fixture").node
        var second = runnableConfiguration(host: "second.fixture").node
        second.certificate = "/fixture/second.crt"
        var profiles = ConnectionProfiles(); profiles.activeId = "first"
        profiles.profiles = [
            ConnectionProfile(id: "first", name: "First", node: first),
            ConnectionProfile(id: "second", name: "Second", node: second),
        ]
        var configuration = AppConfiguration(); configuration.connections = profiles
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        model.sheet = .node
        model.configuration.node.host = "edited.first.fixture"
        model.configuration.connections?.profiles[1].node.rpc =
            NodeRPCConfiguration(host: "rpc.inactive.fixture")

        let finished = await model.finishSettings()

        XCTAssertTrue(finished)
        XCTAssertNil(model.notice, "Only the visible active profile's draft is reported")
        XCTAssertEqual(model.configuration.connections?.profiles[1].node.rpc,
            NodeRPCConfiguration(host: "rpc.inactive.fixture"),
            "An inactive draft stays editable")
        XCTAssertFalse(model.hasUnsavedSettings,
            "A retained inactive draft is not an unsaved persisted setting")
        let configures = try fixture.loggedCommands().filter { $0.hasPrefix("configure ") }.count
        XCTAssertEqual(configures, 1)
        let later = await model.finishSettings()
        XCTAssertTrue(later)
        XCTAssertEqual(try fixture.loggedCommands().filter { $0.hasPrefix("configure ") }.count,
            configures, "A later Done must not save when only a retained draft differs")
    }

    func testFinishingSettingsFromASheetWithoutRPCFieldsReportsNoNotice() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let configuration = runnableConfiguration(host: "done.fixture")
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        model.sheet = .boards
        model.configuration.node.host = "edited.done.fixture"
        model.configuration.node.rpc = NodeRPCConfiguration(host: "rpc.draft.fixture")

        let finished = await model.finishSettings()

        XCTAssertTrue(finished)
        XCTAssertNil(model.notice, "Boards has no RPC fields to explain a dropped draft")
        XCTAssertEqual(model.configuration.node.rpc,
            NodeRPCConfiguration(host: "rpc.draft.fixture"),
            "Done from a sheet without RPC fields leaves the draft editable")
        XCTAssertFalse(model.hasUnsavedSettings,
            "A retained RPC draft is not an unsaved persisted setting")
        let configures = try fixture.loggedCommands().filter { $0.hasPrefix("configure ") }.count
        XCTAssertEqual(configures, 1)
        let later = await model.finishSettings()
        XCTAssertTrue(later)
        XCTAssertEqual(try fixture.loggedCommands().filter { $0.hasPrefix("configure ") }.count,
            configures, "A later Done must not save when only a retained draft differs")
    }

    func testFinishingSettingsDoesNotHideAStartupError() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let configuration = runnableConfiguration(host: "done.fixture")
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        try fixture.setStartupFailure(true)
        await model.restoreStartupIntent()
        XCTAssertEqual(model.errorMessage, "startup fixture failed")
        model.sheet = .node
        model.configuration.node.rpc = NodeRPCConfiguration(host: "rpc.draft.fixture")

        let finished = await model.finishSettings()

        XCTAssertTrue(finished)
        XCTAssertEqual(model.errorMessage, "startup fixture failed")
        XCTAssertEqual(model.notice,
            "RPC changes not saved: the RPC settings are incomplete.")
    }

    func testRPCDraftNoticeClearsOnItsOwn() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let configuration = runnableConfiguration(host: "done.fixture")
        try fixture.writeConfiguration(configuration)
        let model = TimelordModel(
            controller: fixture.controller, initialConfiguration: configuration,
            monitoringEnabled: false, noticeDuration: .milliseconds(20))
        model.sheet = .node
        model.configuration.node.rpc = NodeRPCConfiguration(host: "rpc.draft.fixture")

        let finished = await model.finishSettings()
        XCTAssertTrue(finished)
        XCTAssertNotNil(model.notice)
        let deadline = Date().addingTimeInterval(2)
        while model.notice != nil && Date() < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertNil(model.notice)
    }

    func testNoticeTimerDoesNotRetainTheModel() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let configuration = runnableConfiguration(host: "done.fixture")
        try fixture.writeConfiguration(configuration)
        var model: TimelordModel? = TimelordModel(
            controller: fixture.controller, initialConfiguration: configuration,
            monitoringEnabled: false, noticeDuration: .seconds(60))
        model?.sheet = .node
        model?.configuration.node.rpc = NodeRPCConfiguration(host: "rpc.draft.fixture")

        let finished = await model?.finishSettings()
        XCTAssertEqual(finished, true)
        XCTAssertNotNil(model?.notice)
        try await Task.sleep(for: .milliseconds(10))
        weak var weakModel = model
        model = nil
        XCTAssertNil(weakModel, "The notice timer must not keep the model alive")
    }

    func testFinishingSettingsReportsDiscardedIncompleteProfileRPCDraft() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        let savedRPC = completeRPC(host: "rpc.first.fixture")
        var first = runnableConfiguration(host: "first.fixture").node
        first.rpc = savedRPC
        var second = runnableConfiguration(host: "second.fixture").node
        second.certificate = "/fixture/second.crt"
        var profiles = ConnectionProfiles(); profiles.activeId = "first"
        profiles.profiles = [
            ConnectionProfile(id: "first", name: "First", node: first),
            ConnectionProfile(id: "second", name: "Second", node: second),
        ]
        var configuration = AppConfiguration(); configuration.connections = profiles
        try fixture.writeConfiguration(configuration)
        let model = fixture.model(configuration)
        model.sheet = .node
        var node = model.configuration.node
        node.rpc?.host = ""
        model.configuration.node = node

        let finished = await model.finishSettings()

        XCTAssertTrue(finished)
        XCTAssertEqual(model.notice,
            "RPC changes not saved: the RPC settings are incomplete.")
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.configuration.node.rpc, savedRPC)
        let persisted = try decode(fixture.configurationURL)
        XCTAssertEqual(persisted.connections?.profiles[0].node.rpc, savedRPC)
    }

    func testEditsMadeDuringRestartKeepTheSheetOpenAndTheEdit() async throws {
        let fixture = try StatusFixture()
        defer { fixture.close() }
        var configuration = runnableConfiguration(host: "restart.fixture")
        try fixture.writeConfiguration(configuration)
        try fixture.publish(mode: "running", phase: "worker_started")
        let model = fixture.model(configuration)
        await model.refresh()
        XCTAssertTrue(model.canControlService)
        model.configuration.service.statusListen = "127.0.0.1:18190"

        let finishing = Task { await model.finishSettings() }
        let deadline = Date().addingTimeInterval(3)
        while !model.isBusy && Date() < deadline {
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTAssertTrue(model.isBusy, "The fixture restart must be in progress")
        model.configuration.node.host = "edited.during.restart"
        let finished = await finishing.value

        XCTAssertFalse(finished)
        XCTAssertEqual(model.errorMessage,
            "Settings changed while applying. Review and save again.")
        XCTAssertEqual(model.configuration.node.host, "edited.during.restart")
        let persisted = try decode(fixture.configurationURL)
        XCTAssertEqual(persisted.node.host, "restart.fixture",
            "The edit made during the restart was not silently persisted")
    }

    private func completeRPC(host: String) -> NodeRPCConfiguration {
        NodeRPCConfiguration(
            host: host, port: 8555, serverName: "chia.net",
            certificate: "/fixture/rpc.crt", privateKey: "/fixture/rpc.key",
            caCertificate: "/fixture/rpc-ca.crt")
    }

    private func decode(_ url: URL) throws -> AppConfiguration {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(AppConfiguration.self, from: Data(contentsOf: url))
    }

    private func runnableConfiguration(host: String = "fixture.node") -> AppConfiguration {
        var configuration = AppConfiguration()
        configuration.node.host = host
        configuration.node.certificate = "/fixture/cert"
        configuration.node.privateKey = "/fixture/key"
        configuration.node.caCertificate = "/fixture/ca"
        configuration.hardware.clientBinary = "/fixture/client"
        configuration.hardware.boards = [ConfiguredBoard(id: "fixture", name: "Fixture", port: 18110, location: 1)]
        return configuration
    }
}

/// A CLI-only supervisor fixture: no worker, launchd, hardware, or TCP listener.
@MainActor
private final class StatusFixture {
    let directory: URL
    let response: URL
    let configurationURL: URL
    let controller: ServiceController
    private let commands: URL
    private let statusFailure: URL
    private let pauseFailure: URL
    private let startupFailure: URL
    private let saveFailure: URL

    init(endpoint: URL = URL(string: "http://127.0.0.1:0/v1/status")!, pauseFails: Bool = false, statusFails: Bool = false) throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("supervisor-presentation-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        response = directory.appendingPathComponent("status.json")
        configurationURL = directory.appendingPathComponent("configuration.json")
        commands = directory.appendingPathComponent("commands.log")
        statusFailure = directory.appendingPathComponent("fail-status")
        pauseFailure = directory.appendingPathComponent("fail-pause")
        startupFailure = directory.appendingPathComponent("fail-startup")
        saveFailure = directory.appendingPathComponent("fail-save")
        if pauseFails { try Data().write(to: pauseFailure) }
        if statusFails { try Data().write(to: statusFailure) }
        try Self.write(AppConfiguration(), to: configurationURL)
        let helper = directory.appendingPathComponent("helper")
        let script = """
        #!/bin/sh
        printf '%s\\n' "$*" >> '\(commands.path)'
        case "$1" in
          status)
            if [ -f '\(statusFailure.path)' ]; then
              printf '%s\\n' 'status fixture failed' >&2
              exit 1
            fi
            cat '\(response.path)' ;;
          ensure-started)
            if [ -f '\(startupFailure.path)' ]; then
              printf '%s\\n' 'startup fixture failed' >&2
              exit 1
            fi
            printf '%s' '{"api_version":1,"supervisor_pid":123,"worker_pid":null,"mode":"paused","revision":1,"worker_generation":0,"restarts":0,"phase":"paused"}' ;;
          pause)
            printf '%s' '{"api_version":1,"supervisor_pid":123,"mode":"paused","revision":2,"worker_generation":1,"restarts":0,"phase":"paused"}' > '\(response.path)'
            if [ -f '\(pauseFailure.path)' ]; then
              printf '%s\\n' 'pause fixture failed' >&2
              exit 1
            fi
            cat '\(response.path)' ;;
          resume)
            printf '%s' '{"api_version":1,"supervisor_pid":123,"mode":"running","revision":2,"worker_generation":1,"restarts":0,"phase":"starting"}' > '\(response.path)'
            cat '\(response.path)' ;;
          reload)
            sleep 1
            printf '%s' '{"api_version":1,"supervisor_pid":123,"mode":"paused","revision":2,"worker_generation":2,"restarts":0,"phase":"paused"}' > '\(response.path)'
            cat '\(response.path)' ;;
          configuration)
            cat '\(configurationURL.path)' ;;
          configure)
            if [ -f '\(saveFailure.path)' ]; then
              printf '%s\\n' 'configuration fixture failed' >&2
              exit 1
            fi
            previous=
            for argument in "$@"; do
              if [ "$previous" = "--input" ]; then cp "$argument" '\(configurationURL.path)'; break; fi
              previous=$argument
            done ;;
          devices) printf '%s' '[{"name":"Fixture","location":1}]' ;;
          test-node) printf '%s' '{"authenticated":true,"message":"Authenticated fixture node."}' ;;
          node-state) printf '{"synced":true,"syncing":false,"height":123456,"timestamp_unix":%s}' "$(date +%s)" ;;
          *) printf 'unsupported fixture command: %s\\n' "$*" >&2; exit 1 ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        controller = ServiceController(
            configURL: directory.appendingPathComponent("config.toml"),
            logsURL: directory.appendingPathComponent("log"),
            helperURL: helper, endpoint: endpoint, supervisorURL: helper)
    }
    func model(_ configuration: AppConfiguration) -> TimelordModel {
        TimelordModel(controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
    }
    func writeConfiguration(_ configuration: AppConfiguration) throws {
        try Self.write(configuration, to: configurationURL)
    }
    private static func write(_ configuration: AppConfiguration, to url: URL) throws {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        try encoder.encode(configuration).write(to: url)
    }
    func loggedCommands() throws -> [String] {
        guard FileManager.default.fileExists(atPath: commands.path) else { return [] }
        return try String(contentsOf: commands, encoding: .utf8)
            .split(whereSeparator: \.isNewline)
            .map(String.init)
    }
    func setStartupFailure(_ enabled: Bool) throws { try setMarker(startupFailure, enabled) }
    func setSaveFailure(_ enabled: Bool) throws { try setMarker(saveFailure, enabled) }
    private func setMarker(_ url: URL, _ enabled: Bool) throws {
        if enabled { try Data().write(to: url) } else { try? FileManager.default.removeItem(at: url) }
    }
    func publish(mode: String, phase: String, error: String? = nil, workerPID: Int? = nil) throws {
        var status: [String: Any] = ["api_version": 1, "supervisor_pid": 123, "mode": mode, "revision": 1, "worker_generation": 1, "restarts": 0, "phase": phase]
        if let error { status["error"] = error }
        if let workerPID { status["worker_pid"] = workerPID }
        try JSONSerialization.data(withJSONObject: status).write(to: response)
    }
    func close() { try? FileManager.default.removeItem(at: directory) }
}
