import AppKit
import SwiftUI
import TimelordPresentation

enum AppSheet: Identifiable {
    case node, timelord, boards, settings, board(String)
    var id: String { switch self { case .node: return "node"; case .timelord: return "timelord"; case .boards: return "boards"; case .settings: return "settings"; case .board(let id): return id } }
}

@MainActor
final class TimelordModel: ObservableObject {
    @Published private(set) var snapshot = LivePresentation.snapshot(status: nil)
    @Published var sheet: AppSheet?
    @Published var errorMessage: String?
    @Published private(set) var starting = false
    @Published var configuration = AppConfiguration()
    @Published private(set) var configurationLoaded = false
    @Published private(set) var configurationNeedsRestart = false
    @Published private(set) var operationInProgress = false
    @Published var connectionResult: String?
    @Published private(set) var discoveredDevices: [DiscoveredDevice] = []
    private var lastDiscovery = Date.distantPast
    private var discoveryError: String?
    private let controller: ServiceController
    private var liveClient: LiveStatusClient
    private let configurationClient: ConfigurationClient
    private var savedConfiguration = AppConfiguration()
    private var liveStatus: ServiceStatus?
    private var history: HistoryPayload?
    private var nodeSync: NodeSyncStatus?
    private var lastNodeSyncFetch = Date.distantPast
    private var lastHistoryFetch = Date.distantPast
    private var pausedByUser = false
    private var quitting = false
    private var monitoring: Task<Void, Never>?
    private let monitoringEnabled: Bool

    var canControlService: Bool { controller.canControlService }
    var configURL: URL { controller.configURL }
    var logsURL: URL { controller.logsURL }
    var statusURL: URL { liveClient.endpoint }
    var quitLabel: String { "Quit interface" }
    var canStartService: Bool { !quitting && canControlService && !controller.desiredRunning && !starting }
    var canInspectConnections: Bool { !controller.desiredRunning && liveStatus == nil && !starting }

