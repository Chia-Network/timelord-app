import Foundation
import XCTest
@testable import TimelordMac
import TimelordPresentation

@MainActor
final class ConnectionProfileTests: XCTestCase {
    func testSwitchingProfilesKeepsEditsWithTheProfileThatOwnsThem() throws {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let mainnet = try decoder.decode(NodeConfiguration.self, from: Data("""
        {"host":"mainnet.fixture","port":8444,"network":"mainnet","server_name":"chia.net",
         "certificate":"/fixture/main.crt","private_key":"/fixture/main.key","ca_certificate":"/fixture/main-ca.crt",
         "rpc":{"host":"rpc-mainnet.fixture","port":8555,"server_name":"chia.net",
         "certificate":"/fixture/rpc.crt","private_key":"/fixture/rpc.key","ca_certificate":"/fixture/rpc-ca.crt"}}
        """.utf8))
        let custom = try decoder.decode(NodeConfiguration.self, from: Data("""
        {"host":"custom.fixture","port":58445,"network":"ephemeral-fixture",
         "certificate":"/fixture/custom.crt","private_key":"/fixture/custom.key","ca_certificate":"/fixture/custom-ca.crt",
         "network_definition":{"schema_version":1,"network_id":"ephemeral-fixture","exported_at":"fixture","chia_version":"fixture",
         "constants":{"GENESIS_CHALLENGE":"abababababababababababababababababababababababababababababababab"},"config_overrides":{}}}
        """.utf8))
        var config = AppConfiguration(); config.node = mainnet
        var profiles = ConnectionProfiles(); profiles.activeId = "main"
        profiles.profiles = [ConnectionProfile(id: "main", name: "Mainnet", node: mainnet), ConnectionProfile(id: "custom", name: "Custom", node: custom)]
        config.connections = profiles
        let model = TimelordModel(initialConfiguration: config, monitoringEnabled: false)
        model.configuration.node.host = "edited-mainnet.fixture"
        model.configuration.node.rpc?.host = "corrected-rpc.fixture"
        model.configuration.node.rpc?.port = 58555
        var expected = mainnet; expected.host = "edited-mainnet.fixture"
        expected.rpc?.host = "corrected-rpc.fixture"; expected.rpc?.port = 58555
        model.selectConnectionProfile("custom")
        XCTAssertEqual(model.configuration.node, custom)
        XCTAssertNil(model.configuration.node.rpc)
        XCTAssertEqual(model.configuration.connections?.profiles.count, 2)
        // Edits live on the active profile, so switching away does not discard them.
        XCTAssertEqual(model.configuration.connections?.profiles[0].node, expected)
        XCTAssertTrue(model.hasUnsavedSettings)
        model.selectConnectionProfile("main")
        XCTAssertEqual(model.configuration.node, expected)
        XCTAssertNotNil(model.configuration.node.rpc)
        XCTAssertNil(model.configuration.node.networkDefinition)
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let restored = try decoder.decode(AppConfiguration.self, from: encoder.encode(model.configuration))
        XCTAssertEqual(restored.connections?.profiles[1].node, custom)
        XCTAssertEqual(restored.node, expected)
        model.selectConnectionProfile("unknown")
        XCTAssertEqual(model.configuration.node, expected)
    }

