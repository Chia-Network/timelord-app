import Foundation

public struct AppConfiguration: Codable, Sendable, Equatable {
    public var schemaVersion = 1
    public var connections: ConnectionProfiles?
    public var service = ServiceConfiguration()
    public var hardware = HardwareConfiguration()
    /// Single-node storage; ignored while `connections.profiles` is non-empty.
    private var singleNode = NodeConfiguration()

    /// The active node is derived from the active profile. Only a single-node
    /// configuration stores it directly, so there is no second mutable owner.
    public var node: NodeConfiguration {
        get {
            guard let connections,
                  let active = connections.profiles.first(where: { $0.id == connections.activeId })
            else { return singleNode }
            return active.node
        }
        set {
            if let index = activeProfileIndex {
                connections?.profiles[index].node = newValue
            } else {
                singleNode = newValue
            }
        }
    }
    /// Resolve incomplete RPC drafts for saving. A draft is UI state, but it
    /// must not erase RPC settings that are already saved: an incomplete draft
    /// falls back to the last saved RPC settings for the same profile. Only an
    /// explicit `rpc = nil` (toggle off) persists no RPC settings.
    public func resolvedForPersistence(against saved: Self) -> Self {
        var resolved = self
        if var profiles = resolved.connections, !profiles.profiles.isEmpty {
            for index in profiles.profiles.indices {
                let id = profiles.profiles[index].id
                let savedRPC = saved.connections?.profiles.first { $0.id == id }?.node.rpc
                _ = profiles.profiles[index].node.resolveIncompleteRPC(using: savedRPC)
            }
            resolved.connections = profiles
        } else {
            // A single-node draft is not the same context as any saved profile,
            // so it must not inherit the removed profile's RPC settings.
            let savedRPC = (saved.connections?.profiles.isEmpty ?? true) ? saved.node.rpc : nil
            _ = resolved.node.resolveIncompleteRPC(using: savedRPC)
        }
        return resolved
    }
    /// Done only discards the draft the user can see. Returns true when the
    /// active profile's incomplete draft was replaced by its saved value.
    public mutating func resolveActiveIncompleteRPC(against saved: Self) -> Bool {
        if let index = activeProfileIndex, var profiles = connections {
            let id = profiles.profiles[index].id
            let savedRPC = saved.connections?.profiles.first { $0.id == id }?.node.rpc
            let dropped = profiles.profiles[index].node.resolveIncompleteRPC(using: savedRPC)
            connections = profiles
            return dropped
        }
        let savedRPC = (saved.connections?.profiles.isEmpty ?? true) ? saved.node.rpc : nil
        return node.resolveIncompleteRPC(using: savedRPC)
    }
    /// `saveConfiguration()` reloads the helper's copy, which has no
    /// incomplete RPC drafts. Put the user's still-editable drafts back into
    /// the in-memory form so implicit saves (connection test, board refresh)
    /// do not clear it. The saved copy stays stripped. A single-node draft is
    /// applied to the active profile when the helper migrates first-time setup.
    public func restoringIncompleteRPCDrafts(from draft: Self) -> Self {
        var restored = self
        if var profiles = restored.connections, !profiles.profiles.isEmpty {
            for index in profiles.profiles.indices {
                let id = profiles.profiles[index].id
                let sourceRPC: NodeRPCConfiguration?
                if let source = draft.connections?.profiles.first(where: { $0.id == id }) {
                    sourceRPC = source.node.rpc
                } else if draft.connections?.profiles.isEmpty ?? true, id == profiles.activeId {
                    sourceRPC = draft.node.rpc
                } else {
                    sourceRPC = nil
                }
                guard let rpc = sourceRPC, !rpc.isComplete else { continue }
                profiles.profiles[index].node.rpc = rpc
            }
            restored.connections = profiles
        } else if let rpc = draft.node.rpc, !rpc.isComplete {
            restored.node.rpc = rpc
        }
        return restored
    }
    private var activeProfileIndex: Int? {
        guard let connections else { return nil }
        return connections.profiles.firstIndex { $0.id == connections.activeId }
    }
    private var profileConnections: ConnectionProfiles? {
        guard let connections, !connections.profiles.isEmpty else { return nil }
        return connections
    }

