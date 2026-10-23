import Foundation

public enum Chain: String, CaseIterable, Identifiable, Codable, Sendable {
    case challenge, reward
    case infusedChallenge = "infused_challenge"
    public var id: String { rawValue }
    public var title: String {
        switch self { case .challenge: return "Challenge"; case .reward: return "Reward"; case .infusedChallenge: return "Infused challenge" }
    }
}

public enum HistoryRange: String, CaseIterable, Identifiable, Sendable {
    case quarterHour, hour, day
    public var id: String { rawValue }
    public var title: String { switch self { case .quarterHour: return "15 minutes"; case .hour: return "1 hour"; case .day: return "24 hours" } }
    public var seconds: TimeInterval { switch self { case .quarterHour: return 900; case .hour: return 3600; case .day: return 86400 } }
    public func interval(endingAt date: Date) -> DateInterval { DateInterval(start: date.addingTimeInterval(-seconds), end: date) }
}

public struct ProofObservation: Identifiable, Sendable {
    public let date: Date
    public let ips: Double
    public let id: String
    public let sequence: Int
    public let boardID: String?
    public init(date: Date, ips: Double, id: String? = nil, sequence: Int = 0, boardID: String? = nil) { self.date = date; self.ips = ips; self.id = id ?? "\(date.timeIntervalSince1970)-\(ips)"; self.sequence = sequence; self.boardID = boardID }
}

public enum HistoryEventKind: String, Sendable {
    case recovery, pause
    case profileChange = "profile_change"
    case telemetryGap = "telemetry_gap"
    case nodeUnavailable = "node_unavailable"
    case boardMissing = "board_missing"
    case boardFault = "board_fault"
    case thermalStop = "thermal_stop"
    public var title: String {
        switch self {
        case .recovery: return "Engine recovered"
        case .profileChange: return "Profile changed"
        case .telemetryGap: return "Telemetry unavailable"
        case .nodeUnavailable: return "Node unreachable"
        case .boardMissing: return "Board disconnected"
        case .boardFault: return "Board needs attention"
        case .thermalStop: return "Temperature limit reached"
        case .pause: return "Paused"
        }
    }
    public var interruptsWork: Bool { self != .recovery && self != .profileChange }
}

public struct HistoryEvent: Identifiable, Sendable {
    public let date: Date
    public let end: Date?
    public let kind: HistoryEventKind
    public let boardID: String?
    public let id: String
    public init(date: Date, end: Date?, kind: HistoryEventKind, id: String? = nil, boardID: String? = nil) { self.date = date; self.end = end; self.kind = kind; self.id = id ?? "\(kind.rawValue)-\(date.timeIntervalSince1970)"; self.boardID = boardID }
    public var title: String { kind.title }
}

public struct PlotPoint: Identifiable, Sendable {
    public let date: Date
    public let ips: Double
    public let segment: Int
    public let id: String
}

public struct HistoryStatistics: Sendable {
    public let average: Double?
    public let minimum: Double?
    public let maximum: Double?
    public let observations: Int
    public let coverage: Double
}

public struct ChainHistory: Identifiable, Sendable {
    public let chain: Chain
    public let samples: [ProofObservation]
    /// Intervals during which capture is known to be available. A node outage
    /// can have complete telemetry coverage while producing no proofs.
    public let captureIntervals: [DateInterval]
    public let events: [HistoryEvent]
    public var id: Chain { chain }

    public static func empty(_ chain: Chain) -> Self {
        Self(chain: chain, samples: [], captureIntervals: [], events: [])
    }

    public func observations(in window: DateInterval) -> [ProofObservation] {
        samples.filter { $0.date >= window.start && $0.date <= window.end && $0.ips.isFinite && $0.ips >= 0 }
            .sorted { $0.date == $1.date ? $0.sequence < $1.sequence : $0.date < $1.date }
    }

    /// Match the colo Prometheus scrape cadence. Each tick samples the latest
    /// validated output; independent chains replace the gauge, never sum.
    public static let scrapeSeconds: TimeInterval = 30
    public static let staleSeconds: TimeInterval = 90

    private func value(at date: Date, source: ProofObservation?, coverage: [DateInterval]) -> Double? {
        guard let source, (0..<Self.staleSeconds).contains(date.timeIntervalSince(source.date)),
              coverage.contains(where: { $0.start <= source.date && $0.end >= date }),
              !events.contains(where: { $0.kind.interruptsWork && ($0.boardID == nil || source.boardID == nil || $0.boardID == source.boardID) && $0.date <= date && ($0.end ?? date) >= source.date }) else { return nil }
        return source.ips
    }

    public func hasFreshValue(at date: Date) -> Bool {
        let window = DateInterval(start: date.addingTimeInterval(-Self.staleSeconds), end: date)
        guard let source = observations(in: window).last,
              (0..<Self.staleSeconds).contains(date.timeIntervalSince(source.date)),
              let capturedThrough = captureIntervals.map(\.end).max(),
              date.timeIntervalSince(capturedThrough) < Self.scrapeSeconds else { return false }
        // History refreshes every 15 seconds. Fresh live status gates the headline;
        // do not treat the normal delay between history fetches as an outage.
        return value(at: min(date, capturedThrough), source: source, coverage: mergedCaptureIntervals(in: window)) != nil
    }

