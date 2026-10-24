import Charts
import SwiftUI
import TimelordPresentation

struct IPSHistoryChart: View {
    let history: ChainHistory
    let range: HistoryRange
    let now: Date
    let upperBound: Double
    var compact = false
    @State private var hovered: ProofObservation?

    private var window: DateInterval { range.interval(endingAt: now) }
    private var points: [PlotPoint] { history.plotPoints(in: window) }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            if compact {
                chart.chartXAxis(.hidden).chartYAxis(.hidden)
            } else {
                chart
                    .chartXAxis {
                        // Interior ticks keep long localized time labels away from
                        // the clipped edges of the plot.
                        AxisMarks(values: (1...3).map { window.start.addingTimeInterval(window.duration * Double($0) / 4) }) { _ in
                            AxisGridLine(); AxisValueLabel(format: .dateTime.hour().minute())
                        }
                    }
                    .chartYAxis {
                        AxisMarks(position: .leading, values: (0...4).map { upperBound * Double($0) / 4 }) { value in
                            AxisGridLine()
                            AxisValueLabel { if let ips = value.as(Double.self) { Text(DisplayFormat.ips(ips)) } }
                        }
                    }
            }
        }
        .overlay {
            if points.isEmpty {
                Text("Waiting for verified proofs").font(.caption).foregroundStyle(.secondary)
                    .padding(8).background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(history.chain.title) IPS over \(range.title)")
        .accessibilityValue("Average \(DisplayFormat.ips(history.statistics(in: window).average)); \(Int(history.statistics(in: window).coverage * 100)) percent telemetry coverage")
    }

    private var chart: some View {
        Chart {
            ForEach(history.events(in: window).filter { $0.kind.interruptsWork && $0.end != nil }) { event in
                RectangleMark(xStart: .value("Start", max(event.date, window.start)), xEnd: .value("End", min(event.end!, window.end)), yStart: .value("Bottom", 0), yEnd: .value("Top", upperBound))
                    .foregroundStyle(Color.secondary.opacity(0.08))
            }
            ForEach(points) { point in
                // Separate series prevent a line being drawn across missing capture
                // or a known interruption. Recovery observations stay in their series.
                LineMark(x: .value("Time", point.date), y: .value("IPS", point.ips), series: .value("Segment", point.segment))
                    .foregroundStyle(history.chain.color)
                    .lineStyle(StrokeStyle(lineWidth: compact ? 1.6 : 2))
                    .interpolationMethod(.stepEnd)
            }
            if !compact {
                ForEach(history.events(in: window).filter { $0.kind == .recovery || $0.kind == .profileChange }) { event in
                    RuleMark(x: .value("Event", event.date)).foregroundStyle(Color.orange.opacity(0.6)).lineStyle(StrokeStyle(lineWidth: 1, dash: [3, 3]))
                }
            }
            if let hovered {
                RuleMark(x: .value("Selected time", hovered.date)).foregroundStyle(Color.secondary.opacity(0.5))
                PointMark(x: .value("Time", hovered.date), y: .value("IPS", hovered.ips))
                    .foregroundStyle(history.chain.color)
                    .annotation(position: .top, spacing: 6, overflowResolution: .init(x: .fit(to: .chart), y: .disabled)) {
                        VStack(alignment: .leading, spacing: 3) {
                            Text(hovered.date, format: .dateTime.hour().minute().second()).font(.caption2)
                            Text("\(Int(hovered.ips).formatted()) IPS").font(.caption.weight(.semibold))
                        }
                        .monospacedDigit().padding(8)
                        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
                        .shadow(color: .black.opacity(0.12), radius: 3, y: 2)
                        .fixedSize().allowsHitTesting(false)
                    }
            }
        }
        .chartXScale(domain: window.start...window.end)
        .chartYScale(domain: 0...upperBound)
        .chartLegend(.hidden)
        .chartOverlay { proxy in
            GeometryReader { geometry in
                Rectangle().fill(.clear).contentShape(Rectangle())
                    .onContinuousHover { phase in
                        switch phase {
                        case .active(let location):
                            guard let plotFrame = proxy.plotFrame else { hovered = nil; return }
                            let frame = geometry[plotFrame]
                            guard frame.contains(location), let date: Date = proxy.value(atX: location.x - frame.minX) else { hovered = nil; return }
                            hovered = history.observation(near: date, in: window)
                        case .ended: hovered = nil
                        }
                    }
            }
        }
    }
}