    public init() {}
    private enum CodingKeys: String, CodingKey { case schemaVersion, node, connections, service, hardware }
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        schemaVersion = try container.decode(Int.self, forKey: .schemaVersion)
        service = try container.decode(ServiceConfiguration.self, forKey: .service)
        hardware = try container.decode(HardwareConfiguration.self, forKey: .hardware)
        connections = try container.decodeIfPresent(ConnectionProfiles.self, forKey: .connections)
        let supplied = try container.decodeIfPresent(NodeConfiguration.self, forKey: .node)
        if profileConnections != nil {
            guard activeProfileIndex != nil else {
                throw DecodingError.dataCorruptedError(forKey: .connections, in: container,
                    debugDescription: "Connection profiles require one existing active profile")
            }
            if let supplied, supplied != node {
                throw DecodingError.dataCorruptedError(forKey: .node, in: container,
                    debugDescription: "[node] is not used when [[connections.profiles]] are configured; edit the active profile instead")
            }
        } else {
            singleNode = supplied ?? NodeConfiguration()
        }
    }
    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(schemaVersion, forKey: .schemaVersion)
        try container.encode(service, forKey: .service)
        try container.encode(hardware, forKey: .hardware)
        if let profileConnections {
            try container.encode(profileConnections, forKey: .connections)
        } else {
            if let connections { try container.encode(connections, forKey: .connections) }
            try container.encode(singleNode, forKey: .node)
        }
    }
    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.schemaVersion == rhs.schemaVersion
            && lhs.service == rhs.service
            && lhs.hardware == rhs.hardware
            && lhs.node == rhs.node
            && lhs.profileConnections == rhs.profileConnections
    }
}
public struct ConnectionProfiles: Codable, Sendable, Equatable {
    public var activeId: String?
    public var profiles: [ConnectionProfile] = []
    public init() {}
}
public struct ConnectionProfile: Codable, Sendable, Equatable, Identifiable {
    public var id: String
    public var name: String
    public var node: NodeConfiguration
    public init(id: String, name: String, node: NodeConfiguration) {
        self.id = id; self.name = name; self.node = node
    }
}
public struct NodeConfiguration: Codable, Sendable, Equatable {
    public var host = "localhost"
    public var port = 8444
    public var network = "mainnet"
    public var networkDefinition: NetworkDefinition?
    public var serverName: String? = "chia.net"
    public var certificate: String?
    public var privateKey: String?
    public var caCertificate: String?
    public var rpc: NodeRPCConfiguration?
    public init() {}
    public var hasIdentity: Bool { certificate != nil && privateKey != nil && caCertificate != nil }
    /// The service accepts RPC only when every endpoint and credential is set.
    /// Returns true when an incomplete draft was replaced by `saved`.
    public mutating func resolveIncompleteRPC(using saved: NodeRPCConfiguration?) -> Bool {
        guard let rpc, !rpc.isComplete else { return false }
        self.rpc = saved
        return true
    }
}
public struct NetworkDefinition: Codable, Sendable, Equatable {
    public var schemaVersion: Int
    public var networkId: String
    public var exportedAt: String
    public var chiaVersion: String
    public var constants: [String: NetworkValue]
    public var configOverrides: [String: NetworkValue]
}