    public func sampledObservations(in window: DateInterval) -> [ProofObservation] {
        let sources = observations(in: DateInterval(start: window.start.addingTimeInterval(-Self.staleSeconds), end: window.end))
        let coverage = mergedCaptureIntervals(in: DateInterval(start: window.start.addingTimeInterval(-Self.staleSeconds), end: window.end))
        var index = 0
        var source: ProofObservation?
        var tick = (floor(window.start.timeIntervalSince1970 / Self.scrapeSeconds) + 1) * Self.scrapeSeconds
        var result: [ProofObservation] = []
        while tick <= window.end.timeIntervalSince1970 {
            let date = Date(timeIntervalSince1970: tick)
            while index < sources.count && sources[index].date <= date {
                source = sources[index]; index += 1
            }
            if let ips = value(at: date, source: source, coverage: coverage) {
                result.append(ProofObservation(date: date, ips: ips, id: "scrape-\(tick)", boardID: source?.boardID))
            }
            tick += Self.scrapeSeconds
        }
        return result
    }

    public func statistics(in window: DateInterval) -> HistoryStatistics {
        let values = sampledObservations(in: window).map(\.ips)
        // Do not remove observations around recoveries or configuration changes.
        let mean = values.isEmpty ? nil : values.reduce(0, +) / Double(values.count)
        let seconds = mergedCaptureIntervals(in: window).reduce(0) { $0 + $1.duration }
        let coverage = window.duration > 0 ? min(1, seconds / window.duration) : 0
        return HistoryStatistics(average: mean, minimum: values.min(), maximum: values.max(), observations: values.count, coverage: coverage)
    }

    private func mergedCaptureIntervals(in window: DateInterval) -> [DateInterval] {
        let intervals = captureIntervals.compactMap { interval -> DateInterval? in
            let start = max(interval.start, window.start), end = min(interval.end, window.end)
            return end > start ? DateInterval(start: start, end: end) : nil
        }.sorted { $0.start < $1.start }
        var merged: [DateInterval] = []
        for interval in intervals {
            if let previous = merged.last, previous.end >= interval.start {
                merged[merged.count - 1] = DateInterval(start: previous.start, end: max(previous.end, interval.end))
            } else { merged.append(interval) }
        }
        return merged
    }

    public func plotPoints(in window: DateInterval) -> [PlotPoint] {
        let points = sampledObservations(in: window)
        let coverage = mergedCaptureIntervals(in: window)
        var previous: Date?
        var segment = 0
        return points.map { sample in
            if let previous {
                let capturedContinuously = coverage.contains { $0.start <= previous && $0.end >= sample.date }
                let interrupted = events.contains {
                    $0.kind.interruptsWork && ($0.boardID == nil || sample.boardID == nil || $0.boardID == sample.boardID) && $0.date < sample.date && ($0.end ?? $0.date) > previous
                }
                if !capturedContinuously || interrupted || sample.date.timeIntervalSince(previous) > Self.scrapeSeconds { segment += 1 }
            }
            previous = sample.date
            return PlotPoint(date: sample.date, ips: sample.ips, segment: segment, id: sample.id)
        }
    }

    public func observation(near date: Date, in window: DateInterval) -> ProofObservation? {
        guard window.contains(date), mergedCaptureIntervals(in: window).contains(where: { $0.contains(date) }) else { return nil }
        guard let sample = sampledObservations(in: window).filter({ abs($0.date.timeIntervalSince(date)) <= Self.scrapeSeconds / 2 }).min(by: { abs($0.date.timeIntervalSince(date)) < abs($1.date.timeIntervalSince(date)) }) else { return nil }
        guard !events.contains(where: { $0.kind.interruptsWork && ($0.boardID == nil || sample.boardID == nil || $0.boardID == sample.boardID) && $0.date <= date && ($0.end ?? $0.date) > date }) else { return nil }
        return sample
    }

    public func events(in window: DateInterval) -> [HistoryEvent] {
        events.filter { $0.date <= window.end && ($0.end ?? $0.date) >= window.start }.sorted { $0.date > $1.date }
    }
}

public enum DisplayFormat {
    public static func ips(_ value: Double?) -> String {
        guard let value, value.isFinite, value >= 0 else { return "—" }
        if value >= 1_000_000 { return String(format: "%.3fM", value / 1_000_000) }
        return "\(Int((value / 1000).rounded()))K"
    }
    public static func uptime(_ seconds: TimeInterval?) -> String {
        guard let seconds else { return "—" }
        let minutes = max(0, Int(seconds) / 60)
        if minutes >= 1440 { return "\(minutes / 1440)d \(minutes % 1440 / 60)h" }
        if minutes >= 60 { return "\(minutes / 60)h \(minutes % 60)m" }
        return "\(minutes)m"
    }
}
