import SwiftUI
import TimelordPresentation

struct AppDetailSheet: View {
    let selection: AppSheet
    @ObservedObject var model: TimelordModel
    var body: some View {
        switch selection {
        case .board(let id):
            if let board = model.snapshot.boards.first(where: { $0.id == id }) { BoardDetailSheet(board: board, snapshot: model.snapshot).id(id) }
            else { VStack { SheetHeader(title: "Board unavailable"); Text("This board is no longer connected.").padding(24) } }
        case .node: NodeConnectionSheet(model: model)
        case .boards: BoardSetupSheet(model: model)
        case .settings: SettingsSheet(model: model)
        case .timelord:
            VStack(spacing: 0) {
                SheetHeader(title: "Timelord"); Divider()
                VStack(alignment: .leading, spacing: 20) {
                    Text("Timelord runs on this Mac. It coordinates the ASICs and sends verified proofs to your full node.").font(.callout)
                    StatusBadge(title: model.snapshot.timelord.title, tone: model.snapshot.timelord.tone)
                    LabeledContent("Full node", value: model.snapshot.node.title)
                    LabeledContent("Boards", value: "\(model.snapshot.boards.count)")
                    HStack {
                        Button("Start Timelord") { model.startService() }.disabled(!model.canStartService)
                        Button("Pause") { model.stopService() }.disabled(!model.ownsService)
                    }
                    if model.configurationNeedsRestart { Button("Apply settings and restart") { Task { await model.restartService() } }.disabled(!model.ownsService) }
                    OperationFeedback(model: model)
                }.padding(24)
            }.frame(width: 620).background(Color(nsColor: .windowBackgroundColor))
        }
    }
}

struct OperationFeedback: View {
    @ObservedObject var model: TimelordModel
    var body: some View {
        if model.operationInProgress { ProgressView().controlSize(.small) }
        if let result = model.connectionResult { Text(result).font(.callout).foregroundStyle(.secondary).textSelection(.enabled) }
        if let error = model.errorMessage { Label(error, systemImage: "exclamationmark.circle").font(.callout).foregroundStyle(.orange).textSelection(.enabled) }
    }
}

struct NodeConnectionSheet: View {
    @ObservedObject var model: TimelordModel
    @State private var location: String
    init(model: TimelordModel) {
        self.model = model
        _location = State(initialValue: ["localhost", "127.0.0.1", "::1"].contains(model.configuration.node.host) ? "local" : "remote")
    }
    var body: some View {
        VStack(spacing: 0) {
            SettingsSheetHeader(title: "Full node", model: model); Divider()
            Form {
                Section {
                    Text("Connect to an existing Chia full node.").foregroundStyle(.secondary)
                    Picker("Location", selection: $location) { Text("This Mac").tag("local"); Text("Another computer").tag("remote") }.pickerStyle(.segmented)
                        .onChange(of: location) { _, value in if value == "local" { model.configuration.node.host = "localhost" } }
                    TextField("Host", text: $model.configuration.node.host)
                    TextField("Port", value: $model.configuration.node.port, format: .number.grouping(.never))
                    Picker("Network", selection: $model.configuration.node.network) { Text("Mainnet").tag("mainnet"); Text("Testnet11").tag("testnet11") }
                }
                Section("Connection identity") {
                    Button(location == "local" ? "Choose Chia folder…" : "Import connection bundle…") { Task { await model.importNode(local: location == "local") } }
                    Text("Use a dedicated client identity supplied by your node administrator.").font(.caption).foregroundStyle(.secondary)
                    credential("Client certificate", path: $model.configuration.node.certificate)
                    credential("Private key", path: $model.configuration.node.privateKey)
                    credential("CA certificate", path: $model.configuration.node.caCertificate)
                    TextField("TLS server name", text: Binding(get: { model.configuration.node.serverName ?? "chia.net" }, set: { model.configuration.node.serverName = $0 }))
                }
                Section("Sync status and height") {
                    if let rpc = model.configuration.node.rpc {
                        LabeledContent("Status connection", value: "\(rpc.host):\(rpc.port)")
                    } else {
                        Text("Choose the Chia folder to enable live sync status and height. A remote node’s connection bundle can include its status connection.").font(.callout).foregroundStyle(.secondary)
                    }
                }
                Section {
                    HStack {
                        Button("Test connection") { Task { await model.testNode() } }.disabled(!model.configuration.node.hasIdentity || !model.canInspectConnections)
                    }
                    Text(model.settingsCompletionHint).font(.caption).foregroundStyle(.secondary)
                    OperationFeedback(model: model)
                }
            }.formStyle(.grouped).disabled(model.operationInProgress)
        }.frame(width: 660, height: 650).interactiveDismissDisabled()
    }
    private func credential(_ title: String, path: Binding<String?>) -> some View {
        HStack {
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                Text(path.wrappedValue.map { URL(fileURLWithPath: $0).lastPathComponent } ?? "Not selected").font(.caption).foregroundStyle(.secondary)
            }
            Spacer(); Button("Choose…") { if let file = model.chooseFile() { path.wrappedValue = file } }
        }
    }
}