/// Preserve nested overrides through settings edits. The helper normalizes
/// integers to {"integer": "decimal digits"} so even u128 stays exact. TOML
/// cannot represent null, so neither configuration nor definitions accept it.
public indirect enum NetworkValue: Codable, Sendable, Equatable {
    case string(String), number(Decimal), bool(Bool), array([NetworkValue]), object([String: NetworkValue])
    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        guard !container.decodeNil() else {
            throw DecodingError.dataCorruptedError(in: container,
                debugDescription: "Network constant values cannot be null")
        }
        if let value = try? container.decode(Bool.self) { self = .bool(value) }
        else if let value = try? container.decode(String.self) { self = .string(value) }
        else if let value = try? container.decode(Decimal.self) { self = .number(value) }
        else if let value = try? container.decode([NetworkValue].self) { self = .array(value) }
        else { self = .object(try container.decode([String: NetworkValue].self)) }
    }
    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .string(let value): try container.encode(value)
        case .number(let value): try container.encode(value)
        case .bool(let value): try container.encode(value)
        case .array(let value): try container.encode(value)
        case .object(let value): try container.encode(value)
        }
    }
}
public struct NodeRPCConfiguration: Codable, Sendable, Equatable {
    public var host: String
    public var port: Int
    public var serverName: String
    public var certificate: String
    public var privateKey: String
    public var caCertificate: String
    public init(host: String = "localhost", port: Int = 8555, serverName: String = "chia.net",
                certificate: String = "", privateKey: String = "", caCertificate: String = "") {
        self.host = host; self.port = port; self.serverName = serverName
        self.certificate = certificate; self.privateKey = privateKey; self.caCertificate = caCertificate
    }
    /// Mirrors `NodeConfig::validate` in `timelord-core`: a partial RPC profile
    /// would make the whole connection profile unsavable.
    public var isComplete: Bool {
        !host.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && port != 0
            && !serverName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && ![certificate, privateKey, caCertificate].contains(where: \.isEmpty)
    }
}
public struct ServiceConfiguration: Codable, Sendable, Equatable {
    public var statusListen = "127.0.0.1:9257"
    public var observabilityListen: String?
    public var allowRemoteObservability = false
    public var supervisorObservabilityListen = "127.0.0.1:9259"
    public var controlSocket: String?
    public var tuningPaused = false
    public var startAutomatically = true
    public init() {}
}
public struct HardwareConfiguration: Codable, Sendable, Equatable {
    public var clientBinary: String?
    public var driverLibrary: String?
    public var automaticState: String?
    public var chainAssignments: ChainAssignments?
    public var boards: [ConfiguredBoard] = []
    public var rememberedBoards: [ConfiguredBoard] = []
    public init() {}
}
public struct ChainAssignments: Codable, Sendable, Equatable {
    public var challenge: String
    public var reward: String
    public var infusedChallenge: String
    public init(challenge: String, reward: String, infusedChallenge: String) {
        self.challenge = challenge; self.reward = reward; self.infusedChallenge = infusedChallenge
    }
}
public struct ConfiguredBoard: Codable, Identifiable, Sendable, Equatable {
    public var id: String
    public var name: String
    public var clientPort: Int
    public var deviceLocation: Int?
    public var serial: String?
    public var profile: String
    public var automatic: AutomaticConfiguration
    public var frequencyMhz: Double
    public var voltageV: Double
    public var temperatureLimitC: Double
    public init(id: String, name: String, port: Int, location: Int) {
        self.id = id; self.name = name; clientPort = port; deviceLocation = location
        frequencyMhz = 1450; voltageV = 1.0; temperatureLimitC = 55
        profile = "automatic"; automatic = AutomaticConfiguration()
    }
    public var profileName: String { profile == "automatic" ? "Automatic" : "Manual" }

}
public struct AutomaticConfiguration: Codable, Sendable, Equatable {
    public var startFrequencyMhz = 1450.0
    public var startVoltageV = 1.0
    public var minFrequencyMhz = 1000.0
    public var maxFrequencyMhz = 2200.0
    public var minVoltageV = 0.9
    public var healthySeconds = 120
    public var cooldownSeconds = 900
    public var retestAfterSeconds: Int?
    public var recoveryWindowSeconds = 300
    public var recoveryLossPercent = 5.0
    public var recoveryBadWindows = 2
    public var minimumProofsPerChain = 3
    public init() {}
}

public struct DiscoveredDevice: Decodable, Identifiable, Sendable {
    public let name: String
    public let location: Int
    public let serial: String?
    public var id: Int { location }
    public init(name:String,location:Int,serial:String? = nil) { self.name=name;self.location=location;self.serial=serial }
}
public enum BoardTopology {
    public static func counts(_ boards:[ConfiguredBoard]) -> [Int] {
        switch boards.count {
        case 0: return []
        case 1: return [3]
        case 2: return [2,1]
        default: return [1,1,1]
        }
    }
    public static func matching(_ device:DiscoveredDevice,in boards:[ConfiguredBoard])->ConfiguredBoard? {
        boards.first { board in if let serial=board.serial { return serial==device.serial };return board.deviceLocation==device.location }
    }
}

/// Identity the running service publishes for a discovered board.
public struct ServiceBoardIdentity: Sendable {
    public let id: String
    public let name: String
    public let serial: String?
    public let location: Int?
    public init(id: String, name: String, serial: String?, location: Int?) {
        self.id = id; self.name = name; self.serial = serial; self.location = location
    }
}
public enum BoardDiscovery {
    /// The identifier the service assigns when it discovers a board at a USB
    /// location and config has no existing owner for it.
    public static func serviceBoardID(location: Int) -> String { "usb-\(location)" }

    /// Same ASIC, even when a stale location-based identifier was later reused
    /// by a different board at that port. Serial wins; identical IDs only
    /// count when serials do not disagree.
    public static func samePhysicalBoard(_ a: ConfiguredBoard, _ b: ConfiguredBoard) -> Bool {
        if let serial = a.serial, serial == b.serial { return true }
        return a.id == b.id && (a.serial == nil || b.serial == nil || a.serial == b.serial)
    }

