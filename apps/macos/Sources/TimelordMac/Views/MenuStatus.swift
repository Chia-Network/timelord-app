import AppKit
import SwiftUI
import TimelordPresentation

struct MenuStatus: View {
    @ObservedObject var model: TimelordModel
    @Environment(\.openWindow) private var openWindow
    var body: some View {
        VStack(alignment: .leading, spacing: 15) {
            Label("Timelord", systemImage: "clock.arrow.circlepath").font(.headline)
            StatusBadge(title: model.snapshot.phase.title, tone: model.snapshot.phase.tone)
            HStack {
                VStack(alignment: .leading, spacing: 4) {
                    Text(model.snapshot.headlineSpeed).font(.title2).monospacedDigit()
                    Text("Challenge · 1h average").font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                Text("\(model.snapshot.boards.count) \(model.snapshot.boards.count == 1 ? "board" : "boards")").font(.callout).foregroundStyle(.secondary)
            }
            Text(model.snapshot.proofFreshness).font(.caption).foregroundStyle(.secondary)
            Divider()
            Button(model.primaryActionTitle) { openMain(); model.primaryAction() }
            Button("Open Timelord") { openMain() }
            Button("Settings…") { openMain(); model.sheet = .settings }
            Divider()
            Button(model.quitLabel) { model.stopAndQuit() }
        }.padding(20).frame(width: 300)
    }
    private func openMain() { openWindow(id: "main"); NSApplication.shared.activate(ignoringOtherApps: true) }
}
