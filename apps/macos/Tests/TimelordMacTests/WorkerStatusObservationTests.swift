import Foundation
import XCTest
@testable import TimelordMac
import TimelordPresentation

final class WorkerStatusObservationTests: XCTestCase {
    func testConnectionGraceIsBoundedAndDoesNotRestartForTheSameWorker() {
        let now = ContinuousClock.now
        var observation = WorkerStatusObservation.absent
        observation.observe(pid: 10, now: now)
        observation.failed(pid: 10, error: URLError(.timedOut), now: now)
        XCTAssertTrue(observation.isStarting(at: now))
        let later = now.advanced(by: .seconds(60))
        observation.observe(pid: 10, now: later)
        observation.failed(pid: 10, error: URLError(.cannotConnectToHost), now: later)
        XCTAssertFalse(observation.isStarting(at: later))
        XCTAssertNotNil(observation.error)
        observation.observe(pid: 11, now: later)
        XCTAssertNil(observation.error)
        XCTAssertTrue(observation.isStarting(at: later))
        observation.received(pid: 10) // A late response cannot certify the replacement.
        XCTAssertTrue(observation.isStarting(at: later))
        observation.received(pid: 11)
        XCTAssertNil(observation.error)
        observation.failed(pid: 11, error: URLError(.timedOut), now: later)
        XCTAssertFalse(observation.isStarting(at: later), "An established endpoint outage is immediate")
    }
    func testMissingSupervisorPollPreservesWorkerFailureUntilConfirmedPause() {
        let now = ContinuousClock.now
        let running = SupervisorStatus(apiVersion: 1, supervisorPid: 2, workerPid: 10, mode: .running,
            revision: 1, workerGeneration: 1, restarts: 0, phase: .workerStarted, error: nil)
        let paused = SupervisorStatus(apiVersion: 1, supervisorPid: 2, workerPid: nil, mode: .paused,
            revision: 2, workerGeneration: 1, restarts: 0, phase: .paused, error: nil)
        var observation = WorkerStatusObservation.absent
        observation.observe(supervisor: running, now: now)
        observation.received(pid: 10)
        observation.failed(pid: 10, error: URLError(.timedOut), now: now)
        observation.observe(supervisor: nil, now: now)
        observation.observe(supervisor: running, now: now)
        XCTAssertNotNil(observation.failure(at: now))
        XCTAssertFalse(observation.isStarting(at: now))
        observation.observe(supervisor: paused, now: now)
        XCTAssertNil(observation.failure(at: now))
    }

    func testGraceExpiryRemainsAFailureWithoutAnotherHTTPPoll() {
        let now = ContinuousClock.now
        var observation = WorkerStatusObservation.absent
        observation.observe(pid: 10, now: now)
        let deadline = now.advanced(by: .seconds(60))
        XCTAssertNil(observation.failure(at: deadline.advanced(by: .nanoseconds(-1))))
        for instant in [deadline, deadline.advanced(by: .seconds(30))] {
            // Discovery/settings may rebuild presentation before another fetch finishes.
            let snapshot = LivePresentation.snapshot(status: nil,
                controlOperation: observation.isStarting(at: instant) ? .start : nil, error: observation.failure(at: instant))
            XCTAssertEqual(snapshot.phase, .serviceUnavailable)
            XCTAssertFalse(snapshot.readyToRun)
            XCTAssertEqual(snapshot.explanation, "The local service did not respond within 60 seconds. Check its log.")
        }
        observation.received(pid: 10)
        XCTAssertNil(observation.failure(at: deadline))
    }

    func testInvalidStatusBypassesStartupGraceAndNoProcessClearsIt() {
        var observation = WorkerStatusObservation.absent
        observation.observe(pid: 10)
        observation.failed(pid: 10, error: StatusError.differentOwner)
        XCTAssertFalse(observation.isStarting())
        observation.observe(pid: nil)
        observation.observe(pid: 10)
        XCTAssertTrue(observation.isStarting())
    }
}
