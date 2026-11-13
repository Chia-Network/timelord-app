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
        model.selectConnectionProfile("custom")
        XCTAssertEqual(model.configuration.node, custom)
        XCTAssertNil(model.configuration.node.rpc)
        XCTAssertEqual(model.configuration.connections?.profiles.count, 2)
        XCTAssertTrue(model.hasUnsavedSettings)
        model.selectConnectionProfile("main")
        var expected = mainnet; expected.host = "edited-mainnet.fixture"
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
}