    func testConfigurationDerivesNodeFromTheActiveProfileAndRejectsDivergence() throws {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        var custom = NodeConfiguration(); custom.host = "custom.fixture"; custom.port = 58445
        custom.network = "ephemeral-fixture"
        var other = NodeConfiguration(); other.host = "other.fixture"
        var config = AppConfiguration()
        var profiles = ConnectionProfiles(); profiles.activeId = "custom"
        profiles.profiles = [
            ConnectionProfile(id: "custom", name: "Custom", node: custom),
            ConnectionProfile(id: "other", name: "Other", node: other),
        ]
        config.connections = profiles
        let data = try encoder.encode(config)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertNil(object["node"])
        let restored = try decoder.decode(AppConfiguration.self, from: data)
        XCTAssertEqual(restored.node, custom)
        XCTAssertEqual(restored.connections?.activeId, "custom")

        var matching = object
        matching["node"] = try JSONSerialization.jsonObject(with: encoder.encode(custom))
        let migrated = try decoder.decode(
            AppConfiguration.self, from: JSONSerialization.data(withJSONObject: matching))
        XCTAssertEqual(migrated.node, custom)

        var diverging = matching
        var edited = custom; edited.host = "ignored.fixture"
        diverging["node"] = try JSONSerialization.jsonObject(with: encoder.encode(edited))
        XCTAssertThrowsError(
            try decoder.decode(
                AppConfiguration.self, from: JSONSerialization.data(withJSONObject: diverging)))

        var single = AppConfiguration(); single.node.host = "single.fixture"
        let restoredSingle = try decoder.decode(AppConfiguration.self, from: encoder.encode(single))
        XCTAssertEqual(restoredSingle.node, single.node)
        let singleObject = try XCTUnwrap(
            JSONSerialization.jsonObject(with: encoder.encode(single)) as? [String: Any])
        var missingService = singleObject; missingService.removeValue(forKey: "service")
        XCTAssertThrowsError(
            try decoder.decode(
                AppConfiguration.self, from: JSONSerialization.data(withJSONObject: missingService)))
    }
    func testSaveReloadsProfilesCreatedByHelper() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-save-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let helper = directory.appendingPathComponent("helper")
        let canonical = directory.appendingPathComponent("canonical.json")
        let initial = AppConfiguration()
        var persisted = initial
        var profiles = ConnectionProfiles(); profiles.activeId = "connection-1"
        profiles.profiles = [ConnectionProfile(id: "connection-1", name: "Mainnet", node: initial.node)]
        persisted.connections = profiles
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        try encoder.encode(persisted).write(to: canonical)
        try Data("#!/bin/sh\nif [ \"$1\" = configuration ]; then cat '\(canonical.path)'; fi\n".utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        let controller = ServiceController(configURL: directory.appendingPathComponent("config.toml"), helperURL: helper, supervisorURL: helper)
        let model = TimelordModel(controller: controller, initialConfiguration: initial, monitoringEnabled: false)
        await model.saveConfiguration()
        XCTAssertNil(model.errorMessage)
        XCTAssertEqual(model.configuration.connections?.activeId, "connection-1")
        XCTAssertEqual(model.configuration.connections?.profiles.count, 1)
        XCTAssertFalse(model.hasUnsavedSettings)
        await model.saveConfiguration()
        XCTAssertEqual(model.configuration.connections?.profiles.count, 1)
    }

