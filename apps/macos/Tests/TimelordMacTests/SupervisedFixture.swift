import Foundation
import XCTest
@testable import TimelordMac
import TimelordPresentation

/// No launchd registration, USB, real certificates, or shared TCP listeners.
@MainActor
final class SupervisedFixture {
    let directory: URL
    let config: URL
    let helper: URL
    let nativeHelper: URL
    let supervisorHelper: URL
    let process: Process
    let controller: ServiceController
    let client: ConfigurationClient

    init() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { root.deleteLastPathComponent() }
        nativeHelper = root.appendingPathComponent("build/rust/debug/timelord-service")
        supervisorHelper = root.appendingPathComponent("build/rust/debug/timelord-supervisor")
        guard [nativeHelper, supervisorHelper].allSatisfy({ FileManager.default.isExecutableFile(atPath: $0.path) }) else { throw XCTSkip("Build helpers with scripts/check.sh first.") }
        directory = URL(fileURLWithPath: "/private/tmp/tlg-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        config = directory.appendingPathComponent("config.toml")
        helper = directory.appendingPathComponent("worker")
        let script = """
        #!/bin/sh
        case "$1" in
          serve)
            shift
            while [ "$#" -gt 0 ]; do
              if [ "$1" = --config ]; then cp "$2" "$0.started.$$"; break; fi
              shift
            done
            exec /bin/sleep 60 ;;
          devices) printf '%s' '[{"name":"Fixture","location":1,"serial":"fixture-only"}]' ;;
          *) exec '\(nativeHelper.path)' "$@" ;;
        esac
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        controller = ServiceController(configURL: config, logsURL: directory.appendingPathComponent("fixture.log"), helperURL: helper, endpoint: URL(string: "http://127.0.0.1:0/v1/status")!, supervisorURL: supervisorHelper)
        client = ConfigurationClient(helper: helper, config: config)
        process = Process(); process.executableURL = supervisorHelper
        process.arguments = ["run", "--config", config.path, "--worker", helper.path]
        let log = directory.appendingPathComponent("supervisor.log")
        FileManager.default.createFile(atPath: log.path, contents: nil)
        let logHandle = try FileHandle(forWritingTo: log)
        process.standardOutput = logHandle; process.standardError = logHandle
    }
    func launch(_ configuration: AppConfiguration = AppConfiguration()) async throws {
        try await client.save(configuration)
        try process.run()
        try await waitForWorker()
    }
    func waitForWorker(excluding old: Int32? = nil) async throws {
        let deadline = Date().addingTimeInterval(8)
        while Date() < deadline {
            await controller.refresh()
            if let pid = controller.processID, pid != old,
               FileManager.default.fileExists(atPath: helper.path + ".started.\(pid)") { return }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        let log = (try? String(contentsOf: directory.appendingPathComponent("supervisor.log"))) ?? ""
        throw ConfigurationError.message("Fixture supervisor did not start: \(controller.errorMessage ?? "unknown"); \(log)")
    }
    func close() {
        if process.isRunning { process.terminate(); process.waitUntilExit() }
        try? FileManager.default.removeItem(at: directory)
    }
}
