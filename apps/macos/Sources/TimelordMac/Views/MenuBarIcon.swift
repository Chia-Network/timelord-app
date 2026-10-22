import AppKit
import TimelordPresentation

/// An 18 pt template image of a chip whose face shows the service state: clock hands while running,
/// dots while waiting, pause bars, or a badge when something needs attention. Coordinates are on a
/// 36-unit (@2x) grid, the same as the app icon's clock face.
enum MenuBarIcon {
    static func image(for glyph: MenuBarGlyph) -> NSImage {
        let image = NSImage(size: NSSize(width: 18, height: 18), flipped: true) { _ in
            let scale = NSAffineTransform()
            scale.scale(by: 0.5)
            scale.concat()
            NSColor.black.set()
            stroke(chip(), width: 3)
            switch glyph {
            case .running: drawHands()
            case .waiting: for x in [13.4, 18, 22.6] as [CGFloat] { fillCircle(x: x, y: 18, radius: 1.9) }
            case .paused:
                NSBezierPath(roundedRect: NSRect(x: 13.6, y: 13, width: 3, height: 10), xRadius: 1.2, yRadius: 1.2).fill()
                NSBezierPath(roundedRect: NSRect(x: 19.4, y: 13, width: 3, height: 10), xRadius: 1.2, yRadius: 1.2).fill()
            case .attention:
                drawHands()
                // Clear a gap around the badge so it reads as separate from the chip outline.
                NSGraphicsContext.current?.compositingOperation = .clear
                fillCircle(x: 28.5, y: 7.5, radius: 7)
                NSGraphicsContext.current?.compositingOperation = .sourceOver
                fillCircle(x: 28.5, y: 7.5, radius: 4.6)
            }
            return true
        }
        image.isTemplate = true
        image.accessibilityDescription = "Timelord"
        return image
    }

    private static func chip() -> NSBezierPath {
        let path = NSBezierPath(roundedRect: NSRect(x: 8.5, y: 8.5, width: 19, height: 19), xRadius: 4.5, yRadius: 4.5)
        for offset in [14.5, 21.5] as [CGFloat] {
            path.move(to: NSPoint(x: offset, y: 3)); path.line(to: NSPoint(x: offset, y: 7))
            path.move(to: NSPoint(x: offset, y: 29)); path.line(to: NSPoint(x: offset, y: 33))
            path.move(to: NSPoint(x: 3, y: offset)); path.line(to: NSPoint(x: 7, y: offset))
            path.move(to: NSPoint(x: 29, y: offset)); path.line(to: NSPoint(x: 33, y: offset))
        }
        return path
    }

    private static func drawHands() {
        let hands = NSBezierPath()
        hands.move(to: NSPoint(x: 14.2, y: 14.4)); hands.line(to: NSPoint(x: 18, y: 18)); hands.line(to: NSPoint(x: 22.4, y: 13.8))
        stroke(hands, width: 2.6)
        fillCircle(x: 18, y: 18, radius: 1.9)
    }

    private static func stroke(_ path: NSBezierPath, width: CGFloat) {
        path.lineWidth = width; path.lineCapStyle = .round; path.lineJoinStyle = .round; path.stroke()
    }

    private static func fillCircle(x: CGFloat, y: CGFloat, radius: CGFloat) {
        NSBezierPath(ovalIn: NSRect(x: x - radius, y: y - radius, width: radius * 2, height: radius * 2)).fill()
    }
}
