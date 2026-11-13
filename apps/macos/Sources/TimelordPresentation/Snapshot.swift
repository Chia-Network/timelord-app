import Foundation

public enum StatusTone: Sendable { case neutral, positive, attention, critical }

public enum ServiceControlOperation: Equatable, Sendable {
    case start, pause, reload
    public var title: String {
        switch self {
        case .start: return "Starting…"
        case .pause: return "Pausing…"
        case .reload: return "Applying settings…"
        }
    }
}

public enum AppPhase: String, Sendable {
    case setupRequired, starting, restarting, pausing, waitingForNode, waitingForProofs, running, paused, nodeUnreachable, boardMissing, temperatureLimit, serviceUnavailable
    public var title: String {
        switch self {
        case .setupRequired: return "Ready to set up"
        case .starting: return "Starting"
        case .restarting: return "Restarting"
        case .pausing: return "Pausing"
        case .waitingForNode: return "Waiting for the node"
        case .waitingForProofs: return "Waiting for proofs"
        case .running: return "Running"
        case .paused: return "Paused"
        case .nodeUnreachable: return "Node unreachable"
        case .boardMissing, .temperatureLimit: return "Needs attention"
        case .serviceUnavailable: return "Service unavailable"
        }
    }
    public var tone: StatusTone {
        switch self { case .running: return .positive; case .boardMissing, .nodeUnreachable: return .attention; case .temperatureLimit, .serviceUnavailable: return .critical; default: return .neutral }
    }
}

public enum BoardState: String, Sendable {
    case uninspected, checking, recovering, needsAttention, active, waiting, disconnected, thermalStop, paused
    public var title: String {
        switch self { case .uninspected: return "Hardware not inspected"; case .checking: return "Checking hardware"; case .recovering: return "Recovering"; case .needsAttention: return "Needs attention"; case .active: return "Producing proofs"; case .waiting: return "Waiting for work"; case .disconnected: return "Disconnected"; case .thermalStop: return "Stopped at temperature limit"; case .paused: return "Paused" }
    }
    public var tone: StatusTone {
        switch self { case .active: return .positive; case .disconnected, .needsAttention, .recovering: return .attention; case .thermalStop: return .critical; default: return .neutral }
    }
}

public struct ConnectionSnapshot: Sendable {
    public let title: String
    public let detail: String
    public let tone: StatusTone
    public var height: UInt64? = nil
}

public struct BoardSnapshot: Identifiable, Sendable {
    public let id: String
    public let name: String
    public let state: BoardState
    public let temperatureC: Double?
    public let temperatureLimitC: Double?
    public let profile: String
    public let frequencyMHz: Double?
    public let voltageV: Double?
    public let histories: [ChainHistory]
    public let lastProof: Date?
    public var rawSpeeds: [RawSpeed] = []
    public var automatic: AutomaticStatus? = nil
    public var recoveryReason: String? = nil
    public var retryAfterUnix: Double? = nil
    public var assignedEngines: [Int]? = nil
    public func recoveryText(at now: Date) -> String? {
        guard let recoveryReason else { return nil }
        if let retryAfterUnix, retryAfterUnix > now.timeIntervalSince1970 {
            return "\(recoveryReason) · Retrying in \(Int(ceil(retryAfterUnix - now.timeIntervalSince1970)))s"
        }
        return "\(recoveryReason) · Retrying automatically"
    }
    public func rawSpeed(for chain: Chain) -> RawSpeed? {
        guard state == .active || state == .waiting,
              let index = histories.firstIndex(where: { $0.chain == chain }) else { return nil }
        let physicalEngine: Int
        if let assignedEngines {
            guard assignedEngines.indices.contains(index) else { return nil }
            physicalEngine = assignedEngines[index]
        } else {
            physicalEngine = histories.count == 1 ? 1 : index
        }
        return rawSpeeds.first { $0.engine == physicalEngine }
    }
    public var isAutomatic: Bool { profile.lowercased() == "automatic" }
    public var automaticTitle: String {
        guard let automatic else { return "Learning starts with Timelord" }
        switch automatic.state { case "paused": return "Settings held"; case "testing": return "Testing settings"; case "monitoring": return "Monitoring"; case "cooldown": return "Recovery cooldown"; case "waiting": return "Waiting for work"; default: return "Learning" }
    }
    public var temperatureText: String { temperatureC.map { String(format: "%.1f°C", $0) } ?? "—" }
}

