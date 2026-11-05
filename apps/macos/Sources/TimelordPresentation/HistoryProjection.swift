import Foundation

/// Immutable presentation records built once when a history response arrives.
/// Status polling reuses these arrays while evaluating live health and freshness.
public struct HistoryProjection: Sendable {
    let identity = UUID()
    public let apiVersion: Int
    public let servicePid: Int
    public let timestampUnix: Double
    private let histories: [String: [Chain: ChainHistory]]
    private let openEvents: Set<String>

    public init(payload: HistoryPayload, boards: [BoardStatus]) {
        apiVersion = payload.apiVersion; servicePid = payload.servicePid; timestampUnix = payload.timestampUnix
        let end = Date(timeIntervalSince1970: payload.timestampUnix)
        let captures = payload.captureIntervals.compactMap { capture -> DateInterval? in
            let start = Date(timeIntervalSince1970: capture.startUnix)
            let finish = min(end, Date(timeIntervalSince1970: capture.endUnix))
            return finish > start ? DateInterval(start: start, end: finish) : nil
        }
        let proofs = Dictionary(grouping: payload.estimatedObservations, by: \.chain)
        let events = payload.events.compactMap { event -> HistoryEvent? in
            guard let kind = HistoryEventKind(rawValue: event.kind) else { return nil }
            return HistoryEvent(date: Date(timeIntervalSince1970: event.timestampUnix),
                end: kind.interruptsWork ? event.endUnix.map(Date.init(timeIntervalSince1970:)) ?? end : nil,
                kind: kind, id: "event-\(event.id)", boardID: event.boardId)
        }
        openEvents = Set(payload.events.filter { $0.endUnix == nil }.map { "event-\($0.id)" })
        let indexed = Array(events.enumerated())
        let globals = indexed.filter { $0.element.boardID == nil }
        let byBoard = Dictionary(grouping: indexed.filter { $0.element.boardID != nil }, by: { $0.element.boardID! })
        histories = Dictionary(boards.map { board in
            let localEvents = (globals + (byBoard[board.id] ?? [])).sorted { $0.offset < $1.offset }.map(\.element)
            let chains = Dictionary( board.assignedChains.compactMap(Chain.init(rawValue:)).map { chain in
                let samples = (proofs[chain] ?? []).map { proof in
                    ProofObservation(date: Date(timeIntervalSince1970: proof.timestampUnix), ips: proof.ips,
                        id: "proof-\(proof.id)", sequence: proof.id, boardID: board.id)
                }
                return (chain, ChainHistory(chain: chain, samples: samples, captureIntervals: captures, events: localEvents))
            }, uniquingKeysWith: { first, _ in first })
            return (board.id, chains)
        }, uniquingKeysWith: { first, _ in first })
    }

    func history(boardID: String, chain: Chain, now: Date) -> ChainHistory {
        guard let cached = histories[boardID]?[chain] else { return .empty(chain) }
        guard now.timeIntervalSince1970 < timestampUnix else { return cached }
        // Preserve clipping when the service clock is briefly ahead of the UI.
        let captures = cached.captureIntervals.compactMap { interval -> DateInterval? in
            let end = min(now, interval.end)
            return end > interval.start ? DateInterval(start: interval.start, end: end) : nil
        }
        let events = cached.events.map { event in
            HistoryEvent(date: event.date, end: event.kind.interruptsWork && openEvents.contains(event.id) ? now : event.end,
                kind: event.kind, id: event.id, boardID: event.boardID)
        }
        return ChainHistory(chain: chain, samples: cached.samples, captureIntervals: captures, events: events)
    }
}

/// Owned by the model; only a payload or board/chain selection change rebuilds
/// the aggregate. Time-dependent chart statistics still run against snapshot.now.
public struct HistorySelectionCache {
    private struct Board: Equatable { let id: String; let chains: [Chain] }
    private struct Key: Equatable { let payload: UUID?; let boards: [Board]; let clippedEnd: Date? }
    private var key: Key?
    private var cached: ChainHistory?
    public init() {}

    mutating func headline(projection: HistoryProjection?, boards: [BoardSnapshot], now: Date) -> ChainHistory? {
        let next = Key(payload: projection?.identity,
            boards: boards.map { Board(id: $0.id, chains: $0.histories.map(\.chain)) },
            clippedEnd: projection.flatMap { now.timeIntervalSince1970 < $0.timestampUnix ? now : nil })
        if key != next { cached = Self.merge(boards); key = next }
        return cached
    }
    static func merge(_ boards: [BoardSnapshot]) -> ChainHistory? {
        let histories = boards.flatMap(\.histories)
        guard !histories.isEmpty else { return nil }
        return ChainHistory(chain: .challenge, samples: histories.flatMap(\.samples),
            captureIntervals: histories.flatMap(\.captureIntervals), events: histories.flatMap(\.events))
    }
}
