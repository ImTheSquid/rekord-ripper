import AppKit
import SwiftUI

/// Edits `config.toml` through the core, which saves each change as it is made
/// and picks up edits made to the file directly.
struct SettingsView: View {
    var body: some View {
        TabView {
            GeneralSettings()
                .tabItem { Label("General", systemImage: "gearshape") }
            SettingsPage(groups: ["Search"])
                .tabItem { Label("Search", systemImage: "magnifyingglass") }
            SettingsPage(groups: ["Bandcamp", "SoundCloud", "Soulseek"],
                         footer: "Keys, passwords and cookies are saved to credentials.toml, readable only by you, "
                             + "and are never shown again once saved.")
                .tabItem { Label("Sources", systemImage: "network") }
            SettingsPage(groups: ["Fingerprint Gate"],
                         footer: "These decide whether a download is the same recording as your track. "
                             + "Loosening them lets a different recording pass the check.")
                .tabItem { Label("Matching", systemImage: "waveform") }
        }
        .frame(width: 560, height: 540)
    }
}

private struct GeneralSettings: View {
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
                ForEach(model.settings.filter { $0.group == "Downloads" }) { SettingRow(setting: $0) }
            } header: {
                Text("Downloads")
            } footer: {
                Text("Each download is imported and its fingerprint checked without a click. "
                    + "Anything that writes to your library still asks first.")
                    .foregroundStyle(.secondary)
            }
            Section {
                ForEach(model.settings.filter { $0.group == "Tools" }) { SettingRow(setting: $0) }
            } header: {
                Text("Tools")
            } footer: {
                Text("Found in your PATH, Homebrew and Nix folders unless given here.")
                    .foregroundStyle(.secondary)
            }
            Section {
                LabeledContent("Saved to") {
                    Button("Open config.toml") { openConfig() }
                }
            } footer: {
                Text("Changes are saved as you make them, and edits made to the file apply straight away.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
    }

    private func openConfig() {
        let url = URL(fileURLWithPath: model.configPath)
        if !NSWorkspace.shared.open(url) {
            NSWorkspace.shared.activateFileViewerSelecting([url.deletingLastPathComponent()])
        }
    }
}

private struct SettingsPage: View {
    @Environment(SessionModel.self) private var model
    let groups: [String]
    var footer: String?

    var body: some View {
        Form {
            ForEach(groups, id: \.self) { group in
                Section {
                    ForEach(model.settings.filter { $0.group == group }) { SettingRow(setting: $0) }
                    ForEach(model.credentials.filter { $0.group == group }) { CredentialRow(credential: $0) }
                } header: {
                    Text(group)
                } footer: {
                    if group == groups.last, let footer {
                        Text(footer).foregroundStyle(.secondary)
                    }
                }
            }
        }
        .formStyle(.grouped)
    }
}

