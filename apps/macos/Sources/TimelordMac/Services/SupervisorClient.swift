import Foundation

/// The UI sends intent to the supervisor; it never substitutes process presence
/// for proof readiness. The CLI enforces peer ownership and bounded transactions.
actor SupervisorClient {
    enum Command: String { case status, pause, resume, reload }
    private let executor: ConfigurationClient
    private let config: URL
    init(helper: URL, config: URL) {
        self.config = config
        executor = ConfigurationClient(helper: helper, config: config)
    }
    func send(_ command: Command, expectedRevision: UInt64? = nil) async throws -> SupervisorStatus {
        var arguments = [command.rawValue, "--config", config.path]
        if let expectedRevision {
            guard command != .status else { throw ConfigurationError.message("Status does not change operating intent.") }
            arguments += ["--expected-revision", String(expectedRevision)]
        }
        let data = try await executor.run(arguments)
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let status = try decoder.decode(SupervisorStatus.self, from: data)
        guard status.apiVersion == 1, status.supervisorPid > 1 else {
            throw ConfigurationError.message("The app and background supervisor versions do not match.")
        }
        return status
    }
}

/// Mirrors the supervisor's versioned snake_case wire phase contract.
enum SupervisorPhase: String, Codable, CaseIterable, Sendable {
    case paused, starting, stopping, recovering
    case stateError = "state_error"
    case workerStarted = "worker_started"
}

struct SupervisorStatus: Decodable, Sendable {
    enum Mode: String, Decodable, Sendable { case running, paused }
    let apiVersion: Int
    let supervisorPid: Int32
    let workerPid: Int32?
    let mode: Mode
    let revision: UInt64
    let workerGeneration: UInt64
    let restarts: UInt64
    let phase: SupervisorPhase
    let error: String?
}
