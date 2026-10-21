import Foundation

public struct AppConfiguration: Codable, Sendable, Equatable {
    public var schemaVersion = 1
    public var node = NodeConfiguration()
    public var service = ServiceConfiguration()
    public var hardware = HardwareConfiguration()
    public init() {}
}
public struct NodeConfiguration: Codable, Sendable, Equatable {
    public var host = "localhost"
    public var port = 8444
    public var network = "mainnet"
    public var serverName: String? = "chia.net"
    public var certificate: String?
    public var privateKey: String?
    public var caCertificate: String?
    public var rpc: NodeRPCConfiguration?
    public init() {}
    public var hasIdentity: Bool { certificate != nil && privateKey != nil && caCertificate != nil }
}
public struct NodeRPCConfiguration: Codable, Sendable, Equatable {
    public var host: String
    public var port: Int
    public var serverName: String
    public var certificate: String
    public var privateKey: String
    public var caCertificate: String
}
public struct ServiceConfiguration: Codable, Sendable, Equatable {
    public var statusListen = "127.0.0.1:9257"
    public var controlSocket: String?
    public var tuningPaused = false
    public var startAutomatically = true
    public init() {}
}
public struct HardwareConfiguration: Codable, Sendable, Equatable {
    public var clientBinary: String?
    public var driverLibrary: String?
    public var automaticState: String?
    public var boards: [ConfiguredBoard] = []
    public var rememberedBoards: [ConfiguredBoard] = []
    public init() {}
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
    public var maxFrequencyMhz = 1600.0
    public var minVoltageV = 0.9
    public var healthySeconds = 120
    public var cooldownSeconds = 900
    public var voltageAfterSeconds = 3600
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
    public static func reconcile(_ devices:[DiscoveredDevice], hardware:HardwareConfiguration) -> HardwareConfiguration {
        var result=hardware
        var remembered=hardware.rememberedBoards
        for board in hardware.boards {
            remembered.removeAll { $0.id==board.id || (board.serial != nil && $0.serial==board.serial) }
            remembered.append(board)
        }
        let candidates=hardware.boards+remembered
        var selected:[ConfiguredBoard]=[]
        for device in devices.prefix(3) {
            let port=(18110...18112).first { p in !selected.contains {$0.clientPort==p} }!
            var board=BoardTopology.matching(device,in:candidates) ?? ConfiguredBoard(id:UUID().uuidString,name:"ASIC \(selected.count+1)",port:port,location:device.location)
            board.deviceLocation=device.location;board.serial=device.serial ?? board.serial
            if selected.contains(where:{$0.clientPort==board.clientPort}) {board.clientPort=port}
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
    public let captureStartUnix: Double
    public let observations: [RecordedProof]
    public let events: [RecordedEvent]
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
