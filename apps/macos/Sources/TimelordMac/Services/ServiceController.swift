import Foundation

/// Controls an independent supervisor. The interface never owns or
/// terminates the worker, and disappearing from the desktop changes no intent.
@MainActor
final class ServiceController {
    let configURL: URL
    let logsURL: URL
    let endpoint: URL
    let helperURL: URL
    let supervisorURL: URL
    private let appURL: URL
    var onChange: (() -> Void)?
    private(set) var errorMessage: String?
    private(set) var supervisor: SupervisorStatus?
    private var hasObservedSupervisor = false
    private var changingIntent = false
    private var controlEpoch: UInt64 = 0
    // Before the first successful contact, an absent background service is setup.
    // Once observed, losing contact must remain visible as a runtime failure.
    var presentationError: String? { hasObservedSupervisor ? errorMessage : nil }
    var canControlService: Bool { supervisor != nil }
    var desiredRunning: Bool { supervisor?.mode == .running }
    var processID: Int32? { supervisor?.workerPid }
    private var client: SupervisorClient { SupervisorClient(helper: supervisorURL, config: configURL) }

    init(configURL: URL? = nil, logsURL: URL? = nil, helperURL: URL? = nil, endpoint: URL? = nil, supervisorURL: URL? = nil, appURL: URL? = nil) {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Timelord", isDirectory: true)
        let environment = ProcessInfo.processInfo.environment
        self.appURL = appURL ?? Bundle.main.bundleURL
        self.helperURL = helperURL ?? environment["TIMELORD_SERVICE_BINARY"].map { URL(fileURLWithPath: $0) } ?? Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/timelord-service")
        self.supervisorURL = supervisorURL ?? environment["TIMELORD_SUPERVISOR_BINARY"].map { URL(fileURLWithPath: $0) } ?? Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/timelord-supervisor")
        self.configURL = configURL ?? environment["TIMELORD_CONFIG"].map { URL(fileURLWithPath: $0) } ?? base.appendingPathComponent("config.toml")
        self.logsURL = logsURL ?? base.appendingPathComponent("supervisor.log")
        self.endpoint = endpoint ?? environment["TIMELORD_STATUS_URL"].flatMap(URL.init(string:)) ?? URL(string: "http://127.0.0.1:9257/v1/status")!
    }

    func refresh() async {
        guard !changingIntent else { return }
        controlEpoch &+= 1
        let epoch = controlEpoch
        do {
            let current = try await client.send(.status)
            guard epoch == controlEpoch, !changingIntent else { return }
            observe(current); errorMessage = current.error
        } catch {
            guard epoch == controlEpoch, !changingIntent else { return }
            supervisor = nil; errorMessage = "Background supervision unavailable: \(error.localizedDescription)"
        }
    }

    func ensureStarted(network: String, monitorListen: String, statusListen: String) async throws {
        guard !changingIntent else { throw ConfigurationError.message("A background service change is already in progress.") }
        controlEpoch &+= 1
        changingIntent = true
        defer { changingIntent = false; onChange?() }
        do {
            observe(try await client.ensureStarted(worker: helperURL, network: network, monitorListen: monitorListen, statusListen: statusListen))
            errorMessage = supervisor?.error
        } catch { errorMessage = error.localizedDescription; throw error }
    }
    /// What a control transaction waits for after the supervisor acknowledges it.
    private enum Completion {
        /// Acknowledgment is the result (Resume: the tick starts the worker).
        case acknowledged
        /// Pause is durable on acknowledgment; wait until the worker family is retired.
        case paused
        /// Reload is acknowledged after validation, once retirement has started;
        /// wait for the replacement worker, a Pause, or a reported failure.
        case replaced
    }
    /// Bounds the wait after acknowledgment. Graceful retirement is bounded by
    /// the supervisor (about 29 s worst case); the rest covers starting the replacement.
    static let completionTimeout: TimeInterval = 90

