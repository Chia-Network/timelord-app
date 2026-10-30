import Foundation
import XCTest
@testable import TimelordMac

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
        observation.observe(pid: 11, now: later)
        XCTAssertTrue(observation.isStarting(at: later))
        observation.received(pid: 10) // A late response cannot certify the replacement.
        XCTAssertTrue(observation.isStarting(at: later))
        observation.received(pid: 11)
        observation.failed(pid: 11, error: URLError(.timedOut), now: later)
        XCTAssertFalse(observation.isStarting(at: later), "An established endpoint outage is immediate")
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
