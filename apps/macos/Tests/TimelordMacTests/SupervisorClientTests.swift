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
        let acknowledged = try await client.send(.pause, expectedRevision: running.revision)
        XCTAssertEqual(acknowledged.mode, .paused, "Pause is durable when acknowledged")
        // Retirement continues in the background; status reports its completion.
        var settled: SupervisorStatus?
        let pauseDeadline = Date().addingTimeInterval(20)
        while Date() < pauseDeadline {
            let status = try await client.send(.status)
            if status.phase == .paused { settled = status; break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        let paused = try XCTUnwrap(settled)
        XCTAssertNil(paused.workerPid, "Paused phase follows worker cleanup")
        do {
            _ = try await client.send(.resume, expectedRevision: running.revision)
            XCTFail("Stale control revision resumed production")
        } catch { XCTAssertFalse(error.localizedDescription.isEmpty) }
        let resumed = try await client.send(.resume, expectedRevision: paused.revision)
        XCTAssertEqual(resumed.mode, .running)
        _ = try await client.send(.pause)
    }

    @MainActor
    func testReloadWaitsForTheReplacementWorkerAfterSlowRetirement() async throws {
        let fixture = try SupervisedFixture(workerBody: """
        trap 'sleep 3; exit 0' TERM
        while :; do sleep 0.1; done
        """)
        defer { fixture.close() }
        try await fixture.launch()
        let before = try XCTUnwrap(fixture.controller.supervisor)
        let oldPID = try XCTUnwrap(before.workerPid)
        try await fixture.controller.reload()
        let after = try XCTUnwrap(fixture.controller.supervisor)
        XCTAssertEqual(after.phase, .workerStarted)
        XCTAssertEqual(after.workerGeneration, before.workerGeneration + 1)
        XCTAssertNotEqual(after.workerPid, oldPID)
        XCTAssertNotEqual(kill(oldPID, 0), 0, "The old worker family was retired before its replacement")
        try await fixture.controller.stop()
        XCTAssertEqual(fixture.controller.supervisor?.phase, .paused)
        XCTAssertNil(fixture.controller.processID)
    }
}