    func testSavingProjectsTheStagedNodeIntoTheActiveProfile() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-project-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let helper = directory.appendingPathComponent("helper")
        let captured = directory.appendingPathComponent("submitted.json")
        // Capture `configure` input, then return it for the reload.
        try Data("""
        #!/bin/sh
        if [ "$1" = configuration ]; then cat '\(captured.path)'; exit 0; fi
        previous=
        for argument in "$@"; do
          if [ "$previous" = "--input" ]; then cp "$argument" '\(captured.path)'; break; fi
          previous=$argument
        done
        """.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        var other = NodeConfiguration(); other.host = "other.fixture"
        var profiles = ConnectionProfiles(); profiles.activeId = "active"
        profiles.profiles = [
            ConnectionProfile(id: "active", name: "Active", node: NodeConfiguration()),
            ConnectionProfile(id: "other", name: "Other", node: other),
        ]
        var config = AppConfiguration(); config.connections = profiles
        let controller = ServiceController(
            configURL: directory.appendingPathComponent("config.toml"), helperURL: helper,
            supervisorURL: helper)
        let model = TimelordModel(
            controller: controller, initialConfiguration: config, monitoringEnabled: false)
        model.configuration.node.host = "edited.fixture"
        await model.saveConfiguration()
        XCTAssertNil(model.errorMessage)
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let submittedData = try Data(contentsOf: captured)
        let submitted = try decoder.decode(AppConfiguration.self, from: submittedData)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: submittedData) as? [String: Any])
        XCTAssertNil(object["node"])
        XCTAssertEqual(submitted.connections?.profiles[0].node.host, "edited.fixture")
        XCTAssertEqual(submitted.connections?.profiles[1].node.host, "other.fixture")
        XCTAssertEqual(model.configuration.connections?.profiles[0].node.host, "edited.fixture")
        XCTAssertEqual(model.configuration.node.host, "edited.fixture")
        XCTAssertFalse(model.hasUnsavedSettings)
    }

    func testRemovingProfilesSelectsRemainingConnectionAndLastRemovalReturnsToSetup() throws {
        var first = NodeConfiguration(); first.host = "first.fixture"
        first.certificate = "/fixture/first.crt"; first.privateKey = "/fixture/first.key"; first.caCertificate = "/fixture/ca.crt"
        first.rpc = NodeRPCConfiguration(host: "rpc-first.fixture", certificate: "/fixture/rpc.crt", privateKey: "/fixture/rpc.key", caCertificate: "/fixture/ca.crt")
        var second = first; second.host = "second.fixture"; second.rpc?.host = "rpc-second.fixture"
        var profiles = ConnectionProfiles(); profiles.activeId = "first"
        profiles.profiles = [ConnectionProfile(id: "first", name: "First", node: first), ConnectionProfile(id: "second", name: "Second", node: second)]
        var config = AppConfiguration(); config.node = first; config.connections = profiles
        let model = TimelordModel(initialConfiguration: config, monitoringEnabled: false)
        model.removeConnectionProfile("missing")
        XCTAssertEqual(model.configuration, config)
        model.removeConnectionProfile("first")
        XCTAssertEqual(model.configuration.connections?.activeId, "second")
        XCTAssertEqual(model.configuration.node, second)
        XCTAssertTrue(model.hasUnsavedSettings)
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let restored = try decoder.decode(AppConfiguration.self, from: encoder.encode(model.configuration))
        XCTAssertEqual(restored.connections?.profiles.map(\.id), ["second"])
        XCTAssertEqual(restored.node.rpc, second.rpc)
        model.removeConnectionProfile("second")
        XCTAssertEqual(model.configuration.connections?.profiles.count, 0)
        XCTAssertNil(model.configuration.connections?.activeId)
        XCTAssertEqual(model.configuration.node, NodeConfiguration())
        XCTAssertFalse(model.canStartService)
    }

    func testRemovingInactiveProfilePreservesActiveEdits() {
        var profiles = ConnectionProfiles(); profiles.activeId = "keep"
        profiles.profiles = [ConnectionProfile(id: "keep", name: "Keep", node: NodeConfiguration()), ConnectionProfile(id: "remove", name: "Remove", node: NodeConfiguration())]
        var config = AppConfiguration(); config.connections = profiles
        let model = TimelordModel(initialConfiguration: config, monitoringEnabled: false)
        model.configuration.node.host = "edited.fixture"
        model.removeConnectionProfile("remove")
        XCTAssertEqual(model.configuration.node.host, "edited.fixture")
        XCTAssertEqual(model.configuration.connections?.activeId, "keep")
        XCTAssertEqual(model.configuration.connections?.profiles.map(\.id), ["keep"])
    }

    func testIncompleteRPCDraftDoesNotPoisonProfileSwitchOrSave() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-rpc-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let helper = directory.appendingPathComponent("helper")
        let captured = directory.appendingPathComponent("submitted.json")
        try Data("""
        #!/bin/sh
        if [ "$1" = configuration ]; then cat '\(captured.path)'; exit 0; fi
        previous=
        for argument in "$@"; do
          if [ "$previous" = "--input" ]; then cp "$argument" '\(captured.path)'; break; fi
          previous=$argument
        done
        """.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)

        var first = NodeConfiguration(); first.host = "first.fixture"
        first.certificate = "/fixture/first.crt"; first.privateKey = "/fixture/first.key"
        first.caCertificate = "/fixture/ca.crt"
        var second = first; second.host = "second.fixture"
        var profiles = ConnectionProfiles(); profiles.activeId = "first"
        profiles.profiles = [
            ConnectionProfile(id: "first", name: "First", node: first),
            ConnectionProfile(id: "second", name: "Second", node: second),
        ]
        var configuration = AppConfiguration(); configuration.connections = profiles
        let controller = ServiceController(
            configURL: directory.appendingPathComponent("config.toml"), helperURL: helper,
            supervisorURL: helper)
        let model = TimelordModel(
            controller: controller, initialConfiguration: configuration, monitoringEnabled: false)
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase

        // Toggling sync status creates an RPC draft before any credential exists.
        let draft = NodeRPCConfiguration(host: model.configuration.node.host)
        model.configuration.node.rpc = draft
        XCTAssertNotNil(model.configuration.node.rpc)
        XCTAssertTrue(model.hasUnsavedSettings, "The draft is unsaved UI state, not a persisted RPC profile")
        model.selectConnectionProfile("second")
        let switched = await model.saveConfiguration()
        XCTAssertTrue(switched, "An incomplete RPC draft must not block a profile switch")
        XCTAssertNil(model.errorMessage)
        let switchedConfig = try decoder.decode(AppConfiguration.self, from: Data(contentsOf: captured))
        XCTAssertNil(switchedConfig.connections?.profiles[0].node.rpc)
        XCTAssertEqual(switchedConfig.connections?.activeId, "second")
        XCTAssertEqual(model.configuration.connections?.profiles[0].node.rpc, draft,
            "The saved configuration strips the draft, but the form keeps it")
        XCTAssertTrue(model.hasUnsavedSettings)

        // Turning the draft off persists the absence of RPC settings.
        model.configuration.connections?.profiles[0].node.rpc = nil
        let clearedDraft = await model.saveConfiguration()
        XCTAssertTrue(clearedDraft)
        XCTAssertFalse(model.hasUnsavedSettings)

        let valid = NodeRPCConfiguration(
            host: "rpc.second.fixture", port: 8555, serverName: "chia.net",
            certificate: "/fixture/rpc.crt", privateKey: "/fixture/rpc.key",
            caCertificate: "/fixture/rpc-ca.crt")
        model.configuration.node.rpc = valid
        let enabledSave = await model.saveConfiguration()
        XCTAssertTrue(enabledSave)
        let enabled = try decoder.decode(AppConfiguration.self, from: Data(contentsOf: captured))
        XCTAssertEqual(enabled.connections?.profiles[1].node.rpc, valid)
        XCTAssertEqual(model.configuration.node.rpc, valid)

        model.configuration.node.rpc = nil
        let disabledSave = await model.saveConfiguration()
        XCTAssertTrue(disabledSave)
        let disabled = try decoder.decode(AppConfiguration.self, from: Data(contentsOf: captured))
        XCTAssertNil(disabled.connections?.profiles[1].node.rpc)
        XCTAssertNil(model.configuration.node.rpc)
        XCTAssertFalse(model.hasUnsavedSettings)
    }

    func testRemovingTheLastProfileDoesNotReuseItsSavedRPCForANewDraft() {
        var savedNode = NodeConfiguration()
        savedNode.host = "saved.fixture"
        savedNode.certificate = "/fixture/cert"
        savedNode.privateKey = "/fixture/key"
        savedNode.caCertificate = "/fixture/ca"
        savedNode.rpc = NodeRPCConfiguration(
            host: "rpc.saved.fixture", certificate: "/fixture/rpc.crt",
            privateKey: "/fixture/rpc.key", caCertificate: "/fixture/rpc-ca.crt")
        var profiles = ConnectionProfiles(); profiles.activeId = "only"
        profiles.profiles = [ConnectionProfile(id: "only", name: "Only", node: savedNode)]
        var saved = AppConfiguration(); saved.connections = profiles
        let model = TimelordModel(initialConfiguration: saved, monitoringEnabled: false)

        model.removeConnectionProfile("only")
        var setupNode = model.configuration.node
        setupNode.rpc = NodeRPCConfiguration(host: "rpc.new.fixture")
        model.configuration.node = setupNode

        let resolved = model.configuration.resolvedForPersistence(against: saved)
        XCTAssertNil(resolved.node.rpc,
            "A single-node draft must not inherit the removed profile's saved RPC")
        var finished = model.configuration
        XCTAssertTrue(finished.resolveActiveIncompleteRPC(against: saved))
        XCTAssertNil(finished.node.rpc)
    }

    func testSingleNodeDraftMigratesToTheHelperCreatedActiveProfile() {
        var active = NodeConfiguration()
        active.host = "migrated.fixture"
        active.certificate = "/fixture/cert"
        active.privateKey = "/fixture/key"
        active.caCertificate = "/fixture/ca"
        var profiles = ConnectionProfiles(); profiles.activeId = "connection-1"
        profiles.profiles = [ConnectionProfile(id: "connection-1", name: "Migrated", node: active)]
        var persisted = AppConfiguration(); persisted.connections = profiles
        var draft = AppConfiguration(); draft.node = active
        draft.node.rpc = NodeRPCConfiguration(host: "rpc.draft.fixture")

        let restored = persisted.restoringIncompleteRPCDrafts(from: draft)

        XCTAssertEqual(restored.node.rpc, draft.node.rpc)
        XCTAssertEqual(restored.connections?.profiles[0].node.rpc, draft.node.rpc)
    }

}
