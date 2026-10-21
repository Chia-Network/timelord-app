import SwiftUI
import TimelordPresentation

extension StatusTone {
    var color: Color { switch self { case .neutral: return .secondary; case .positive: return .green; case .attention: return .orange; case .critical: return .red } }
}

extension Chain {
    var color: Color { switch self { case .challenge: return .accentColor; case .reward: return .teal; case .infusedChallenge: return .purple } }
}

extension View {
    func timelordCard(padding: CGFloat = 20) -> some View {
        self.padding(padding)
            .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 16))
            .overlay(RoundedRectangle(cornerRadius: 16).strokeBorder(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5))
    }
}

struct StatusBadge: View {
    let title: String
    let tone: StatusTone
    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: "circle.fill").font(.system(size: 7)).foregroundStyle(tone.color)
            Text(title).foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }
}

struct MetricTile: View {
    let title: String
    let value: String
    let caption: String
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.subheadline).foregroundStyle(.secondary)
            Text(value).font(.system(size: 29, weight: .medium)).monospacedDigit()
            Text(caption).font(.caption).foregroundStyle(.secondary).lineLimit(2)
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct SheetHeader: View {
    let title: String
    @Environment(\.dismiss) private var dismiss
    var body: some View {
        HStack {
            Text(title).font(.title2.weight(.semibold))
            Spacer()
            Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
        }.padding(24)
    }
}

/// Configuration sheets commit with the visible primary action; Done never silently discards edits.
struct SettingsSheetHeader: View {
    let title: String
    @ObservedObject var model: TimelordModel
    @Environment(\.dismiss) private var dismiss
    @State private var finishing = false
    var body: some View {
        HStack {
            Text(title).font(.title2.weight(.semibold))
            Spacer()
            Button(model.settingsDoneTitle) {
                finishing = true
                Task {
                    if await model.finishSettings() { dismiss() }
                    finishing = false
                }
            }.keyboardShortcut(.defaultAction)
                .disabled(finishing || !model.canFinishSettings)
        }.padding(24)
    }
}