    /// Enrich a metadata-hidden USB row rather than creating another device
    /// with the same location ID. Fresh conflicting inventory wins over status.
    public static func includingManagedOwner(_ owner: DiscoveredDevice, in devices: [DiscoveredDevice]) -> [DiscoveredDevice] {
        // A serial observed at a new port outranks the owner's old location,
        // regardless of the order in which inventory rows arrived.
        if let serial = owner.serial, devices.contains(where: { $0.serial == serial }) { return devices }
        var result = devices
        if let index = result.firstIndex(where: { $0.location == owner.location }) {
            let observed = result[index]
            if observed.serial == nil && observed.location == owner.location {
                result[index] = DiscoveredDevice(name: observed.name, location: observed.location, serial: owner.serial)
            }
        } else {
            result.append(owner)
        }
        return result
    }
    public static func reconcile(_ devices:[DiscoveredDevice], hardware:HardwareConfiguration, service:ServiceConfiguration) -> HardwareConfiguration {
        var result=hardware
        let reservedPorts=Set([service.statusListen,service.observabilityListen,service.supervisorObservabilityListen].compactMap { address in
            address.flatMap { Int($0.split(separator:":").last.map(String.init) ?? "") }
        })
        if hardware.chainAssignments != nil {
            // Keep serial-pinned owners even while unplugged. The service retries
            // their connections without reallocating a healthy board's chain.
            for device in devices {
                if let index = result.boards.firstIndex(where: { $0.serial != nil && $0.serial == device.serial }) {
                    result.boards[index].deviceLocation = device.location
                }
            }
            return result
        }
        var remembered=hardware.rememberedBoards
        for board in hardware.boards {
            remembered.removeAll { samePhysicalBoard($0, board) }
            remembered.append(board)
        }
        let candidates=hardware.boards+remembered
        var selected:[ConfiguredBoard]=[]
        for device in devices.prefix(3) {
            let port=(18110...65535).first { p in !reservedPorts.contains(p) && !selected.contains {$0.clientPort==p} }!
            let serviceID=serviceBoardID(location:device.location)
            var board=BoardTopology.matching(device,in:candidates) ?? ConfiguredBoard(id:serviceID,name:"ASIC \(selected.count+1)",port:port,location:device.location)
            board.deviceLocation=device.location;board.serial=device.serial ?? board.serial
            // Location-based service IDs must follow the current port. A board
            // that kept usb-{old port} after a move would collide with a new
            // board minted at that port.
            if board.id != serviceID, board.id.hasPrefix("usb-"), !selected.contains(where: { $0.id == serviceID }) {
                board.id = serviceID
            }
            if reservedPorts.contains(board.clientPort) || selected.contains(where:{$0.clientPort==board.clientPort}) {board.clientPort=port}
            selected.append(board)
        }
        result.boards=selected
        result.rememberedBoards=remembered.filter { old in !selected.contains { samePhysicalBoard($0, old) } }
        return result
    }

    /// Prefer the running service's board IDs and names when the same physical
    /// board is already present in configuration under a different identifier.
    public static func adoptingServiceIdentities(_ hardware: HardwareConfiguration, identities: [ServiceBoardIdentity]) -> HardwareConfiguration {
        var result = hardware
        func adopt(_ board: inout ConfiguredBoard) {
            guard let match = identities.first(where: { identity in
                if let serial = identity.serial, let existing = board.serial {
                    return serial == existing
                }
                if let location = identity.location, board.deviceLocation == location { return true }
                return false
            }) else { return }
            board.id = match.id
            if board.name.hasPrefix("ASIC ") { board.name = match.name }
        }
        for index in result.boards.indices { adopt(&result.boards[index]) }
        for index in result.rememberedBoards.indices { adopt(&result.rememberedBoards[index]) }
        return result
    }
}
public struct HistoryPayload: Decodable, Sendable {
    public let apiVersion: Int
    public let servicePid: Int
    public let timestampUnix: Double
    public let captureIntervals: [RecordedCapture]
    public let estimatedObservations: [RecordedProof]
    public let events: [RecordedEvent]
}
public struct RecordedCapture: Decodable, Sendable {
    public let startUnix: Double
    public let endUnix: Double
}
public struct RecordedProof: Decodable, Sendable {
    public let id: Int
    public let chain: Chain
    public let timestampUnix: Double
    public let ips: Double
    /// The board that produced the proof; absent in older checkpoints.
    public var boardId: String? = nil
}
public struct RecordedEvent: Decodable, Sendable {
    public let id: Int
    public let timestampUnix: Double
    public let boardId: String?
    public let kind: String
    public let endUnix: Double?
}