private struct SettingRow: View {
    @Environment(SessionModel.self) private var model
    let setting: Setting

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            control
            if let error = model.settingError, error.key == setting.id {
                Text(error.message).font(.caption).foregroundStyle(.red)
            } else if !setting.help.isEmpty {
                Text(setting.help).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder private var control: some View {
        switch setting.kind {
        case .bool:
            Toggle(setting.label, isOn: Binding(get: { setting.value == "true" }, set: set))
        case .integer:
            SettingField(setting: setting) {
                Stepper("", value: Binding(get: { Double(setting.value) ?? setting.range.lowerBound },
                                           set: { model.setSetting(setting.id, String(Int($0))) }),
                        in: setting.range)
                    .labelsHidden()
            }
        case .path:
            SettingField(setting: setting) {
                Button("Choose…") { choose() }
            }
        case .number, .text, .list:
            SettingField(setting: setting) { EmptyView() }
        case .choice:
            Picker(setting.label, selection: Binding(get: { setting.value },
                                                     set: { model.setSetting(setting.id, $0) })) {
                ForEach(setting.choices, id: \.value) { Text($0.label).tag($0.value) }
                // A value written into the file by hand, such as a profile.
                if !setting.choices.contains(where: { $0.value == setting.value }) {
                    Text(setting.value).tag(setting.value)
                }
            }
        }
    }

    private func set(_ on: Bool) {
        if on, setting.id == "import.insert_content_rows", !confirmImporting() { return }
        model.setSetting(setting.id, on ? "true" : "false")
    }

    private func choose() {
        let panel = NSOpenPanel()
        let folder = setting.id.hasSuffix("_dir")
        panel.canChooseDirectories = folder
        panel.canChooseFiles = !folder
        panel.canCreateDirectories = folder
        if panel.runModal() == .OK, let url = panel.url {
            model.setSetting(setting.id, url.path)
        }
    }
}

/// Write-only for a secret: the field starts empty, and what is typed is sent
/// once and dropped.
private struct CredentialRow: View {
    @Environment(SessionModel.self) private var model
    let credential: CredentialSetting
    @State private var draft = ""
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            LabeledContent(credential.label) {
                HStack {
                    field
                        .labelsHidden()
                        .focused($focused)
                        .onSubmit(save)
                    if credential.secret, credential.set, !credential.overridden {
                        Button("Clear") { model.setCredential(credential.id, "") }
                    }
                }
            }
            if let error = model.settingError, error.key == credential.id {
                Text(error.message).font(.caption).foregroundStyle(.red)
            } else if !credential.note.isEmpty {
                Text(credential.note).font(.caption)
                    .foregroundStyle(credential.overridden ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
            }
        }
        .onAppear { if !credential.secret { draft = credential.value } }
        .onChange(of: credential.value) { _, value in
            if !credential.secret, !focused { draft = value }
        }
        .onChange(of: focused) { _, now in
            if !now, !credential.secret { save() }
        }
    }

    @ViewBuilder private var field: some View {
        if credential.secret {
            SecureField(credential.label, text: $draft,
                        prompt: Text(credential.set ? "saved — type to replace" : "not set"))
        } else {
            TextField(credential.label, text: $draft, prompt: Text("not set"))
        }
    }

    private func save() {
        if credential.secret {
            // Empty means nothing typed here; Clear removes a saved one.
            guard !draft.isEmpty else { return }
            model.setCredential(credential.id, draft)
            draft = ""
        } else if draft != credential.value {
            model.setCredential(credential.id, draft)
        }
    }
}

/// Writes rows into a database rekordbox owns, so turning it on is asked.
@MainActor
private func confirmImporting() -> Bool {
    let alert = NSAlert()
    alert.alertStyle = .warning
    alert.messageText = "Import downloads into rekordbox?"
    alert.informativeText = "Rekord Ripper will add each download to master.db itself, after taking a backup "
        + "and only while rekordbox is closed. Each import still asks first."
    alert.addButton(withTitle: "Cancel")
    alert.addButton(withTitle: "Turn On")
    return alert.runModal() == .alertSecondButtonReturn
}

/// A text field that saves on Return or on leaving it, and follows the file
/// while it isn't being edited.
private struct SettingField<Accessory: View>: View {
    @Environment(SessionModel.self) private var model
    let setting: Setting
    @ViewBuilder var accessory: Accessory
    @State private var draft = ""
    @FocusState private var focused: Bool

    var body: some View {
        LabeledContent(setting.label) {
            HStack {
                TextField(setting.label, text: $draft, prompt: Text(prompt))
                    .labelsHidden()
                    .multilineTextAlignment(numeric ? .trailing : .leading)
                    .frame(maxWidth: numeric ? 80 : .infinity)
                    .focused($focused)
                    .onSubmit(commit)
                accessory
            }
        }
        .onAppear { draft = setting.value }
        .onChange(of: setting.value) { _, value in
            if !focused { draft = value }
        }
        .onChange(of: focused) { _, now in
            if !now { commit() }
        }
    }

    private var numeric: Bool {
        setting.kind == .integer || setting.kind == .number
    }

    private var prompt: String {
        switch setting.kind {
        case .list: "comma-separated"
        case .path, .text: "not set"
        default: ""
        }
    }

    private func commit() {
        if draft != setting.value { model.setSetting(setting.id, draft) }
    }
}
