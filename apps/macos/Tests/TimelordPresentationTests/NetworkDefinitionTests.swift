import Foundation
import XCTest
@testable import TimelordPresentation

final class NetworkDefinitionTests: XCTestCase {
    func testImportedNetworkSurvivesSettingsRoundTripWithExactConstants() throws {
        let data = Data("""
        {"host":"fixture.invalid","port":58445,"network":"ephemeral-fixture",
         "network_definition":{"schema_version":1,"network_id":"ephemeral-fixture",
          "exported_at":"fixture","chia_version":"fixture",
          "constants":{"GENESIS_CHALLENGE":"abababababababababababababababababababababababababababababababab",
           "DIFFICULTY_CONSTANT_FACTOR":10052721566054,"MAX_COIN_AMOUNT":18446744073709551615,
           "PLOT_FILTER_V2_RELATIVE_HEIGHT":[33000,30000,9216]},
          "config_overrides":{"address_prefix":"txch"}}}
        """.utf8)
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        var node = try decoder.decode(NodeConfiguration.self, from: data)
        XCTAssertEqual(node.network, "ephemeral-fixture")
        XCTAssertEqual(node.networkDefinition?.networkId, node.network)
        node.host = "changed.fixture.invalid"
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let encoded = try encoder.encode(node)
        let restored = try decoder.decode(NodeConfiguration.self, from: encoded)
        XCTAssertEqual(restored, node)
        XCTAssertEqual(restored.networkDefinition?.constants["MAX_COIN_AMOUNT"], .number(Decimal(string: "18446744073709551615")!))
        XCTAssertEqual(restored.networkDefinition?.constants["DIFFICULTY_CONSTANT_FACTOR"], .number(Decimal(string: "10052721566054")!))
        XCTAssertEqual(restored.networkDefinition?.configOverrides["address_prefix"], .string("txch"))
    }

    func testBuiltInConfigurationWithoutDefinitionRemainsCompatible() throws {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let node = try decoder.decode(NodeConfiguration.self, from: Data("{\"host\":\"localhost\",\"port\":8444,\"network\":\"mainnet\"}".utf8))
        XCTAssertNil(node.networkDefinition)
    }
}
