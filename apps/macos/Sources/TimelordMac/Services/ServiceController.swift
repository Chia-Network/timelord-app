import Foundation

/// Owns only the helper launched by this app; never an external Chia process.
@MainActor
final class ServiceController {
    let configURL: URL
    let logsURL: URL
    let endpoint: URL
    private let helperOverride: URL?
    var helperURL: URL { helperOverride ?? ProcessInfo.processInfo.environment["TIMELORD_SERVICE_BINARY"].map { URL(fileURLWithPath: $0) } ?? Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/timelord-service") }
    var onChange: (() -> Void)?
    private(set) var errorMessage: String?
    private var process: Process?
    private var logHandle: FileHandle?
    private var activity: NSObjectProtocol?
    var preventsIdleSleep: Bool { activity != nil }

    private func releaseActivity() {
        if let activity { ProcessInfo.processInfo.endActivity(activity) }
        activity = nil
    }
    var ownsService: Bool { process?.isRunning == true }
    var processID: Int32? { process?.processIdentifier }

    init(configURL: URL? = nil, logsURL: URL? = nil, helperURL: URL? = nil, endpoint: URL? = nil) {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("Timelord", isDirectory: true)
        let environment = ProcessInfo.processInfo.environment
        self.helperOverride = helperURL
        self.configURL = configURL ?? environment["TIMELORD_CONFIG"].map { URL(fileURLWithPath: $0) } ?? base.appendingPathComponent("config.toml")
        self.logsURL = logsURL ?? base.appendingPathComponent("service.log")
        let defaultEndpoint = URL(string: "http://127.0.0.1:9257/v1/status")!
        self.endpoint = endpoint ?? environment["TIMELORD_STATUS_URL"].flatMap(URL.init(string:)) ?? defaultEndpoint
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

    func start() throws {
        guard !ownsService else { return }
        // A child can have exited before its main-actor cleanup callback runs.
        // Retire its handle now; that callback must not clear a replacement child.
        if process != nil {
            releaseActivity()
            process = nil; try? logHandle?.close(); logHandle = nil
        }
        let helper = helperURL
        guard FileManager.default.isExecutableFile(atPath: helper.path) else {
            throw NSError(domain: "Timelord", code: 1, userInfo: [NSLocalizedDescriptionKey: "The service helper is missing. Rebuild the development app bundle."])
        }
        try prepareConfig()
        try FileManager.default.createDirectory(at: logsURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        if !FileManager.default.fileExists(atPath: logsURL.path) { FileManager.default.createFile(atPath: logsURL.path, contents: nil) }
        let handle = try FileHandle(forWritingTo: logsURL)
        try handle.seekToEnd()
        let child = Process()
        child.executableURL = helper
        child.arguments = ["serve", "--config", configURL.path]
        child.standardOutput = handle
        child.standardError = handle
        child.terminationHandler = { [weak self] child in
            Task { @MainActor in
                guard let self, self.process === child else { return }
                self.releaseActivity()
                self.process = nil
                try? self.logHandle?.close()
                self.logHandle = nil
                self.errorMessage = child.terminationStatus == 0 ? nil : "The local service stopped unexpectedly. Check its configuration and log."
                self.onChange?()
            }
        }
        do { try child.run() } catch { try? handle.close(); throw error }
        process = child
        activity = ProcessInfo.processInfo.beginActivity(options: .userInitiated, reason: "Producing Chia timelord proofs")
        logHandle = handle
        errorMessage = nil
        onChange?()
    }

    func stop() { if let process, process.isRunning { process.terminate() } }

    /// The helper exits only after its native clients and final history flush finish.
    func stopAndWait(timeout: TimeInterval = 30) async -> Bool {
        guard let child = process else { return true }
        if child.isRunning { child.terminate() }
        let deadline = ProcessInfo.processInfo.systemUptime + timeout
        while child.isRunning && ProcessInfo.processInfo.systemUptime < deadline {
            do { try await Task.sleep(nanoseconds: 20_000_000) }
            catch { break }
        }
        guard !child.isRunning else {
            // Keep ownership, the log and the sleep assertion until the actual
            // child exits. Refusing Quit is safer than abandoning native clients.
            errorMessage = "The local service has not stopped. Quit was cancelled; the app still owns the service. Check its log and try again."
            onChange?()
            return false
        }
        // The termination callback may still be queued on the main actor.
        if process === child {
            releaseActivity()
            process = nil
            try? logHandle?.close(); logHandle = nil
        }
        return true
    }
}
