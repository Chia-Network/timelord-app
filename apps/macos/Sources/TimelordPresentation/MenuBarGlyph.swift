/// The menu bar icon's state. Menu bar icons are monochrome template images, so each state is a different shape.
public enum MenuBarGlyph: Sendable, Equatable {
    case running, waiting, paused, attention

    public init(phase: AppPhase) {
        switch phase {
        case .running: self = .running
        case .paused: self = .paused
        case .setupRequired, .starting, .waitingForNode, .waitingForProofs: self = .waiting
        case .nodeUnreachable, .boardMissing, .temperatureLimit, .serviceUnavailable: self = .attention
        }
    }
}
