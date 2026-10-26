import AppKit
import SwiftUI

@main
struct RekordRipperApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate
    @State private var model = SessionModel()

    var body: some Scene {
        Window("Rekord Ripper", id: "main") {
            ContentView()
                .environment(model)
                .onAppear { delegate.model = model }
        }
        .defaultSize(width: 1280, height: 800)
        .commands { AppCommands(model: model) }
    }
}

/// Asks before a quit would throw work away, as the TUI's second `q` does.
final class AppDelegate: NSObject, NSApplicationDelegate {
    @MainActor var model: SessionModel?

    func applicationShouldTerminateAfterLastWindowClosed(_: NSApplication) -> Bool { true }

    @MainActor
    func applicationShouldTerminate(_: NSApplication) -> NSApplication.TerminateReply {
        guard let warning = model?.quitWarning else { return .terminateNow }
        let alert = NSAlert()
        alert.messageText = "Quit Rekord Ripper?"
        alert.informativeText = "\(warning.prefix(1).uppercased())\(warning.dropFirst())."
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Quit")
        return alert.runModal() == .alertSecondButtonReturn ? .terminateNow : .terminateCancel
    }
}

struct ContentView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        if let error = model.openError {
            ContentUnavailableView {
                Label("Could not open your rekordbox library", systemImage: "exclamationmark.triangle")
            } description: {
                Text(error).textSelection(.enabled)
            }
            .frame(minWidth: 520, minHeight: 320)
        } else {
            MainView()
        }
    }
}

struct MainView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            List(Screen.allCases, selection: $model.screen) { screen in
                Label(screen.rawValue, systemImage: screen.symbol)
                    .badge(badge(for: screen))
                    .tag(screen)
            }
            .navigationSplitViewColumnWidth(min: 150, ideal: 170)
        } detail: {
            VStack(spacing: 0) {
                if model.rekordboxRunning {
                    RekordboxBanner()
                }
                switch model.screen {
                case .transfer: TransferView()
                case .shop: ShopView()
                case .queue: QueueView()
                }
                Divider()
                StatusBar()
            }
        }
        .sheet(item: Binding(get: { model.confirmation }, set: { if $0 == nil { model.cancelConfirmation() } })) {
            ConfirmSheet(confirmation: $0)
        }
    }

    private func badge(for screen: Screen) -> Int {
        switch screen {
        case .transfer: 0
        case .shop: Int(model.shopStatus.searchesOutstanding) + model.fetchQueue.count
        case .queue: model.queueEntries.count
        }
    }
}

struct RekordboxBanner: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        HStack {
            Image(systemName: "exclamationmark.triangle.fill")
            Text(model.bypassRekordboxCheck
                ? "Rekordbox is running and the safety check is bypassed. Writes may corrupt your library."
                : "Rekordbox is running. Quit it before applying anything; writes are refused until then.")
            Spacer()
        }
        .font(.callout)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .foregroundStyle(.white)
        .background(model.bypassRekordboxCheck ? Color.red : Color.orange)
    }
}

struct StatusBar: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        HStack(spacing: 8) {
            if model.shopStatus.searchesOutstanding > 0 || model.fetchStatus.kind == .running {
                ProgressView().controlSize(.small)
            }
            Text(model.status.text)
                .foregroundStyle(model.status.tone.color)
                .lineLimit(2)
                .textSelection(.enabled)
            Spacer()
            Text("\(model.tracks.count) tracks")
                .foregroundStyle(.secondary)
        }
        .font(.callout)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }
}

struct ConfirmSheet: View {
    @Environment(SessionModel.self) private var model
    let confirmation: Confirmation

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label(confirmation.title, systemImage: confirmation.isDestructive ? "exclamationmark.octagon" : "checkmark.shield")
                .font(.headline)
                .foregroundStyle(confirmation.isDestructive ? .red : .primary)
            ScrollView {
                VStack(alignment: .leading, spacing: 2) {
                    ForEach(Array(confirmation.lines.enumerated()), id: \.offset) { _, line in
                        Text(line.text)
                            .font(.system(.body, design: .monospaced))
                            .fontWeight(line.tone == .strong ? .semibold : .regular)
                            .foregroundStyle(line.tone.color)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                .textSelection(.enabled)
                .padding(8)
            }
            .background(.background.secondary, in: .rect(cornerRadius: 6))
            if model.status.level == .error {
                Text(model.status.text).foregroundStyle(.red).font(.callout)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { model.cancelConfirmation() }
                    .keyboardShortcut(.cancelAction)
                Button(confirmation.accept, role: confirmation.isDestructive ? .destructive : nil) {
                    model.accept()
                }
                .keyboardShortcut(confirmation.isDestructive ? nil : .defaultAction)
            }
        }
        .padding()
        .frame(minWidth: 620, idealWidth: 720, minHeight: 360, idealHeight: 520)
    }
}
