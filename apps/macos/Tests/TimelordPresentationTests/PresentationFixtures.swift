import Foundation
@testable import TimelordPresentation

/// Pure fixtures: no networking, process launching, configuration writes or USB access.
public enum PresentationScenario: String, CaseIterable, Identifiable, Sendable {
    case automatic = "automatic"
    case runningThreeBoards = "three-boards"
    case runningOneBoard = "one-board"
    case waitingForNode = "waiting-for-node"
    case nodeUnreachable = "node-unreachable"
    case boardDisconnected = "board-disconnected"
    case temperatureLimit = "temperature-limit"
    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .automatic: return "Running · Automatic"
        case .runningThreeBoards: return "Running · 3 boards"
        case .runningOneBoard: return "Running · 1 board"
        case .waitingForNode: return "Waiting for the node"
        case .nodeUnreachable: return "Node unreachable"
        case .boardDisconnected: return "Board disconnected"
        case .temperatureLimit: return "Temperature limit"
        }
    }

    public func snapshot(now: Date = Date(), paused: Bool = false) -> AppSnapshot {
        let single = self == .runningOneBoard || self == .automatic
        let start = now.addingTimeInterval(-86400)
        let phase: AppPhase = paused ? .paused : {
            switch self { case .runningThreeBoards, .runningOneBoard, .automatic: return .running; case .waitingForNode: return .waitingForNode; case .nodeUnreachable: return .nodeUnreachable; case .boardDisconnected: return .boardMissing; case .temperatureLimit: return .temperatureLimit }
        }()
        let boards = (0..<(single ? 1 : 3)).map { boardIndex -> BoardSnapshot in
            let missing = self == .boardDisconnected && boardIndex == 2
            let hot = self == .temperatureLimit && boardIndex == 1
            let state: BoardState = paused ? .paused : missing ? .disconnected : hot ? .thermalStop : self == .waitingForNode || self == .nodeUnreachable ? .waiting : .active
            let chains: [Chain] = single ? Chain.allCases : [Chain.allCases[boardIndex]]
            let histories = chains.map { chain -> ChainHistory in
                if self == .waitingForNode { return .empty(chain) }
                let chainIndex = Chain.allCases.firstIndex(of: chain)!
                let recovery = now.addingTimeInterval(-2460)
                let captureGap = DateInterval(start: now.addingTimeInterval(-1800), duration: 120)
                var events = [HistoryEvent(date: recovery, end: nil, kind: .recovery), HistoryEvent(date: captureGap.start, end: captureGap.end, kind: .telemetryGap)]
                let outageKind: HistoryEventKind? = paused ? .pause : missing ? .boardMissing : hot ? .thermalStop : self == .nodeUnreachable ? .nodeUnavailable : nil
                let outage = outageKind.map { _ in DateInterval(start: now.addingTimeInterval(-180), end: now) }
                if let outage, let outageKind { events.append(HistoryEvent(date: outage.start, end: outage.end, kind: outageKind)) }
                let base = (single ? 953_000.0 : 971_000.0) - Double(chainIndex) * 2700
                let samples = (0...1440).compactMap { index -> ProofObservation? in
                    let date = start.addingTimeInterval(Double(index) * 60)
                    if captureGap.contains(date) || (outage?.contains(date) ?? false) { return nil }
                    let wave = sin(Double(index) * 0.47 + Double(chainIndex)) * 11000 + cos(Double(index) * 0.19) * 5000
                    // Retain the observed slow proof near recovery; never turn it into a gap.
                    let penalty = abs(date.timeIntervalSince(recovery)) < 35 ? 350_000.0 : 0
                    return ProofObservation(date: date, ips: base + wave - penalty)
                }
                return ChainHistory(chain: chain, samples: samples,
                    captureIntervals: [DateInterval(start: start, end: captureGap.start), DateInterval(start: captureGap.end, end: now)], events: events)
            }
            return BoardSnapshot(id: "fixture-\(boardIndex)", name: "ASIC \(["A", "B", "C"][boardIndex])", state: state,
                temperatureC: missing ? nil : hot ? 55 : single ? 48.2 : 42.5 + Double(boardIndex) * 1.2,
                temperatureLimitC: 55, profile: self == .automatic ? "Automatic" : "Standard", frequencyMHz: self == .automatic ? 1450 : single ? 1540 : 1620, voltageV: 1,
                histories: histories, lastProof: histories.flatMap(\.samples).map(\.date).max(), automatic: self == .automatic ? AutomaticStatus(state:"learning",message:"Observing verified proofs before a frequency increase",activeSeconds:90,cooldownSeconds:0) : nil)
        }
        let explanation: String = {
            switch phase {
            case .running: return single ? "Your ASIC is producing proofs for the Chia network." : "All three ASICs are producing proofs for the Chia network."
            case .paused: return "Proof production is paused. Resume when you’re ready."
            case .waitingForNode: return "Your boards are ready. Waiting for the full node to provide work."
            case .nodeUnreachable: return "The connection to your full node was lost. Check that it’s running and reachable."
            case .boardMissing: return "ASIC C is disconnected. Check its USB cable and power."
            case .temperatureLimit: return "ASIC B stopped at its temperature limit. Check airflow before restarting."
            default: return "Setup is incomplete."
            }
        }()
        let nodeReady = self != .waitingForNode && self != .nodeUnreachable
        return AppSnapshot(phase: phase, explanation: explanation,
            node: ConnectionSnapshot(title: "Example mainnet node", detail: nodeReady ? "Synced" : self == .waitingForNode ? "Syncing" : "Sync status unavailable", tone: nodeReady ? .positive : .attention, height: nodeReady ? 9_500_241 : self == .waitingForNode ? 9_499_820 : nil),
            timelord: ConnectionSnapshot(title: paused ? "Paused" : nodeReady ? "Connected" : "Waiting", detail: "Built into this app", tone: paused || !nodeReady ? .neutral : .positive),
            boards: boards, proofsToday: self == .waitingForNode ? 0 : 28_741, uptimeSeconds: self == .waitingForNode ? nil : 14 * 86400 + 6 * 3600,
            now: now, serviceAvailable: false)
    }
}
