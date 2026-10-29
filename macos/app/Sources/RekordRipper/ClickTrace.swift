import AppKit
import os

/// Traces clicks and core calls to the unified log, when turned on with
/// `defaults write com.github.imthesquid.rekord-ripper DebugClicks -bool true`.
/// Read with `/usr/bin/log show --last 5m --predicate 'subsystem == "rekord-ripper"'`.
enum ClickTrace {
    static let on = UserDefaults.standard.bool(forKey: "DebugClicks")
    private static let log = Logger(subsystem: "rekord-ripper", category: "clicks")

    @MainActor
    static func install() {
        guard on else { return }
        note("tracing clicks")
        NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .leftMouseUp]) { e in
            guard let window = e.window else { return e }
            let p = e.locationInWindow
            let hit = window.contentView?.hitTest(p)
            let chain = sequence(first: hit, next: { $0?.superview }).prefix(4)
                .map { $0.map { String(describing: type(of: $0)) } ?? "nil" }.joined(separator: " < ")
            note("\(e.type == .leftMouseDown ? "down" : "up") at \(String(format: "%.0f,%.0f", p.x, p.y)) -> \(chain)")
            return e
        }
    }

    static func note(_ text: String) {
        guard on else { return }
        log.notice("\(text, privacy: .public)")
    }
}
