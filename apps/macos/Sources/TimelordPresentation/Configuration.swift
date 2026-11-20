import Foundation

public struct AppConfiguration: Codable, Sendable, Equatable {
    public var schemaVersion = 1
    public var node = NodeConfiguration()
    public var connections: ConnectionProfiles?
    public var service = ServiceConfiguration()
    public var hardware = HardwareConfiguration()
    public init() {}
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
/// integers to {"integer": "decimal digits"} so even u128 stays exact.
public indirect enum NetworkValue: Codable, Sendable, Equatable {
    case string(String), number(Decimal), bool(Bool), array([NetworkValue]), object([String: NetworkValue]), null
    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() { self = .null }
        else if let value = try? container.decode(Bool.self) { self = .bool(value) }
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
        case .null: try container.encodeNil()
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

public enum BoardDiscovery {
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
            remembered.removeAll { $0.id==board.id || (board.serial != nil && $0.serial==board.serial) }
            remembered.append(board)
        }
        let candidates=hardware.boards+remembered
        var selected:[ConfiguredBoard]=[]
        for device in devices.prefix(3) {
            let port=(18110...65535).first { p in !reservedPorts.contains(p) && !selected.contains {$0.clientPort==p} }!
            var board=BoardTopology.matching(device,in:candidates) ?? ConfiguredBoard(id:UUID().uuidString,name:"ASIC \(selected.count+1)",port:port,location:device.location)
            board.deviceLocation=device.location;board.serial=device.serial ?? board.serial
            if reservedPorts.contains(board.clientPort) || selected.contains(where:{$0.clientPort==board.clientPort}) {board.clientPort=port}
            selected.append(board)
        }
        result.boards=selected
        result.rememberedBoards=remembered.filter { old in !selected.contains {$0.id==old.id} }
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
}
public struct RecordedEvent: Decodable, Sendable {
    public let id: Int
    public let timestampUnix: Double
    public let boardId: String?
    public let kind: String
    public let endUnix: Double?
}
