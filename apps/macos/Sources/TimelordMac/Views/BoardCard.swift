import SwiftUI
import TimelordPresentation

struct BoardCard: View {
    let board: BoardSnapshot
    let snapshot: AppSnapshot
    let open: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Button(action: open) {
                VStack(alignment: .leading, spacing: 10) {
                    HStack {
                        Image(systemName: "cpu").font(.title2).foregroundStyle(board.state.tone.color)
                            .help(board.state.title).accessibilityLabel(board.state.title)
                        Text(board.name).font(.headline).lineLimit(1)
                        Spacer()
                        Text(board.temperatureText).font(.headline).monospacedDigit().foregroundStyle(board.state == .thermalStop ? Color.red : Color.primary)
                    }
                    ForEach(Array(board.histories.enumerated()), id: \.element.id) { index, history in
                        VStack(alignment: .leading, spacing: 6) {
                            HStack {
                                Label(history.chain.title, systemImage: "circle.fill").font(.subheadline).foregroundStyle(history.chain.color)
                                Spacer()
                            }
                            HStack(spacing: 12) {
                                let raw = board.state == .active || board.state == .waiting ? board.rawSpeeds.first(where: { $0.engine == index }) : nil
                                Text("Raw \(raw?.displayText(at: snapshot.now) ?? "—")")
                                    .help(raw?.state == "sampling" ? "Last sample; starting a new chain." : "Recent hardware speed.")
                                Text("Mean \(snapshot.recentSpeed(for: history))")
                                    .help("Delivered IPS · 5-minute mean")
                                Text("Median \(snapshot.recentMedian(for: history))")
                                    .help("Delivered IPS · 5-minute median")
                            }.font(.caption).monospacedDigit().foregroundStyle(.secondary).lineLimit(1).minimumScaleFactor(0.85)
                            IPSHistoryChart(history: history, range: .hour, now: snapshot.now, upperBound: snapshot.chartUpperBound, compact: true)
                                .frame(minHeight: 42, maxHeight: .infinity)
                        }
                    }
                    if board.histories.isEmpty { Text("No chains assigned").font(.callout).foregroundStyle(.secondary).frame(maxHeight: .infinity) }
                    HStack { Text("1 hour ago"); Spacer(); Text("Now") }.font(.caption2).foregroundStyle(.secondary)
                }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            }.buttonStyle(.plain)
                .accessibilityHint("Show \(board.name) history, temperature and profile")
            Divider()
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(board.isAutomatic && board.frequencyMHz != nil && board.voltageV != nil ? String(format:"Automatic · %.0f MHz · %.3f V",board.frequencyMHz!,board.voltageV!) : "\(board.profile) profile")
                        .font(.caption).foregroundStyle(.secondary).lineLimit(1).minimumScaleFactor(0.85)
                    Text(board.isAutomatic ? board.automaticTitle : "Settings held").font(.caption.weight(.medium))
                        .foregroundStyle(Color.secondary)
                        .help(board.automatic?.message ?? "")
                }
                Spacer(minLength: 0)
                Button(action: open) {
                    HStack(spacing: 5) {
                        Text("Details")
                        Image(systemName: "chevron.right").font(.caption).foregroundStyle(.tertiary)
                    }
                }
                    .buttonStyle(.plain).font(.callout)
                    .accessibilityLabel("\(board.name) details")
            }
        }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading).timelordCard()
    }
}
