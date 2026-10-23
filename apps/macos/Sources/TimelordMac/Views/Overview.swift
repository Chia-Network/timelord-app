import SwiftUI
import TimelordPresentation

struct Overview: View {
    @ObservedObject var model: TimelordModel
    var body: some View {
        content
        .background(Color(nsColor: .windowBackgroundColor))
        .frame(minWidth: 1120, minHeight: 760)
        .toolbar {
            ToolbarItem(placement: .automatic) {
                Button { model.toggleRun() } label: {
                    Label(model.runControlTitle, systemImage: model.canPause ? "pause.fill" : "play.fill")
                }
                .labelStyle(.titleAndIcon)
                .disabled(model.starting || (model.snapshot.serviceAvailable && !model.ownsService))
                .help(model.canPause ? "Pause Timelord" : "Run Timelord")
            }
            ToolbarItem(placement: .automatic) { Button { model.sheet = .settings } label: { Label("Settings", systemImage: "gearshape") } }
        }
        .sheet(item: $model.sheet) { sheet in AppDetailSheet(selection: sheet, model: model) }
    }

    var content: some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack(alignment: .center, spacing: 24) {
                VStack(alignment: .leading, spacing: 12) {
                    HStack(spacing: 12) {
                        Image(systemName: "circle.fill").font(.system(size: 12)).foregroundStyle(model.snapshot.phase.tone.color)
                        Text(model.snapshot.runningTitle).font(.system(size: 30, weight: .semibold))
                    }
                    if model.snapshot.phase != .running {
                        Text(model.snapshot.explanation).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
                    }
                    Text(model.snapshot.proofFreshness)
                        .font(.caption).foregroundStyle(.secondary).lineLimit(2)
                }.frame(maxWidth: .infinity, alignment: .leading)
                HStack(alignment: .top, spacing: 18) {
                    MetricTile(title: "IPS", value: model.snapshot.headlineSpeed, caption: "Last-hour average")
                    Divider()
                    MetricTile(title: "Proofs delivered", value: model.snapshot.proofsToday.map { $0.formatted() } ?? "—", caption: "Verified proofs sent to your node this run")
                    Divider()
                    MetricTile(title: "Uptime", value: DisplayFormat.uptime(model.snapshot.uptimeSeconds), caption: "Since the timelord started")
                }.fixedSize(horizontal: false, vertical: true)
                    .frame(width: 550).timelordCard(padding: 16)
            }.padding(.vertical, 4)
            ConnectionStrip(model: model)
            HStack {
                Text("Your boards").font(.title3.weight(.semibold))
                Spacer()
                if model.snapshot.automaticActive {
                    Button(model.snapshot.tuningPaused ? "Resume tuning" : "Pause tuning") { Task { await model.toggleTuning() } }
                        .controlSize(.small).disabled(!model.ownsService || model.operationInProgress)
                        .help("Holds or resumes Automatic tuning while proof production continues")
                }
                Text("Chain assignments").font(.caption).foregroundStyle(.secondary)
            }
            boardLayout.frame(maxHeight: .infinity)
            if let error = model.errorMessage { Label(error, systemImage: "exclamationmark.circle").font(.callout).foregroundStyle(.orange).lineLimit(2).help(error).textSelection(.enabled) }
            HStack {
                Label("Closing this window keeps Timelord in the menu bar.", systemImage: "menubar.rectangle").font(.caption).foregroundStyle(.secondary)
                Spacer()
            }
        }.padding(24).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    @ViewBuilder private var boardLayout: some View {
        if model.snapshot.boards.isEmpty {
            VStack(spacing: 14) {
                Image(systemName: "cable.connector.horizontal").font(.system(size: 34, weight: .light)).foregroundStyle(.secondary)
                Text("Connect your ASIC boards").font(.headline)
                Text("Connected boards appear here automatically.")
                    .font(.callout).foregroundStyle(.secondary).multilineTextAlignment(.center)
                Button("Set up boards") { model.sheet = .boards }
            }.frame(maxWidth: .infinity, maxHeight: .infinity).timelordCard()
        } else if model.snapshot.boards.count == 1 {
            card(model.snapshot.boards[0])
        } else {
            HStack(alignment: .top, spacing: 18) {
                ForEach(model.snapshot.boards) { card($0) }
            }
        }
    }
    private func card(_ board: BoardSnapshot) -> some View {
        BoardCard(board: board, snapshot: model.snapshot, open: { model.sheet = .board(board.id) })
    }
}
