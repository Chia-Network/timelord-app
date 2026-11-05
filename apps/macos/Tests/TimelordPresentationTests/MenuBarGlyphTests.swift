import XCTest
@testable import TimelordPresentation

final class MenuBarGlyphTests: XCTestCase {
    func testPhasesMapToMenuBarGlyphs() {
        XCTAssertEqual(MenuBarGlyph(phase: .running), .running)
        XCTAssertEqual(MenuBarGlyph(phase: .paused), .paused)
        for phase: AppPhase in [.setupRequired, .starting, .restarting, .pausing, .waitingForNode, .waitingForProofs] {
            XCTAssertEqual(MenuBarGlyph(phase: phase), .waiting, phase.rawValue)
        }
        for phase: AppPhase in [.nodeUnreachable, .boardMissing, .temperatureLimit, .serviceUnavailable] {
            XCTAssertEqual(MenuBarGlyph(phase: phase), .attention, phase.rawValue)
        }
    }

    func testRetryingCleanupFailureNeedsAttention() {
        for operation: ServiceControlOperation in [.pause, .reload] {
            XCTAssertEqual(MenuBarGlyph(snapshot: LivePresentation.snapshot(status: nil, controlOperation: operation)), .waiting)
            XCTAssertEqual(MenuBarGlyph(snapshot: LivePresentation.snapshot(status: nil, controlOperation: operation, error: "cleanup failed")), .attention)
        }
    }
}
