import Foundation

/// Controls an independently installed supervisor. The interface never owns or
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
    private var changingIntent = false
    private var controlEpoch: UInt64 = 0
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
            supervisor = current; errorMessage = current.error
        } catch {
            guard epoch == controlEpoch, !changingIntent else { return }
            supervisor = nil; errorMessage = "Background supervision unavailable: \(error.localizedDescription)"
        }
    }

    /// Fresh revision protects a remote administrator's concurrent Pause/Run.
    private func control(_ command: SupervisorClient.Command) async throws {
        guard !changingIntent else { throw ConfigurationError.message("A background service change is already in progress.") }
        controlEpoch &+= 1
        changingIntent = true; defer { changingIntent = false; onChange?() }
        do {
            let current = try await client.send(.status)
            supervisor = current
            supervisor = try await client.send(command, expectedRevision: current.revision)
            errorMessage = supervisor?.error
        } catch { errorMessage = error.localizedDescription; throw error }
    }
    func start() async throws { try await control(.resume) }
    func stop() async throws { try await control(.pause) }
    func reload() async throws { try await control(.reload) }

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
