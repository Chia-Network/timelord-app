import SwiftUI
import TimelordPresentation

struct ConnectionStrip: View {
    @ObservedObject var model: TimelordModel
    private var asicTone: StatusTone {
        if model.snapshot.boards.contains(where: { $0.state == .thermalStop }) { return .critical }
        if model.snapshot.boards.contains(where: { $0.state == .disconnected }) { return .attention }
        return model.snapshot.boards.contains(where: { $0.state == .active }) ? .positive : .neutral
    }
    var body: some View {
        HStack(spacing: 10) {
            connection("Full node", symbol: "network", value: model.snapshot.node, showHeight: true) { model.sheet = .node }
            Image(systemName: "chevron.left.forwardslash.chevron.right").foregroundStyle(.tertiary).accessibilityHidden(true)
            connection("Timelord", symbol: "clock.arrow.circlepath", value: model.snapshot.timelord) { model.sheet = .timelord }
            Image(systemName: "chevron.left.forwardslash.chevron.right").foregroundStyle(.tertiary).accessibilityHidden(true)
            Button { model.sheet = .boards } label: {
                VStack(alignment: .leading, spacing: 7) {
                    HStack(spacing: 12) {
                        Image(systemName: "cpu").font(.title2).foregroundStyle(asicTone.color)
                        VStack(alignment: .leading, spacing: 5) {
                            Text("ASICs").font(.caption).foregroundStyle(.secondary)
                            Text(model.snapshot.boards.isEmpty ? "No boards connected" : "\(model.snapshot.boards.count) \(model.snapshot.boards.count == 1 ? "board" : "boards")").font(.headline)
                        }
                    }
                    Text("\(model.snapshot.boards.filter { $0.state == .active }.count) producing proofs").font(.caption).foregroundStyle(.secondary)
                }.frame(maxWidth: .infinity, minHeight: 78, alignment: .leading).timelordCard(padding: 16)
            }.buttonStyle(.plain).accessibilityHint("Open board status")
        }
    }

    private func connection(_ title: String, symbol: String, value: ConnectionSnapshot, showHeight: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 12) {
                Image(systemName: symbol).font(.title2).foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 5) {
                    Text(title).font(.caption).foregroundStyle(.secondary)
                    Text(value.title).font(.headline).lineLimit(1)
                    if showHeight {
                        HStack(spacing: 6) {
                            Text(value.detail).foregroundStyle(value.tone.color)
                            Text("· Height \(value.height.map { $0.formatted() } ?? "—")").foregroundStyle(.secondary)
                        }.font(.caption).lineLimit(1).minimumScaleFactor(0.85)
                    } else {
                        StatusBadge(title: value.detail, tone: value.tone).font(.caption).lineLimit(1)
                    }
                }
                Spacer(minLength: 0)
            }.frame(maxWidth: .infinity, minHeight: 78, alignment: .leading).timelordCard(padding: 16)
        }.buttonStyle(.plain).accessibilityHint("Open \(title.lowercased()) details")
    }
}
