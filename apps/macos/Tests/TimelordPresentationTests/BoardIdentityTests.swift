import Foundation
import XCTest
@testable import TimelordPresentation

final class BoardIdentityTests: XCTestCase {
    private struct File: Decodable {
        let vectors: [Vector]
    }
    private struct Vector: Decodable {
        let name: String
        let boards: [Board]
        let rememberedBoards: [Board]
        let devices: [Device]
        let expectedBoards: [Board]
        let expectedRememberedBoards: [Board]
    }
    private struct Board: Decodable {
        let id: String
        let name: String?
        let clientPort: Int?
        let deviceLocation: Int?
        let serial: String?
    }
    private struct Device: Decodable {
        let name: String?
        let location: Int
        let serial: String?
    }

    private func vectorURL() -> URL {
        var url = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        let relative = "tests/board-identity-vectors.json"
        while url.path != "/" {
            let candidate = url.appendingPathComponent(relative)
            if FileManager.default.fileExists(atPath: candidate.path) { return candidate }
            url.deleteLastPathComponent()
        }
        return URL(fileURLWithPath: "/nonexistent/\(relative)")
    }
    private func vectors() throws -> [Vector] {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(File.self, from: Data(contentsOf: vectorURL())).vectors
    }
    private func configured(_ board: Board, index: Int) -> ConfiguredBoard {
        var result = ConfiguredBoard(
            id: board.id,
            name: board.name ?? "ASIC \(index + 1)",
            port: board.clientPort ?? (18110 + index),
            location: board.deviceLocation ?? 0)
        result.serial = board.serial
        return result
    }
    private func boardKey(_ board: ConfiguredBoard) -> (String, String, Int) {
        (board.serial ?? "", board.id, board.deviceLocation ?? 0)
    }
    private func expectedBoardKey(_ board: Board) -> (String, String, Int) {
        (board.serial ?? "", board.id, board.deviceLocation ?? 0)
    }
    private func sortedBoards(_ boards: [ConfiguredBoard]) -> [ConfiguredBoard] {
        boards.sorted {
            let a = boardKey($0), b = boardKey($1)
            if a.0 != b.0 { return a.0 < b.0 }
            if a.1 != b.1 { return a.1 < b.1 }
            return a.2 < b.2
        }
    }
    private func sortedExpectedBoards(_ boards: [Board]) -> [Board] {
        boards.sorted {
            let a = expectedBoardKey($0), b = expectedBoardKey($1)
            if a.0 != b.0 { return a.0 < b.0 }
            if a.1 != b.1 { return a.1 < b.1 }
            return a.2 < b.2
        }
    }
    private func assertBoards(_ actual: [ConfiguredBoard], _ expected: [Board], _ name: String) {
        let actual = sortedBoards(actual)
        let expected = sortedExpectedBoards(expected)
        XCTAssertEqual(actual.count, expected.count, name)
        for (board, expected) in zip(actual, expected) {
            XCTAssertEqual(board.id, expected.id, name)
            XCTAssertEqual(board.serial, expected.serial, name)
            XCTAssertEqual(board.deviceLocation, expected.deviceLocation, name)
        }
    }

    func testSharedBoardIdentityVectorsMatchReconcile() throws {
        let vectors = try vectors()
        XCTAssertGreaterThanOrEqual(vectors.count, 12, "shared board identity vectors are missing")
        for vector in vectors {
            var hardware = HardwareConfiguration()
            hardware.boards = vector.boards.enumerated().map { configured($0.element, index: $0.offset) }
            hardware.rememberedBoards = vector.rememberedBoards.enumerated().map {
                configured($0.element, index: $0.offset)
            }
            let devices = vector.devices.map {
                DiscoveredDevice(name: $0.name ?? "ASIC", location: $0.location, serial: $0.serial)
            }
            let reconciled = BoardDiscovery.reconcile(
                devices, hardware: hardware, service: ServiceConfiguration())
            assertBoards(reconciled.boards, vector.expectedBoards, vector.name)
            let ids = reconciled.boards.map(\.id)
            XCTAssertEqual(Set(ids).count, ids.count, vector.name)
            let remembered = reconciled.rememberedBoards.filter { old in
                !reconciled.boards.contains { BoardDiscovery.samePhysicalBoard($0, old) }
            }
            assertBoards(remembered, vector.expectedRememberedBoards, vector.name)
        }
    }

    func testWaitingCardIsNotSuppressedByAStaleConfiguredLocation() {
        let live = BoardSnapshot(
            id: "usb-4113", name: "ASIC 1", state: .waiting, temperatureC: nil,
            temperatureLimitC: 55, profile: "Automatic", frequencyMHz: nil, voltageV: nil,
            histories: [], lastProof: nil)
        var configured = ConfiguredBoard(
            id: "usb-9001", name: "ASIC 1", port: 18110, location: 4113)
        configured.serial = "serial-0"
        let device = DiscoveredDevice(name: "ASIC", location: 9001, serial: "serial-0")
        XCTAssertFalse(
            LivePresentation.samePhysicalBoard(live, configured: configured, device: device))
    }

    func testStaleServiceIdentityLeavesMovePlusNewBoardUnique() {
        var hardware = HardwareConfiguration()
        var board = ConfiguredBoard(id: "usb-4113", name: "ASIC 1", port: 18110, location: 4113)
        board.serial = "serial-0"
        hardware.boards = [board]
        hardware = BoardDiscovery.reconcile(
            [DiscoveredDevice(name: "ASIC", location: 9001, serial: "serial-0"),
             DiscoveredDevice(name: "ASIC", location: 4113, serial: "serial-1")],
            hardware: hardware, service: ServiceConfiguration())
        XCTAssertEqual(hardware.boards.map(\.id), ["usb-4113", "usb-4113-1"])
        let adopted = BoardDiscovery.adoptingServiceIdentities(hardware, identities: [
            ServiceBoardIdentity(id: "usb-4113", name: "ASIC 1", serial: "serial-0", location: 4113)
        ])
        XCTAssertEqual(adopted.boards.map(\.id), ["usb-4113", "usb-4113-1"])
        let ids = adopted.boards.map(\.id) + adopted.rememberedBoards.map(\.id)
        XCTAssertEqual(Set(ids).count, ids.count)
    }
}
