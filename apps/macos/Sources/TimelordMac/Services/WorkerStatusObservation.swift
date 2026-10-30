import Foundation

/// A process starting and its HTTP endpoint responding are separate observations.
/// Only connection failures get a bounded first-response grace period.
enum WorkerStatusObservation {
    case absent
    case awaiting(pid: Int32, until: ContinuousClock.Instant)
    case responsive(pid: Int32)
    case unavailable(pid: Int32)

    private var pid: Int32? {
        switch self {
        case .absent: return nil
        case .awaiting(let pid, _), .responsive(let pid), .unavailable(let pid): return pid
        }
    }
    mutating func observe(pid: Int32?, now: ContinuousClock.Instant = .now) {
        guard pid != self.pid else { return }
        self = pid.map { .awaiting(pid: $0, until: now.advanced(by: .seconds(60))) } ?? .absent
    }
    func isStarting(at now: ContinuousClock.Instant = .now) -> Bool {
        if case .awaiting(_, let until) = self { return now < until }
        return false
    }
    mutating func received(pid: Int32) {
        guard self.pid == pid else { return }
        self = .responsive(pid: pid)
    }
    mutating func failed(pid: Int32, error: Error, now: ContinuousClock.Instant = .now) {
        guard self.pid == pid else { return }
        if isStarting(at: now), let code = (error as? URLError)?.code,
           [.cannotConnectToHost, .timedOut, .networkConnectionLost].contains(code) { return }
        self = .unavailable(pid: pid)
    }
}
