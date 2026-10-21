import Foundation
import TimelordPresentation

actor ConfigurationClient {
    let helper: URL
    let config: URL
    init(helper: URL, config: URL) { self.helper = helper; self.config = config }

    func load() async throws -> AppConfiguration { try decode(await run(["configuration", "--config", config.path])) }
    func save(_ configuration: AppConfiguration) async throws {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("timelord-config-\(UUID().uuidString).json")
        try encoder.encode(configuration).write(to: path, options: .atomic)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: path.path)
        defer { try? FileManager.default.removeItem(at: path) }
        _ = try await run(["configure", "--config", config.path, "--input", path.path])
    }
    func testNode() async throws -> String {
        let data = try await run(["test-node", "--config", config.path])
        let response = try JSONDecoder().decode(ConnectionResult.self, from: data)
        return response.message
    }
    func importNode(bundle: String) async throws { _ = try await run(["import-node", "--config", config.path, "--bundle", bundle]) }
    func localNode(root: String) async throws { _ = try await run(["local-node", "--config", config.path, "--chia-root", root]) }
    func nodeState() async throws -> NodeSyncStatus { try decode(await run(["node-state", "--config", config.path])) }
    func tuning(paused: Bool) async throws -> TuningResponse { try decode(await run(["tuning","--config",config.path,"--paused",String(paused)])) }
    func devices() async throws -> [DiscoveredDevice] { try decode(await run(["devices", "--config", config.path])) }
    private func decode<T: Decodable>(_ data: Data) throws -> T {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(T.self, from: data)
    }
    private func run(_ arguments: [String]) async throws -> Data {
        try Task.checkCancellation()
        guard FileManager.default.isExecutableFile(atPath: helper.path) else { throw ConfigurationError.message("The service helper could not be found.") }
        let process = Process(); process.executableURL = helper; process.arguments = arguments
        let output = Pipe(); let errors = Pipe(); process.standardOutput = output; process.standardError = errors
        let stdout = Task.detached { output.fileHandleForReading.readDataToEndOfFile() }
        let stderr = Task.detached { errors.fileHandleForReading.readDataToEndOfFile() }
        let status: Int32 = try await withTaskCancellationHandler(operation: { try await withCheckedThrowingContinuation { continuation in
            process.terminationHandler = { child in continuation.resume(returning: child.terminationStatus) }
            do { try process.run(); if Task.isCancelled {process.terminate()} } catch {
                try? output.fileHandleForWriting.close(); try? errors.fileHandleForWriting.close()
                continuation.resume(throwing: error)
            }
        } }, onCancel: { if process.isRunning { process.terminate() } })
        let data = await stdout.value; let errorData = await stderr.value
        try Task.checkCancellation()
        guard status == 0 else { throw ConfigurationError.message(String(data: errorData, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "The operation failed.") }
        return data
    }
}
private struct ConnectionResult: Decodable { let authenticated: Bool; let message: String }
enum ConfigurationError: LocalizedError {
    case message(String)
    var errorDescription: String? { switch self { case .message(let text): return text } }
}

struct TuningResponse: Decodable { let paused: Bool; let servicePid: Int }
