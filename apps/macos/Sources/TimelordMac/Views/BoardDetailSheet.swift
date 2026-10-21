import SwiftUI
import TimelordPresentation

struct BoardDetailSheet: View {
    let board: BoardSnapshot
    let snapshot: AppSnapshot
    @State private var range: HistoryRange = .hour
    @State private var chain: Chain

    init(board: BoardSnapshot, snapshot: AppSnapshot) {
        self.board = board
        self.snapshot = snapshot
        _chain = State(initialValue: board.histories.first?.chain ?? .challenge)
    }

    private var history: ChainHistory { board.histories.first { $0.chain == chain } ?? .empty(chain) }

    var body: some View {
        VStack(spacing: 0) {
            SheetHeader(title: board.name)
            Divider()
            ScrollView {
                content
            }
        }.frame(width: 740, height: 740).background(Color(nsColor: .windowBackgroundColor))
    }

    var content: some View {
                VStack(alignment: .leading, spacing: 24) {
                    StatusBadge(title: board.state.title, tone: board.state.tone)
                    VStack(alignment: .leading, spacing: 18) {
                        HStack {
                            Text("Delivered speed").font(.headline)
                            Spacer()
                            Picker("Time range", selection: $range) { ForEach(HistoryRange.allCases) { Text($0.title).tag($0) } }
                                .labelsHidden().frame(width: 145)
                        }
                        if !board.histories.isEmpty {
                            Picker("Chain", selection: $chain) { ForEach(board.histories) { Text($0.chain.title).tag($0.chain) } }
                                .pickerStyle(.segmented)
                        }
                        IPSHistoryChart(history: history, range: range, now: snapshot.now, upperBound: snapshot.chartUpperBound)
                            .frame(height: 220)
                        statistics
                        Text("Verified proof observations for this chain. Slow proofs around recoveries remain included. Gaps and work interruptions break the line; telemetry coverage is reported separately.")
                            .font(.caption).foregroundStyle(.secondary)
                    }.timelordCard()
                    HStack(alignment: .top, spacing: 18) {
                        VStack(alignment: .leading, spacing: 10) {
                            Text("Temperature").font(.headline)
                            Text(board.temperatureText).font(.title).monospacedDigit()
                            Text(board.temperatureLimitC.map { "Stop limit: \(Int($0))°C" } ?? "No hardware measurements yet")
                                .font(.caption).foregroundStyle(.secondary)
                        }.frame(maxWidth: .infinity, alignment: .leading).timelordCard()
                        VStack(alignment: .leading, spacing: 10) {
                            Text("Profile").font(.headline)
                            Text(board.profile).font(.title3)
                            Text("Configured operating point")
                                .font(.caption).foregroundStyle(.secondary)
                        }.frame(maxWidth: .infinity, alignment: .leading).timelordCard()
                    }
                    VStack(alignment: .leading, spacing: 14) {
                        Text("Activity in this range").font(.headline)
                        if history.events(in: range.interval(endingAt: snapshot.now)).isEmpty {
                            Text("No recorded events.").font(.callout).foregroundStyle(.secondary)
                        }
                        ForEach(history.events(in: range.interval(endingAt: snapshot.now))) { event in
                            HStack(alignment: .top, spacing: 12) {
                                Image(systemName: event.kind == .recovery ? "arrow.clockwise" : "info.circle").foregroundStyle(.secondary)
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(event.title).font(.callout)
                                    if event.kind == .telemetryGap { Text("No capture data; reflected in coverage.").font(.caption).foregroundStyle(.secondary) }
                                    if event.kind == .recovery { Text("Observed proof speeds remain in the average.").font(.caption).foregroundStyle(.secondary) }
                                }
                                Spacer()
                                Text(event.date, style: .time).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }.timelordCard()
                    DisclosureGroup("Advanced · For administrators") {
                        VStack(alignment: .leading, spacing: 12) {
                            Text("The app manages chain assignments. Engine changes must keep all required chains covered.")
                                .font(.caption).foregroundStyle(.secondary)
                            ForEach(board.histories) { row in
                                HStack { Text(row.chain.title); Spacer(); Text(board.state.title).foregroundStyle(.secondary) }.font(.callout)
                            }
                            Divider()
                            LabeledContent("Frequency", value: board.frequencyMHz.map { "\(Int($0)) MHz" } ?? "—")
                            LabeledContent("Requested voltage", value: board.voltageV.map { String(format: "%.3f V", $0) } ?? "—")
                            if let automatic=board.automatic { Text(automatic.message).font(.callout) }
                            Text(board.isAutomatic ? "Automatic tunes while proofs continue. Pause tuning on the dashboard to hold the observed setting." : "Change operating points in board settings. Restart Timelord to apply changes.").font(.caption).foregroundStyle(.secondary)
                        }.padding(.top, 12)
                    }.timelordCard()
                }.padding(24)
    }

    private var statistics: some View {
        let stats = history.statistics(in: range.interval(endingAt: snapshot.now))
        return HStack(alignment: .top, spacing: 16) {
            MetricTile(title: "Average", value: DisplayFormat.ips(stats.average), caption: "\(stats.observations) observations")
            MetricTile(title: "Lowest", value: DisplayFormat.ips(stats.minimum), caption: "Recorded proof IPS")
            MetricTile(title: "Highest", value: DisplayFormat.ips(stats.maximum), caption: "Recorded proof IPS")
            MetricTile(title: "Coverage", value: String(format: "%.1f%%", stats.coverage * 100), caption: "Telemetry available")
        }
    }
}
