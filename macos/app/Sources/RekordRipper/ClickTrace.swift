import AppKit
import SwiftUI
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
        Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { _ in
            MainActor.assumeIsolated {
                guard let root = NSApp.windows.first(where: { $0.isVisible && $0.contentView != nil })?.contentView else { return }
                note("controls: " + controls(in: root, near: nil).joined(separator: "; "))
            }
        }
        NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .leftMouseUp]) { e in
            guard let window = e.window else { return e }
            let p = e.locationInWindow
            let hit = window.contentView?.hitTest(p)
            let chain = sequence(first: hit, next: { $0?.superview }).prefix(4)
                .map { $0.map { String(describing: type(of: $0)) } ?? "nil" }.joined(separator: " < ")
            note("\(e.type == .leftMouseDown ? "down" : "up") at \(String(format: "%.0f,%.0f", p.x, p.y)) -> \(chain)")
            if e.type == .leftMouseDown, let root = window.contentView {
                note("  near: " + controls(in: root, near: p).joined(separator: "; "))
            }
            return e
        }
    }

    /// Buttons and spinners within reach of `p`, with their frames in window coordinates.
    @MainActor
    private static func controls(in view: NSView, near p: NSPoint?) -> [String] {
        var out: [String] = []
        let name = String(describing: type(of: view))
        if name.contains("Button") || view is NSProgressIndicator {
            let f = view.convert(view.bounds, to: nil)
            // Near the click, or with no click, every SwiftUI button.
            let wanted = p.map { abs(f.midY - $0.y) < 120 && abs(f.midX - $0.x) < 300 }
                ?? name.hasPrefix("SwiftUIAppKitButton")
            if wanted {
                let title = (view as? NSButton)?.title ?? ""
                out.append("\(name.prefix(24)) '\(title)' x \(Int(f.minX))-\(Int(f.maxX)) y \(Int(f.minY))-\(Int(f.maxY))")
            }
        }
        for sub in view.subviews { out += controls(in: sub, near: p) }
        return out
    }

    static func note(_ text: String) {
        guard on else { return }
        log.notice("\(text, privacy: .public)")
    }
}

extension View {
    /// Logs what made SwiftUI re-evaluate this body, when tracing.
    static func traceChanges() {
        if ClickTrace.on, #available(macOS 14.1, *) { Self._logChanges() }
    }
}