public struct AppSnapshot: Sendable {
    public let phase: AppPhase
    public let explanation: String
    public let node: ConnectionSnapshot
    public let timelord: ConnectionSnapshot
    public let boards: [BoardSnapshot]
    public let proofsToday: Int?
    public let uptimeSeconds: TimeInterval?
    public let now: Date
    public let serviceAvailable: Bool
    public var tuningPaused: Bool = false
    public var automaticActive: Bool { boards.contains { $0.isAutomatic } }
    public var readyToRun = false
    public var sessionUptimeSeconds: TimeInterval? = nil
    public var runningTitle: String { phase == .serviceUnavailable && serviceAvailable ? "Proof service stopped" : readyToRun ? "Ready to run" : phase == .running && !tuningPaused && boards.contains(where: { ["learning","testing"].contains($0.automatic?.state ?? "") }) ? "Running · Optimizing" : phase.title }

    public let headlineHistory: ChainHistory?
    public init(phase: AppPhase, explanation: String, node: ConnectionSnapshot, timelord: ConnectionSnapshot,
                boards: [BoardSnapshot], proofsToday: Int?, uptimeSeconds: TimeInterval?, now: Date,
                serviceAvailable: Bool, tuningPaused: Bool = false, readyToRun: Bool = false,
                sessionUptimeSeconds: TimeInterval? = nil, headlineHistory: ChainHistory? = nil) {
        self.phase = phase; self.explanation = explanation; self.node = node; self.timelord = timelord
        self.boards = boards; self.proofsToday = proofsToday; self.uptimeSeconds = uptimeSeconds
        self.now = now; self.serviceAvailable = serviceAvailable; self.tuningPaused = tuningPaused
        self.readyToRun = readyToRun; self.sessionUptimeSeconds = sessionUptimeSeconds
        self.headlineHistory = headlineHistory ?? HistorySelectionCache.merge(boards)
    }
    public var headlineSpeed: String {
        guard phase == .running, let history = headlineHistory,
              history.hasFreshValue(at: now) else { return "—" }
        return DisplayFormat.ips(history.statistics(in: HistoryRange.hour.interval(endingAt: now)).average)
    }
    /// Historical five-minute statistics remain valid between proofs or during idle.
    /// The sampled inputs still exclude stale gauges and interruptions.
    /// Board tiles respond to tuning changes while smoothing individual proof variation.
    /// Use the same 30-second gauge samples as the headline, over a shorter window.
    public func recentSpeed(for history: ChainHistory) -> String {
        guard serviceAvailable, phase != .paused else { return "—" }
        return DisplayFormat.ips(history.statistics(in: DateInterval(start: now.addingTimeInterval(-300), end: now)).average)
    }
    public func recentMedian(for history: ChainHistory) -> String {
        guard serviceAvailable, phase != .paused else { return "—" }
        let values = history.sampledObservations(in: DateInterval(start: now.addingTimeInterval(-300), end: now)).map(\.ips).sorted()
        guard !values.isEmpty else { return "—" }
        return DisplayFormat.ips((values[(values.count - 1) / 2] + values[values.count / 2]) / 2)
    }
    public var latestProof: Date? { boards.compactMap(\.lastProof).max() }
    public var proofFreshness: String {
        guard let latestProof else { return "Waiting for the first verified proof" }
        let seconds = max(0, Int(now.timeIntervalSince(latestProof)))
        if seconds < 60 { return "Latest proof \(seconds)s ago" }
        return "Latest proof \(seconds / 60)m ago"
    }
    public var chartUpperBound: Double {
        let values = boards.flatMap(\.histories).flatMap { $0.observations(in: HistoryRange.day.interval(endingAt: now)) }.map(\.ips)
        // Leave visible space above peaks and avoid small scale changes between refreshes.
        return ceil(max(1_000_000, values.max() ?? 0) * 1.2 / 100_000) * 100_000
    }
}

public struct ServiceStatus: Decodable, Sendable {
    public let apiVersion: Int
    public let servicePid: Int
    public let timelordState: String
    public let message: String
    public let productionEnabled: Bool
    public let fullNode: NodeStatus
    public let boards: [BoardStatus]
    public let timestampUnix: Double
    public let startedUnix: Double
    public let serviceStartedUnix: Double?
    public let latestProofUnix: Double?
    public let proofsDelivered: Int
    public let tuningPaused: Bool
}

