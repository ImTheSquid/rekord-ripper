import SwiftUI

/// Downloads that have not become transfers yet.
struct QueueView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Group {
            if model.queueEntries.isEmpty {
                ContentUnavailableView("The queue is empty", systemImage: "tray",
                    description: Text("Downloads from the Shop land here until their analysis is transferred."))
            } else {
                List(selection: $model.queueSelection) {
                    ForEach(model.queueEntries) { entry in
                        QueueRow(entry: entry).tag(entry.id)
                    }
                }
                .contextMenu(forSelectionType: QueueEntry.ID.self) { ids in
                    if let id = ids.first {
                        Button("Retry") { model.retry(id) }
                        Button("Force Apply Without Fingerprint…") { model.forceApply(id) }
                        Divider()
                        Button("Forget", role: .destructive) { model.forget(id) }
                    }
                }
            }
        }
        .onAppear { model.reloadQueue() }
        .toolbar { QueueToolbar() }
    }
}

struct QueueRow: View {
    let entry: QueueEntry

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text("#\(entry.id)").foregroundStyle(.secondary).monospacedDigit()
                Text(entry.title.isEmpty ? "(untitled)" : entry.title).fontWeight(.semibold)
                Text("— \(entry.artist.isEmpty ? "—" : entry.artist)").foregroundStyle(.secondary)
            }
            HStack(spacing: 6) {
                if entry.inFlight { ProgressView().controlSize(.mini) }
                Text(entry.state).foregroundStyle(entry.tone.color)
                Text("· \(entry.fileName)").foregroundStyle(.tertiary).lineLimit(1)
            }
            .font(.callout)
        }
        .padding(.vertical, 2)
    }
}

struct QueueToolbar: ToolbarContent {
    @Environment(SessionModel.self) private var model

    var body: some ToolbarContent {
        ToolbarItemGroup {
            Button { model.reloadQueue() } label: { Label("Refresh", systemImage: "arrow.clockwise") }
                .help("Re-read the queue, retiring entries whose file or track is gone (R)")
            Button { model.importDownloads() } label: { Label("Import", systemImage: "square.and.arrow.down") }
                .help("Create rekordbox rows for downloads that have none (i)")
            Button { model.checkAndApply() } label: { Label("Check & Apply", systemImage: "waveform.badge.checkmark") }
                .help("Fingerprint each imported download against its source, then transfer what matches (a)")
        }
        ToolbarItemGroup {
            Button { if let id = model.queueSelection { model.retry(id) } } label: {
                Label("Retry", systemImage: "arrow.uturn.backward")
            }
            .disabled(model.queueSelection == nil)
            .help("Put a rejected entry back in the running (r)")
            Button { if let id = model.queueSelection { model.forceApply(id) } } label: {
                Label("Force Apply", systemImage: "exclamationmark.shield")
            }
            .disabled(model.queueSelection == nil)
            .help("Transfer without the fingerprint check, after showing the plan (F)")
            Button(role: .destructive) { if let id = model.queueSelection { model.forget(id) } } label: {
                Label("Forget", systemImage: "trash")
            }
            .disabled(model.queueSelection == nil)
            .help("Drop this entry. The downloaded file stays on disk (c)")
            Button(role: .destructive) { model.clearQueue() } label: {
                Label("Clear Queue", systemImage: "trash.slash")
            }
            .disabled(model.queueEntries.isEmpty)
            .help("Forget every entry, after asking (C)")
        }
    }
}
