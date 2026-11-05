import Foundation
import XCTest
@testable import TimelordMac

final class SupervisorClientTests: XCTestCase {
    func testPhaseProtocolRoundTripsTheSharedRustFixture() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let data = try Data(contentsOf: root.appendingPathComponent("fixtures/supervisor-phases.json"))
        let phases = try JSONDecoder().decode([SupervisorPhase].self, from: data)
        XCTAssertEqual(Set(phases), Set(SupervisorPhase.allCases))
        let roundTrip = try JSONDecoder().decode([String].self, from: JSONEncoder().encode(phases))
        XCTAssertEqual(roundTrip, try JSONDecoder().decode([String].self, from: data))
        XCTAssertThrowsError(try JSONDecoder().decode(SupervisorPhase.self, from: Data("\"unknown_phase\"".utf8)))
    }

    func testActualSupervisorPersistsPauseRejectsStaleCommandsAndResumesWithoutGuiOwnership() async throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        let helper = root.appendingPathComponent("build/rust/debug/timelord-supervisor")
        guard FileManager.default.isExecutableFile(atPath: helper.path) else { throw XCTSkip("Build the supervisor with scripts/check.sh first.") }
        let directory = URL(fileURLWithPath: "/private/tmp/tls-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let config = directory.appendingPathComponent("config.toml")
        let worker = directory.appendingPathComponent("worker")
        try Data("#!/bin/sh\nexec /bin/sleep 60\n".utf8).write(to: worker)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: worker.path)
        let process = Process(); process.executableURL = helper
        process.arguments = ["run", "--config", config.path, "--worker", worker.path]
        process.standardOutput = FileHandle.nullDevice; process.standardError = FileHandle.nullDevice
        try process.run()
        defer {
            if process.isRunning { process.terminate(); process.waitUntilExit() }
            try? FileManager.default.removeItem(at: directory)
        }
        let client = SupervisorClient(helper: helper, config: config)
        let deadline = Date().addingTimeInterval(8)
        var current: SupervisorStatus?
        while Date() < deadline {
            if let status = try? await client.send(.status), status.workerPid != nil { current = status; break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        let running = try XCTUnwrap(current)
        XCTAssertEqual(running.mode, .running)
        XCTAssertNotEqual(running.workerPid, running.supervisorPid)
        let paused = try await client.send(.pause, expectedRevision: running.revision)
        XCTAssertEqual(paused.mode, .paused)
        XCTAssertNil(paused.workerPid, "Pause acknowledgment must follow worker cleanup")
        do {
            _ = try await client.send(.resume, expectedRevision: running.revision)
            XCTFail("Stale control revision resumed production")
        } catch { XCTAssertFalse(error.localizedDescription.isEmpty) }
        let resumed = try await client.send(.resume, expectedRevision: paused.revision)
        XCTAssertEqual(resumed.mode, .running)
        _ = try await client.send(.pause)
    }
}