    /// Fresh revision protects a remote administrator's concurrent Pause/Run.
    @discardableResult
    private func control(_ command: SupervisorClient.Command, completion: Completion = .acknowledged) async throws -> SupervisorStatus {
        guard !changingIntent else { throw ConfigurationError.message("A background service change is already in progress.") }
        controlEpoch &+= 1
        changingIntent = true; defer { changingIntent = false; onChange?() }
        let current: SupervisorStatus
        do {
            current = try await client.send(.status)
        } catch {
            errorMessage = error.localizedDescription; throw error
        }
        observe(current)
        do {
            let acknowledged = try await client.send(command, expectedRevision: current.revision)
            supervisor = acknowledged
            let settled = try await awaitCompletion(completion, before: current, acknowledged: acknowledged)
            supervisor = settled
            errorMessage = settled.error
            return current
        } catch {
            if let refreshed = try? await client.send(.status) { observe(refreshed) }
            errorMessage = error.localizedDescription; throw error
        }
    }
    /// The supervisor never blocks a control request on worker retirement, so
    /// completion is observed through status: the phase, the owned worker PID
    /// and the worker generation, which increases with every started worker.
    private func awaitCompletion(_ completion: Completion, before: SupervisorStatus, acknowledged: SupervisorStatus) async throws -> SupervisorStatus {
        guard completion != .acknowledged else { return acknowledged }
        let deadline = Date().addingTimeInterval(Self.completionTimeout)
        var status = acknowledged
        while true {
            supervisor = status
            // A durable Pause (ours or a concurrent administrator's) settles both waits.
            if status.mode == .paused && (status.phase == .paused || status.phase == .stateError) { return status }
            if let error = status.error, status.phase == .stopping, error.hasPrefix("Worker cleanup failed") {
                throw ConfigurationError.message(error)
            }
            switch completion {
            case .acknowledged:
                return status
            case .paused:
                // Someone resumed while we waited; the Pause no longer applies.
                if status.mode == .running { return status }
            case .replaced:
                if status.phase == .workerStarted && status.workerGeneration > before.workerGeneration { return status }
                if status.phase == .recovering || status.phase == .stateError {
                    throw ConfigurationError.message(status.error ?? "The background service did not restart after reload.")
                }
            }
            guard Date() < deadline else {
                let detail = status.error.map { ": \($0)" } ?? ""
                throw ConfigurationError.message("The background service is still \(status.phase.rawValue)\(detail). Check its status before retrying.")
            }
            try await Task.sleep(nanoseconds: 200_000_000)
            status = try await client.send(.status)
        }
    }
    private func observe(_ status: SupervisorStatus) {
        supervisor = status
        hasObservedSupervisor = true
    }
    func start() async throws -> Bool { try await control(.resume).mode == .paused }
    func stop() async throws { try await control(.pause, completion: .paused) }
    func reload() async throws { try await control(.reload, completion: .replaced) }

    /// Only an explicit setup action opens the macOS administrator approval prompt.
    /// The installer validates ownership/signatures and refuses competing workers.
    func install() async throws {
        let script = appURL.appendingPathComponent("Contents/Resources/install-service.sh")
        let command = ["/bin/bash", script.path, "install", "--app", appURL.path,
                       "--config", configURL.path, "--user", NSUserName()]
            .map(Self.shellQuote).joined(separator: " ")
        let appleScript = "do shell script \"\(Self.appleScriptQuote(command))\" with administrator privileges"
        let executor = ConfigurationClient(helper: URL(fileURLWithPath: "/usr/bin/osascript"), config: configURL)
        _ = try await executor.run(["-e", appleScript])
        await refresh()
    }
    private static func shellQuote(_ value: String) -> String { "'" + value.replacingOccurrences(of: "'", with: "'\"'\"'") + "'" }
    private static func appleScriptQuote(_ value: String) -> String {
        value.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
    }

    func prepareConfig() throws {
        try FileManager.default.createDirectory(at: configURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        guard !FileManager.default.fileExists(atPath: configURL.path) else { return }
        let text = """
        # Choose a node identity and discover boards in Timelord before starting.
        schema_version = 1
        [node]
        host = "localhost"
        port = 8444
        network = "mainnet"
        [service]
        status_listen = "127.0.0.1:9257"
        [hardware]
        boards = []
        """
        try Data(text.utf8).write(to: configURL, options: [.withoutOverwriting])
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: configURL.path)
    }

}
