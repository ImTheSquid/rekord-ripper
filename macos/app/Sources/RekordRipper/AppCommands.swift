import AppKit
import SwiftUI

/// Menu commands, with shortcuts that follow the TUI's keys behind ⌘.
struct AppCommands: Commands {
    let model: SessionModel

    var body: some Commands {
        CommandGroup(after: .sidebar) {
            Divider()
            ForEach(Array(Screen.allCases.enumerated()), id: \.element) { i, screen in
                Button(screen.rawValue) { model.screen = screen }
                    .keyboardShortcut(KeyEquivalent(Character("\(i + 1)")), modifiers: .command)
            }
        }

        CommandMenu("Library") {
            Button("Reload") { model.reload() }
                .keyboardShortcut("r", modifiers: .command)
            Divider()
            Button("Review Transfer…") { model.reviewTransfer() }
                .keyboardShortcut(.return, modifiers: .command)
                .disabled(model.srcSelection == nil || model.dstSelection.isEmpty)
            Button("Shop for Source") { model.shopForSource() }
                .keyboardShortcut("s", modifiers: [.command, .shift])
                .disabled(model.srcSelection == nil)
            Button("Clear Destinations") { model.dstSelection.removeAll() }
                .keyboardShortcut("c", modifiers: [.command, .shift])
            Divider()
            Toggle("Unanalysed Destinations Only", isOn: Binding(
                get: { model.autoFilter }, set: { model.autoFilter = $0 }))
                .keyboardShortcut("a", modifiers: [.command, .shift])
            Toggle("Destinations Matching Source", isOn: Binding(
                get: { model.fuzzyFromSrc }, set: { model.fuzzyFromSrc = $0 }))
                .keyboardShortcut("f", modifiers: [.command, .shift])
            Toggle("Replace Existing Cues", isOn: Binding(
                get: { model.copyReplace }, set: { model.setCopyOptions(replace: $0, lock: model.copyLock) }))
                .keyboardShortcut("r", modifiers: [.command, .shift])
            Toggle("Lock After Copying", isOn: Binding(
                get: { model.copyLock }, set: { model.setCopyOptions(replace: model.copyReplace, lock: $0) }))
                .keyboardShortcut("l", modifiers: [.command, .shift])
            Divider()
            Toggle("Allow Writes While Rekordbox Is Running", isOn: Binding(
                get: { model.bypassRekordboxCheck }, set: { setBypass($0) }))
        }

        CommandGroup(replacing: .help) {
            Button("Search Syntax") { showSearchHelp() }
                .keyboardShortcut("?", modifiers: .command)
        }
    }

    /// Off by default and never remembered: the GUI equivalent of the CLI's
    /// deliberately unwieldy override flag.
    @MainActor
    private func setBypass(_ on: Bool) {
        guard on else { return model.setBypassRekordboxCheck(false) }
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Write to master.db while rekordbox is open?"
        alert.informativeText = "Rekordbox keeps its own copy of the database in memory and may overwrite "
            + "or corrupt these changes. This lasts until you quit Rekord Ripper."
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Allow Writes")
        if alert.runModal() == .alertSecondButtonReturn {
            model.setBypassRekordboxCheck(true)
        }
    }

    @MainActor
    private func showSearchHelp() {
        let alert = NSAlert()
        alert.messageText = "Search Syntax"
        alert.informativeText = """
        burial untrue            both words, in any order
        "burial untrue"          adjacent, in that order
        burial -remix            excludes remix
        burial OR zomby          either one
        p:"jack night"           tracks in a playlist or folder
        is:stream  is:missing    where the audio lives
        is:lossless  type:flac   what you have
        has:cues  is:locked      what the track carries
        bpm:120-130  len:>6m     numbers, comparisons and spans

        Every term matches as a substring. A numeric query is an exact track ID.
        """
        alert.runModal()
    }
}