    init(controller suppliedController: ServiceController? = nil, initialConfiguration: AppConfiguration? = nil, monitoringEnabled: Bool = true) {
        let controller = suppliedController ?? ServiceController(); self.controller = controller
        self.monitoringEnabled = monitoringEnabled
        liveClient = LiveStatusClient(endpoint: controller.endpoint)
        configurationClient = ConfigurationClient(helper: controller.helperURL, config: controller.configURL)
        if let initialConfiguration {
            configuration = initialConfiguration; savedConfiguration = initialConfiguration; configurationLoaded = true
        }
        controller.onChange = { [weak self] in
            guard let self else { return }; self.objectWillChange.send()
            if monitoringEnabled { Task { await self.refresh() } }
        }
        guard monitoringEnabled else { return }
        monitoring = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refresh()
                do { try await Task.sleep(nanoseconds: 2_000_000_000) } catch { break }
            }
        }
    }

    private func useSavedStatusEndpoint() {
        let endpoint = ProcessInfo.processInfo.environment["TIMELORD_STATUS_URL"].flatMap(URL.init(string:))
            ?? URL(string: "http://\(savedConfiguration.service.statusListen)/v1/status")!
        liveClient = LiveStatusClient(endpoint: endpoint)
    }
    private func updateSnapshot() {
        snapshot = LivePresentation.snapshot(status: liveStatus, history: history, configuration: configurationLoaded ? savedConfiguration : nil, detectedDevices: discoveredDevices, nodeSync: nodeSync, paused: pausedByUser, starting: starting, error: controller.errorMessage)
    }
    func refresh() async {
        guard !quitting else { return }
        if !configurationLoaded {
            do {
                try controller.prepareConfig(); configuration = try await configurationClient.load(); savedConfiguration = configuration; configurationLoaded = true
                let packagedClient = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/hw_vdf_client").path
                if configuration.hardware.clientBinary == nil, FileManager.default.isExecutableFile(atPath: packagedClient) { configuration.hardware.clientBinary = packagedClient; savedConfiguration.hardware.clientBinary = packagedClient }
                useSavedStatusEndpoint()
            } catch { errorMessage = error.localizedDescription }
        }
        await controller.refresh()
        pausedByUser = controller.supervisor?.mode == .paused && controller.supervisor?.phase != "state_error"
        await refreshNodeSync()
        do {
            if canControlService && controller.processID == nil { throw ConfigurationError.message(controller.errorMessage ?? (pausedByUser ? "Timelord is paused." : "Background service is recovering.")) }
            let result = try await liveClient.fetch(expectedPID: controller.processID)
            if liveStatus?.servicePid != result.servicePid || liveStatus?.startedUnix != result.startedUnix { history = nil; lastHistoryFetch = .distantPast }
            liveStatus = result
            if Date().timeIntervalSince(lastHistoryFetch) >= 15 {
                do { history = try await liveClient.history(expectedPID: result.servicePid); lastHistoryFetch = Date() }
                catch { /* Keep the previous history with its original coverage end. */ }
            }
            updateSnapshot()
        } catch {
            let hadService = liveStatus != nil || canControlService; liveStatus = nil; history = nil
            snapshot = LivePresentation.snapshot(status: nil, configuration: configurationLoaded ? savedConfiguration : nil, detectedDevices: discoveredDevices, nodeSync: nodeSync, paused: pausedByUser, starting: starting, error: hadService && !pausedByUser ? error.localizedDescription : controller.errorMessage)
        }
        if !operationInProgress && !starting && Date().timeIntervalSince(lastDiscovery)>15 { await refreshDevices() }
    }

    private func refreshNodeSync() async {
        if configurationLoaded && Date().timeIntervalSince(lastNodeSyncFetch) >= 15 {
            lastNodeSyncFetch = Date()
            let expectedNode = savedConfiguration.node
            let result = expectedNode.rpc == nil ? nil : try? await configurationClient.nodeState()
            if savedConfiguration.node == expectedNode { nodeSync = result }
        }
    }

    var canPause: Bool { canControlService && controller.desiredRunning && !starting }
    var runControlTitle: String { starting ? "Starting…" : canPause ? "Pause" : "Run" }
    func toggleRun() {
        if canPause { stopService() }
        else if canStartService { startService() }
        else { primaryAction() }
    }

    var primaryActionTitle: String {
        if canPause && snapshot.phase != .temperatureLimit && snapshot.phase != .serviceUnavailable { return "Pause" }
        switch snapshot.phase {
        case .running: return "Details"
        case .paused: return "Resume"
        case .nodeUnreachable, .waitingForNode: return "Check connection"
        case .boardMissing, .waitingForProofs: return "Check boards"
        case .temperatureLimit: return "View board"
        case .starting: return "Starting…"
        case .serviceUnavailable: return "View details"
        case .setupRequired:
            if !savedConfiguration.node.hasIdentity { return "Connect node" }
            if savedConfiguration.hardware.boards.isEmpty || savedConfiguration.hardware.clientBinary == nil { return "Set up boards" }
            return canControlService ? "Start Timelord" : "Enable background service"
        }
    }
    func primaryAction() {
        if canPause && snapshot.phase != .temperatureLimit && snapshot.phase != .serviceUnavailable { stopService(); return }
        if snapshot.phase == .paused { startService(); return }
        switch snapshot.phase {
        case .boardMissing, .waitingForProofs: sheet = .boards
        case .temperatureLimit: sheet = snapshot.boards.first(where: { $0.state == .thermalStop }).map { .board($0.id) } ?? .boards
        case .serviceUnavailable, .running: sheet = .timelord
        case .setupRequired:
            if !savedConfiguration.node.hasIdentity { sheet = .node }
            else if savedConfiguration.hardware.boards.isEmpty || savedConfiguration.hardware.clientBinary == nil { sheet = .boards }
            else if canControlService { startService() }
            else { sheet = .settings }
        default: sheet = .node
        }
    }
    func useBundledAutomaticClient() {
        let path = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/hw_vdf_client").path
        if FileManager.default.isExecutableFile(atPath: path) {
            configuration.hardware.clientBinary = path
            configuration.hardware.driverLibrary = Bundle.main.bundleURL.appendingPathComponent("Contents/Frameworks/libftd2xx.dylib").path
        }
    }
    func toggleTuning() async {
        guard canControlService, !operationInProgress, let pid = controller.processID else { return }
        operationInProgress = true; defer { operationInProgress = false }
        do {
            let response = try await configurationClient.tuning(paused: !snapshot.tuningPaused)
            guard response.servicePid == Int(pid) else { throw ConfigurationError.message("Tuning response came from another service.") }
            await refresh()
        } catch { errorMessage = error.localizedDescription }
    }
    func startService() {
        guard canStartService else { return }
        starting = true; errorMessage = nil
        Task {
            defer { starting = false }
            guard !quitting else { return }
            do { try await controller.start() }
            catch { errorMessage = error.localizedDescription }
            starting = false
            if monitoringEnabled { await refresh() }
        }
    }
    func stopService() {
        guard canPause else { return }
        starting = true; errorMessage = nil
        Task {
            defer { starting = false }
            do { try await controller.stop() }
            catch { errorMessage = error.localizedDescription }
            starting = false
            if monitoringEnabled { await refresh() }
        }
    }
    func restartService() async {
        guard canControlService, !starting else { return }
        starting = true; defer { starting = false }
        do {
            try await controller.reload()
            liveStatus = nil; history = nil
            configurationNeedsRestart = false; useSavedStatusEndpoint()
        } catch { errorMessage = error.localizedDescription }
    }
    func installBackgroundService() async {
        guard !quitting, !starting, !operationInProgress else { return }
        await saveConfiguration()
        guard errorMessage == nil, !quitting else { return }
        operationInProgress = true; defer { operationInProgress = false }
        do { try await controller.install(); await refresh() }
        catch { errorMessage = error.localizedDescription }
    }
    var hasUnsavedSettings: Bool { configuration != savedConfiguration }
    private var hasRuntimeSettingsChanges: Bool {
        var runtimeConfiguration = configuration
        runtimeConfiguration.service.startAutomatically = savedConfiguration.service.startAutomatically
        return runtimeConfiguration != savedConfiguration
    }
    var settingsDoneTitle: String {
        canControlService && (hasRuntimeSettingsChanges || configurationNeedsRestart) ? "Save & Restart" : "Done"
    }
    var canFinishSettings: Bool {
        configurationLoaded && !operationInProgress && !starting
    }
    var settingsCompletionHint: String {
        if canControlService {
            return hasRuntimeSettingsChanges || configurationNeedsRestart
                ? "Save & Restart applies your changes and briefly restarts Timelord."
                : "Your settings are saved. Timelord keeps running."
        }
        if liveStatus != nil { return "Done saves your changes. Restart the externally managed service to apply them." }
        return "Timelord runs independently of this interface. Enable the background service to start at boot."
    }
    /// A settings sheet only dismisses after validation and persistence succeed.
    func finishSettings() async -> Bool {
        guard canFinishSettings else { return false }
        if hasUnsavedSettings || (!canControlService && liveStatus == nil) {
            await saveConfiguration()
            guard errorMessage == nil else { return false }
        }
        if configurationNeedsRestart && canControlService {
            await restartService()
            guard !configurationNeedsRestart else { return false }
        }
        return true
    }
    func saveConfiguration() async {
        guard !operationInProgress else { return }; operationInProgress = true; defer { operationInProgress = false }
        do {
            let runtimeChanged = hasRuntimeSettingsChanges
            let updated = configuration
            try await configurationClient.save(updated)
            savedConfiguration = updated
            nodeSync = nil; lastNodeSyncFetch = .distantPast
            configurationNeedsRestart = configurationNeedsRestart || ((canControlService || liveStatus != nil) && runtimeChanged)
            errorMessage = nil; configurationLoaded = true
            connectionResult = configurationNeedsRestart ? "Saved. Restart Timelord to apply these settings." : "Settings saved."
            if !configurationNeedsRestart { useSavedStatusEndpoint() }
            updateSnapshot()
        } catch { errorMessage = error.localizedDescription }
    }
    func testNode() async {
        guard canInspectConnections else { errorMessage = "Pause Timelord before testing a connection."; return }
        await saveConfiguration(); guard errorMessage == nil else { return }
        operationInProgress = true; defer { operationInProgress = false }
        do { connectionResult = try await configurationClient.testNode() } catch { errorMessage = error.localizedDescription; connectionResult = nil }
    }
    func discoverBoards() async {
        guard canInspectConnections else { errorMessage = "Pause Timelord before scanning USB devices."; return }
        await saveConfiguration(); guard errorMessage == nil else { return }
        operationInProgress = true; defer { operationInProgress = false }
        await refreshDevices()
    }
    private func refreshDevices() async {
        guard configurationLoaded, savedConfiguration.hardware.clientBinary != nil else { return }
        lastDiscovery=Date()
        do {
            var devices=try await configurationClient.devices()
            // Open D2XX interfaces can hide their enumeration metadata. A fresh
            // supervised USB owner remains presence evidence when its chain is idle.
            for status in liveStatus?.boards ?? [] where status.hardwareState == "connected" || (status.connectedClients>0 && status.hardwareState == "starting") {
                let configured=(savedConfiguration.hardware.boards + savedConfiguration.hardware.rememberedBoards).first(where:{$0.id==status.id})
                let serial=status.serial ?? configured?.serial
                if let location=status.deviceLocation ?? configured?.deviceLocation,
                   !devices.contains(where:{ device in if let serial { return device.serial == serial }; return device.location == location }) {
                    devices.append(DiscoveredDevice(name:status.name,location:location,serial:serial))
                }
            }
            discoveredDevices=devices
            if errorMessage==discoveryError {errorMessage=nil};discoveryError=nil
            savedConfiguration.hardware=BoardDiscovery.reconcile(devices,hardware:savedConfiguration.hardware)
            configuration.hardware=BoardDiscovery.reconcile(devices,hardware:configuration.hardware)
            // The service owns debounced topology changes, including headless
            // deployments. Discovery must not ask users to restart it manually.
            updateSnapshot()
        } catch { if liveStatus == nil {discoveredDevices=[];updateSnapshot()}; discoveryError=error.localizedDescription;errorMessage=error.localizedDescription }
    }
    func importNode(local: Bool) async {
        guard !operationInProgress else { return }
        let panel = NSOpenPanel(); panel.canChooseFiles = !local; panel.canChooseDirectories = true; panel.allowsMultipleSelection = false
        panel.message = local ? "Choose your Chia root folder." : "Choose connection.toml or its connection bundle folder."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        operationInProgress = true; defer { operationInProgress = false }
        do {
            if local { try await configurationClient.localNode(root: url.path) }
            else {
                var isDirectory: ObjCBool = false; FileManager.default.fileExists(atPath: url.path, isDirectory: &isDirectory)
                try await configurationClient.importNode(bundle: isDirectory.boolValue ? url.appendingPathComponent("connection.toml").path : url.path)
            }
            configuration = try await configurationClient.load(); savedConfiguration = configuration; configurationLoaded = true; errorMessage = nil
            nodeSync = nil; lastNodeSyncFetch = .distantPast
            configurationNeedsRestart = canControlService || liveStatus != nil
            connectionResult = "Connection imported. Test the connection when you’re ready."
            updateSnapshot()
        } catch { errorMessage = error.localizedDescription }
    }
    func chooseFile() -> String? {
        let panel = NSOpenPanel(); panel.canChooseFiles = true; panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        return panel.runModal() == .OK ? panel.url?.path : nil
    }
    func revealConfig() {
        do { try controller.prepareConfig(); NSWorkspace.shared.activateFileViewerSelecting([configURL]) } catch { errorMessage = error.localizedDescription }
    }
    func openLog() {
        if FileManager.default.fileExists(atPath: logsURL.path) { NSWorkspace.shared.open(logsURL) } else { errorMessage = "There is no service log yet." }
    }
    func copyDiagnostics() {
        let lines = ["Timelord local status", "State: \(snapshot.phase.title)", "Boards shown: \(snapshot.boards.count)", snapshot.proofFreshness]
        NSPasteboard.general.clearContents(); NSPasteboard.general.setString(lines.joined(separator: "\n"), forType: .string)
    }
    func stopAndQuit() { NSApplication.shared.terminate(nil) }

    func prepareForTermination() async {
        quitting = true
        monitoring?.cancel(); monitoring = nil
        controller.onChange = nil
        // An intent transaction already sent must complete, but interface exit
        // never sends Pause or terminates the independent supervisor.
        while starting { try? await Task.sleep(nanoseconds: 100_000_000) }
    }
}
