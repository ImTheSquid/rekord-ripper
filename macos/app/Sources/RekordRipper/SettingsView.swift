import AppKit
import SwiftUI

struct SettingsView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        Form {
            Section("Copying Analysis") {
                Toggle("Replace existing cues", isOn: Binding(
                    get: { model.copyReplace }, set: { model.setCopyOptions(replace: $0, lock: model.copyLock) }))
                Toggle("Lock analysis after copying", isOn: Binding(
                    get: { model.copyLock }, set: { model.setCopyOptions(replace: model.copyReplace, lock: $0) }))
            }
            Section {
                Toggle("Move downloads along automatically", isOn: Binding(
                    get: { model.autoAdvance }, set: { model.setAutoAdvance($0) }))
                if !model.importEnabled {
                    Label {
                        VStack(alignment: .leading, spacing: 6) {
                            Text("Importing is off in config.toml, so downloads stop before they become rekordbox tracks.")
                            Text("Set insert_content_rows = true under [import] to turn it on.")
                                .foregroundStyle(.secondary)
                            Button("Open config.toml") { openConfig() }
                        }
                    } icon: {
                        Image(systemName: "exclamationmark.triangle").foregroundStyle(.orange)
                    }
                }
            } header: {
                Text("Downloads")
            } footer: {
                Text("Each download is imported and its fingerprint checked without a click. Anything that writes to your library still asks first.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .frame(width: 480)
        .fixedSize(horizontal: false, vertical: true)
    }

    private func openConfig() {
        let url = URL(fileURLWithPath: model.configPath)
        if !NSWorkspace.shared.open(url) {
            NSWorkspace.shared.activateFileViewerSelecting([url.deletingLastPathComponent()])
        }
    }
}
