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
        .defaultSize(width: 1380, height: 820)
        .commands { AppCommands(model: model) }

        Settings {
            SettingsView().environment(model)
        }
    }
}

/// Asks before a quit would throw work away, as the TUI's second `q` does.
final class AppDelegate: NSObject, NSApplicationDelegate {
    @MainActor var model: SessionModel?

    @MainActor
    func applicationDidFinishLaunching(_: Notification) {
        ClickTrace.install()
    }

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
            SidebarView()
                .navigationSplitViewColumnWidth(min: 180, ideal: 220)
        } detail: {
            VStack(spacing: 0) {
                if model.rekordboxRunning {
                    RekordboxBanner()
                }
                LibraryTable()
            }
            .overlay(alignment: .bottom) { ToastView() }
            .inspector(isPresented: $model.inspectorShown) {
                InspectorView()
                    .inspectorColumnWidth(min: 300, ideal: 360, max: 520)
            }
        }
        .navigationTitle(title)
        .navigationSubtitle("\(model.visibleTracks.count) tracks")
        .searchable(text: $model.query, placement: .toolbar, prompt: "Search")
        .searchSuggestions {
            ForEach(SearchHint.completing(model.query), id: \.self) { hint in
                Text(hint.term).searchCompletion(hint.query)
            }
        }
        .toolbar {
            ToolbarItem {
                ActivityButton()
            }
            ToolbarItem {
                Button { model.inspectorShown.toggle() } label: {
                    Label("Inspector", systemImage: "sidebar.trailing")
                }
                .help("Show or hide the inspector")
            }
        }
        .sheet(item: Binding(get: { model.presentedConfirmation }, set: { if $0 == nil { model.presented = nil } })) {
            ReviewSheet(confirmation: $0)
        }
    }

    private var title: String {
        switch model.scope {
        case let .collection(code):
            model.collections.first { $0.code == code }?.title ?? "Library"
        case let .playlist(id):
            PlaylistNode.find(id, in: model.playlists)?.name ?? "Playlist"
        }
    }
}

struct SearchHint: Hashable {
    let term: String
    /// The whole query with the term completed.
    let query: String

    private static let terms = [
        "is:lossless", "is:lossy", "is:stream", "is:cloud", "is:local", "is:missing", "is:locked",
        "has:cues", "type:flac", "type:mp3", "bpm:120-130", "len:>6m", "p:",
    ]

    /// Terms the word being typed could become; none until it is typed.
    static func completing(_ query: String) -> [SearchHint] {
        let word = query.split(separator: " ", omittingEmptySubsequences: false).last.map(String.init) ?? ""
        let bare = word.hasPrefix("-") ? String(word.dropFirst()) : word
        guard !bare.isEmpty else { return [] }
        let head = query.dropLast(word.count)
        let sign = word.hasPrefix("-") ? "-" : ""
        return terms
            .filter { $0.hasPrefix(bare.lowercased()) && $0 != bare.lowercased() }
            .map { SearchHint(term: sign + $0, query: head + sign + $0 + ($0.hasSuffix(":") ? "" : " ")) }
    }
}

extension PlaylistNode {
    static func find(_ id: String, in nodes: [PlaylistNode]) -> PlaylistNode? {
        for n in nodes {
            if n.id == id { return n }
            if let hit = find(id, in: n.children ?? []) { return hit }
        }
        return nil
    }
}

struct SidebarView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        // Built once the library has loaded: filling an empty outline with
        // every row at once trips AppKit's reentrancy warning.
        if model.collections.isEmpty {
            ProgressView().controlSize(.small).frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            list
        }
    }

    private var list: some View {
        List(selection: Binding(get: { model.scope }, set: { if let s = $0 { model.scope = s } })) {
            Section("Library") {
                ForEach(model.collections) { c in
                    Label(c.title, systemImage: c.symbol)
                        .badge(Int(c.count))
                        .tag(Scope.collection(c.code))
                }
            }
            Section("Playlists") {
                OutlineGroup(model.playlists, children: \.children) { node in
                    Label(node.name, systemImage: node.folder ? "folder" : node.smart ? "gearshape" : "music.note.list")
                        .tag(Scope.playlist(node.id))
                }
            }
        }
        .listStyle(.sidebar)
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