struct BoardSetupSheet: View {
    @ObservedObject var model: TimelordModel
    var body: some View {
        VStack(spacing: 0) {
            SettingsSheetHeader(title: "Your boards", model: model); Divider()
            Form {
                Section {
                    Text("Timelord assigns three engines across your connected boards automatically.").foregroundStyle(.secondary)
                    HStack {
                        Text("ASIC client"); Spacer()
                        Text(model.configuration.hardware.clientBinary.map { URL(fileURLWithPath: $0).lastPathComponent } ?? "Choose hw_vdf_client").foregroundStyle(.secondary)
                        Button("Choose…") { if let path = model.chooseFile() { model.configuration.hardware.clientBinary = path } }
                    }
                    Button("Refresh boards") { Task { await model.discoverBoards() } }.disabled(model.configuration.hardware.clientBinary == nil || !model.canInspectConnections)
                }
                ForEach($model.configuration.hardware.boards) { $board in
                    Section(board.name) {
                        TextField("Name", text: $board.name)
                        LabeledContent("Engines",value:String(engineCount(board.id)))
                        Picker("Profile",selection:Binding(get:{board.profile},set:{board.profile=$0;if $0 == "automatic" {model.useBundledAutomaticClient()}})) {
                            Text("Automatic (recommended)").tag("automatic")
                            Text("Manual").tag("manual")
                        }
                        if board.profile == "automatic" {
                            Text(String(format:"Starts at %.0f MHz and requested %.3f V, then learns from verified proofs while Timelord runs. Saved settings are specific to this board, engine count, Mac model and client.",board.automatic.startFrequencyMhz,board.automatic.startVoltageV)).font(.caption).foregroundStyle(.secondary)
                            DisclosureGroup("Automatic limits") {
                                TextField("Maximum frequency (MHz)", value: Binding(get:{board.automatic.maxFrequencyMhz},set:{value in board.automatic.maxFrequencyMhz=value}),format:.number.grouping(.never))
                                Toggle("Pause tuning at startup",isOn:Binding(get:{model.configuration.service.tuningPaused},set:{model.configuration.service.tuningPaused=$0}))
                            }
                        }
                        if board.profile == "manual" { DisclosureGroup("Manual settings") {
                            TextField("Frequency (MHz)", value: $board.frequencyMhz, format: .number.grouping(.never))
                            Stepper(value: $board.voltageV, in: 0.7...1.0, step: 0.00625) { LabeledContent("Requested voltage", value: String(format: "%.4f V", board.voltageV)) }
                        } }
                        DisclosureGroup("Board information") {
                            LabeledContent("Serial",value:board.serial ?? "Available when the board is idle")
                            LabeledContent("USB location",value:board.deviceLocation.map(String.init) ?? "—")
                            LabeledContent("Temperature stop",value:"\(Int(board.temperatureLimitC))°C")
                        }
                    }
                }
                Section {
                    Text(model.settingsCompletionHint).font(.caption).foregroundStyle(.secondary)
                    OperationFeedback(model: model)
                }
            }.formStyle(.grouped).disabled(model.operationInProgress)
        }.frame(width: 700, height: 760).interactiveDismissDisabled()
    }
    private func engineCount(_ id:String)->Int {guard let index=model.configuration.hardware.boards.firstIndex(where:{$0.id==id}) else {return 0};return BoardTopology.counts(model.configuration.hardware.boards)[index]}
}

struct SettingsSheet: View {
    @ObservedObject var model: TimelordModel
    var body: some View {
        VStack(spacing: 0) {
            SettingsSheetHeader(title: "Settings", model: model); Divider()
            Form {
                Section("Connections") {
                    LabeledContent("Full node", value: model.snapshot.node.title)
                    Button("Connection settings…") { model.sheet = .node }
                    Button("Manage boards…") { model.sheet = .boards }
                }
                Section("Appearance") { Text("Follows your Mac’s appearance.").foregroundStyle(.secondary) }
                Section("Startup and background") {
                    Toggle("Run Timelord when the app opens", isOn: Binding(get: { model.configuration.service.startAutomatically }, set: { model.configuration.service.startAutomatically = $0 }))
                    Text("Starts when your node is configured and ASIC boards are connected. Pause keeps Timelord stopped until you press Run or reopen the app.").font(.caption).foregroundStyle(.secondary)
                    Text("Closing the window keeps Timelord in the menu bar.").foregroundStyle(.secondary)
                }
                Section {
                    DisclosureGroup("Administrator") {
                        Text(model.statusURL.absoluteString).font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                        HStack {
                            Button("Reveal configuration") { model.revealConfig() }
                            Button("View log") { model.openLog() }
                            Button("Copy diagnostics") { model.copyDiagnostics() }
                        }
                        if model.configurationNeedsRestart { Button("Apply settings and restart") { Task { await model.restartService() } }.disabled(!model.ownsService) }
                    }
                    OperationFeedback(model: model)
                }
            }.formStyle(.grouped).disabled(model.operationInProgress)
        }.frame(width: 660, height: 600).interactiveDismissDisabled()
    }
}