public struct NodeSyncStatus: Decodable, Sendable {
    public let synced: Bool
    public let syncing: Bool
    public let height: UInt64?
    public let timestampUnix: Double
}

public struct NodeStatus: Decodable, Sendable {
    public let host: String
    public let state: String
}

public struct RawSpeed: Decodable, Sendable {
    public let engine: Int
    public let ips: Double
    public let timestampUnix: Double
    public let state: String?
    public func freshIPS(at now: Date) -> Double? {
        guard state == nil || state == "running" else { return nil }
        return recentIPS(at: now)
    }
    private func recentIPS(at now: Date) -> Double? {
        guard ips.isFinite, ips >= 0, (0..<6).contains(now.timeIntervalSince1970 - timestampUnix) else { return nil }
        return ips
    }
    public func displayText(at now: Date) -> String {
        switch state {
        case "idle": return "Idle"
        case "sampling": return recentIPS(at: now).map(DisplayFormat.ips) ?? "Starting"
        default: return freshIPS(at: now).map(DisplayFormat.ips) ?? "—"
        }
    }
}

public struct BoardStatus: Decodable, Identifiable, Sendable {
    public let id: String
    public let name: String
    public let assignedChains: [String]
    public let assignedEngines: [Int]?
    public let connectedClients: Int
    public let hardwareState: String
    public let temperatureC: Double?
    public let temperatureLimitC: Double?
    public let frequencyMhz: Double?
    public let voltageV: Double?
    public let latestProofUnix: Double?
    public let profile: String
    public let deviceLocation: Int?
    public let serial: String?
    public let automatic: AutomaticStatus?
    public let rawSpeeds: [RawSpeed]?
    public let faultKind: String?
    public let recoveryReason: String?
    public let retryAfterUnix: Double?
    public let nativePid: Int?
}

public struct AutomaticStatus: Decodable, Sendable {
    public let state: String
    public let message: String
    public let activeSeconds: Int
    public let cooldownSeconds: Int
}

