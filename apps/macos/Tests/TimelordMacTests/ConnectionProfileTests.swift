import Foundation
import XCTest
@testable import TimelordMac
import TimelordPresentation

@MainActor
final class ConnectionProfileTests: XCTestCase {
    func testSwitchingProfilesRestoresCompleteNodesAndPreservesEdits() throws {
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
        model.selectConnectionProfile("custom")
        XCTAssertEqual(model.configuration.node, custom)
        XCTAssertNil(model.configuration.node.rpc)
        XCTAssertEqual(model.configuration.connections?.profiles.count, 2)
        XCTAssertTrue(model.hasUnsavedSettings)
        model.selectConnectionProfile("main")
        var expected = mainnet; expected.host = "edited-mainnet.fixture"
        expected.rpc?.host = "corrected-rpc.fixture"; expected.rpc?.port = 58555
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

}
