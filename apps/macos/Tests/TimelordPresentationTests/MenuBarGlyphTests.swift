import XCTest
@testable import TimelordPresentation

final class MenuBarGlyphTests: XCTestCase {
    func testPhasesMapToMenuBarGlyphs() {
        XCTAssertEqual(MenuBarGlyph(phase: .running), .running)
        XCTAssertEqual(MenuBarGlyph(phase: .paused), .paused)
        for phase: AppPhase in [.setupRequired, .starting, .waitingForNode, .waitingForProofs] {
            XCTAssertEqual(MenuBarGlyph(phase: phase), .waiting, phase.rawValue)
        }
        for phase: AppPhase in [.nodeUnreachable, .boardMissing, .temperatureLimit, .serviceUnavailable] {
            XCTAssertEqual(MenuBarGlyph(phase: phase), .attention, phase.rawValue)
        }
    }
}