public enum LivePresentation {
    public static func snapshot(status: ServiceStatus?, history: HistoryProjection? = nil, configuration: AppConfiguration? = nil, detectedDevices: [DiscoveredDevice]? = nil, nodeSync: NodeSyncStatus? = nil, paused: Bool = false, workerPID: Int? = nil, controlOperation: ServiceControlOperation? = nil, error: String? = nil, now: Date = Date()) -> AppSnapshot {
        var cache = HistorySelectionCache()
        return snapshot(status: status, history: history, historyCache: &cache, configuration: configuration,
            detectedDevices: detectedDevices, nodeSync: nodeSync, paused: paused, workerPID: workerPID, controlOperation: controlOperation, error: error, now: now)
    }
    public static func snapshot(status: ServiceStatus?, history: HistoryProjection? = nil, historyCache: inout HistorySelectionCache, configuration: AppConfiguration? = nil, detectedDevices: [DiscoveredDevice]? = nil, nodeSync: NodeSyncStatus? = nil, paused: Bool = false, workerPID: Int? = nil, controlOperation: ServiceControlOperation? = nil, error: String? = nil, now: Date = Date()) -> AppSnapshot {
        let recent = status.map { now.timeIntervalSince1970 - $0.timestampUnix <= 10 && $0.timestampUnix <= now.timeIntervalSince1970 + 5 } ?? false
        let live = recent ? status : nil
        let hostname = live?.fullNode.host ?? configuration?.node.host
        let nodeTitle = hostname.map { ["localhost", "127.0.0.1", "::1"].contains($0) ? "This Mac" : $0 } ?? "Not configured"
        let freshSync = nodeSync.flatMap { (0...30).contains(now.timeIntervalSince1970 - $0.timestampUnix) ? $0 : nil }
        let nodeDetail = freshSync.map { $0.syncing ? "Syncing" : $0.synced ? "Synced" : "Not synced" } ?? "Sync status unavailable"
        let nodeTone: StatusTone = freshSync.map { $0.synced && !$0.syncing ? .positive : .attention } ?? .neutral
        let nodeReady = ["authenticated", "ready"].contains(live?.fullNode.state ?? "")
        let productive = live?.boards.contains(where: { $0.hardwareState == "connected" }) == true && live?.productionEnabled == true && live?.timelordState == "running" && live?.latestProofUnix.map { now.timeIntervalSince1970 - $0 < 90 } == true
        let thermal = live?.boards.contains { $0.hardwareState == "thermal_stop" } == true
        let allBoardsFailed = live.map { !$0.boards.isEmpty && $0.boards.allSatisfy { ["fault", "thermal_stop"].contains($0.hardwareState) && $0.recoveryReason == nil } } ?? false
        let admissionBlocked = !productive && live?.timelordState == "waiting_for_clients" && live?.boards.contains { $0.hardwareState == "quarantined" } == true
        let checkingHardware = live?.timelordState == "waiting_for_clients" && live?.boards.contains { $0.hardwareState == "checking" } == true
        let phase: AppPhase
        if controlOperation == .pause { phase = .pausing }
        else if controlOperation == .start { phase = .starting }
        else if controlOperation == .reload && !paused { phase = .restarting }
        else if thermal && !productive { phase = .temperatureLimit }
        else if error != nil || live?.timelordState == "fault" || allBoardsFailed { phase = .serviceUnavailable }
        else if paused { phase = .paused }
        else if productive { phase = .running }
        else if live?.boards.contains(where: { $0.recoveryReason != nil }) == true { phase = .starting }
        else if admissionBlocked { phase = .boardMissing }
        else if checkingHardware { phase = .starting }
        else if live?.timelordState == "waiting_for_clients" { phase = .boardMissing }
        else if live?.fullNode.state == "disconnected" { phase = .nodeUnreachable }
        else if live?.productionEnabled == true { phase = nodeReady ? .waitingForProofs : .waitingForNode }
        else { phase = .setupRequired }
        let proofCapable = live?.productionEnabled == true
        let validHistory = history.flatMap { proofCapable && $0.servicePid == live?.servicePid && $0.apiVersion == 3 ? $0 : nil }
        var boards: [BoardSnapshot]
        if let live {
            boards = live.boards.filter { board in
                // Fresh supervised owners are stronger presence evidence than
                // enumeration, which can hide metadata for open USB interfaces.
                // Checking and quarantined are supervised lifecycle reports,
                // not claims of detected hardware or productive work.
                if ["checking", "quarantined"].contains(board.hardwareState) || board.recoveryReason != nil { return true }
                if board.hardwareState == "connected" || (board.hardwareState == "starting" && board.connectedClients > 0) { return true }
                guard let detectedDevices else {return true}
                let configured=configuration?.hardware.boards.first(where:{$0.id==board.id})
                let location=board.deviceLocation ?? configured?.deviceLocation
                let serial=board.serial ?? configured?.serial
                return detectedDevices.contains { device in if let serial {return device.serial==serial};return device.location==location }
            }.map { board in
                let last = proofCapable ? board.latestProofUnix.map(Date.init(timeIntervalSince1970:)) : nil
                let freshProof = last.map { now.timeIntervalSince($0) < 90 } ?? false
                let state: BoardState = board.recoveryReason != nil && board.hardwareState != "thermal_stop" ? .recovering : board.hardwareState == "quarantined" ? .needsAttention : board.hardwareState == "checking" ? .checking : board.hardwareState == "thermal_stop" ? .thermalStop : ["fault", "reconnecting"].contains(board.hardwareState) ? .disconnected : productive && freshProof && board.hardwareState == "connected" ? .active : board.hardwareState == "connected" ? .waiting : .uninspected
                let histories = board.assignedChains.compactMap(Chain.init(rawValue:)).map { chain -> ChainHistory in
                    guard let validHistory else { return .empty(chain) }
                    return validHistory.history(boardID: board.id, chain: chain, now: now)
                }
                return BoardSnapshot(id: board.id, name: board.name, state: state, temperatureC: board.temperatureC, temperatureLimitC: board.temperatureLimitC, profile: board.profile, frequencyMHz: board.frequencyMhz, voltageV: board.voltageV, histories: histories, lastProof: last, rawSpeeds: board.rawSpeeds ?? [], automatic: board.automatic, recoveryReason: board.recoveryReason, retryAfterUnix: board.retryAfterUnix, assignedEngines: board.assignedEngines)
            }
        } else {
            let connected = (detectedDevices ?? []).compactMap { BoardTopology.matching($0,in:configuration?.hardware.boards ?? []) }
            let counts=BoardTopology.counts(connected)
            boards = connected.enumerated().map { index, board in
                let offset=counts.prefix(index).reduce(0,+)
                let chains: [Chain]
                if let mapping = configuration?.hardware.chainAssignments, let serial = board.serial {
                    let serials = [mapping.challenge, mapping.reward, mapping.infusedChallenge]
                    chains = zip(Chain.allCases, serials).compactMap { chain, assigned in assigned == serial ? chain : nil }
                } else {
                    chains = Array(Chain.allCases.dropFirst(offset).prefix(counts[index]))
                }
                return BoardSnapshot(id: board.id, name: board.name, state: paused ? .paused : .waiting, temperatureC: nil, temperatureLimitC: board.temperatureLimitC, profile: board.profileName, frequencyMHz: nil, voltageV: nil, histories: chains.map(ChainHistory.empty), lastProof: nil)
            }
        }
        if live != nil {
            for device in detectedDevices ?? [] {
                guard let board=BoardTopology.matching(device,in:configuration?.hardware.boards ?? []),!boards.contains(where:{$0.id==board.id}) else {continue}
                boards.append(BoardSnapshot(id:board.id,name:board.name,state:.waiting,temperatureC:nil,temperatureLimitC:board.temperatureLimitC,profile:board.profileName,frequencyMHz:nil,voltageV:nil,histories:[],lastProof:nil))
            }
        }
        let faultReason = live.flatMap { status -> String? in
            guard status.timelordState == "fault" || admissionBlocked || checkingHardware || status.timelordState == "waiting_for_clients" else { return nil }
            if status.boards.contains(where: { $0.faultKind == "control_closed" }) {
                return "The ASIC client connection closed unexpectedly. Proof production has stopped. Pause and Run to reconnect. Details: Native control closed before acknowledgement."
            }
            return status.message
        }
        let timelordTitle: String
        if phase == .pausing { timelordTitle = "Stopping" }
        else if phase == .restarting { timelordTitle = "Restarting" }
        else if phase == .serviceUnavailable { timelordTitle = workerPID != nil && live == nil ? "Not responding" : "Stopped" }
        else if productive { timelordTitle = "Running" }
        else if live != nil { timelordTitle = "Ready" }
        else { timelordTitle = workerPID == nil ? "Stopped" : "Starting" }
        // A retrying cleanup failure keeps the stopping phase but needs attention.
        let stoppingWithError = (phase == .pausing || phase == .restarting) && error != nil
        let setupComplete = configuration?.node.hasIdentity == true && configuration?.hardware.clientBinary != nil && !boards.isEmpty
        return AppSnapshot(phase: phase,
            explanation: error ?? explanation(phase: phase, faultReason: faultReason, paused: paused, live: live, setupComplete: setupComplete),
            node: ConnectionSnapshot(title: nodeTitle, detail: nodeDetail, tone: nodeTone, height: freshSync?.height),
            timelord: ConnectionSnapshot(title: timelordTitle, detail: (phase == .pausing || phase == .restarting) ? error ?? "On this Mac" : "On this Mac", tone: phase == .serviceUnavailable ? .critical : productive ? .positive : stoppingWithError ? .attention : .neutral),
            boards: boards, proofsToday: proofCapable ? live?.proofsDelivered : nil, uptimeSeconds: proofCapable ? live.map { max(0, now.timeIntervalSince1970 - ($0.serviceStartedUnix ?? $0.startedUnix)) } : nil, now: now, serviceAvailable: live != nil, tuningPaused: live?.tuningPaused ?? false, readyToRun: phase == .setupRequired && setupComplete, sessionUptimeSeconds: proofCapable ? live.map { max(0, now.timeIntervalSince1970 - $0.startedUnix) } : nil, headlineHistory: historyCache.headline(projection: validHistory, boards: boards, now: now))
    }

    /// A fault outranks every phase message except an in-progress Pause.
    private static func explanation(phase: AppPhase, faultReason: String?, paused: Bool, live: ServiceStatus?, setupComplete: Bool) -> String {
        if phase != .pausing, let faultReason { return faultReason }
        switch phase {
        case .pausing: return "Pausing Timelord; waiting for the worker to stop."
        case .starting: return "Starting Timelord; waiting for the service to respond."
        case .restarting: return "Restarting Timelord; waiting for the previous worker to stop."
        default:
            if paused { return "Timelord is paused. Resume when you’re ready." }
            return live?.message ?? (setupComplete
                ? "Enable the background service in Settings to run Timelord independently of this interface."
                : "Connect a full node and plug in your ASIC boards to get started.")
        }
    }
}
