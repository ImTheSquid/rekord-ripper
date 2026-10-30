import SwiftUI

/// The toolbar's window onto background work: a ring while anything runs, and
/// a badge for confirmations nobody has looked at.
struct ActivityButton: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Button { model.activityShown.toggle() } label: {
            ZStack(alignment: .topTrailing) {
                if model.busy {
                    ProgressView().controlSize(.small).frame(width: 18, height: 18)
                } else {
                    Image(systemName: "tray.full")
                }
                if !model.waiting.isEmpty {
                    Circle().fill(.red).frame(width: 8, height: 8).offset(x: 3, y: -3)
                }
            }
        }
        .help("Activity")
        .accessibilityLabel("Activity")
        .popover(isPresented: $model.activityShown, arrowEdge: .bottom) {
            ActivityView().environment(model)
        }
    }
}

struct ActivityView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if !model.waiting.isEmpty {
                    group("Waiting for You") {
                        ForEach(model.waiting) { c in
                            HStack {
                                Label(c.waitingLabel, systemImage: "exclamationmark.bubble")
                                Spacer()
                                Button("Review…") { model.present(c.kind) }
                            }
                        }
                    }
                }
                if model.shop.busy {
                    group("Searching") {
                        if model.shop.total > 1 {
                            ProgressView(value: Double(model.shop.done), total: Double(model.shop.total)) {
                                Text(model.shop.text).lineLimit(1)
                            }
                        } else {
                            Label { Text(model.shop.text).lineLimit(1) } icon: { ProgressView().controlSize(.small) }
                        }
                    }
                }
                if !model.downloads.isEmpty {
                    group("Downloading") {
                        ForEach(model.downloads) { DownloadRow(download: $0) }
                    }
                }
                if !model.entries.isEmpty {
                    group("Downloads") {
                        ForEach(model.entries) { e in
                            VStack(alignment: .leading, spacing: 0) {
                                Text(e.title.isEmpty ? e.fileName : e.title).font(.caption.weight(.medium))
                                EntryRow(entry: e)
                            }
                        }
                        HStack {
                            Button("Import") { model.importDownloads() }
                            Button("Check") { model.checkDownloads() }
                            Spacer()
                            Button("Clear…", role: .destructive) { model.clearQueue() }
                        }
                        .controlSize(.small)
                    }
                }
                group("Recent") {
                    if model.notices.isEmpty {
                        Text("Nothing yet.").foregroundStyle(.secondary)
                    }
                    ForEach(model.notices.reversed()) { n in
                        NoticeRow(notice: n)
                    }
                }
            }
            .padding()
        }
        .frame(width: 400, height: 480)
    }

    private func group(_ title: String, @ViewBuilder _ content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
            content()
        }
    }
}

struct NoticeRow: View {
    @Environment(SessionModel.self) private var model
    let notice: Notice

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: notice.level.symbol).foregroundStyle(notice.level.tone.color)
            Text(notice.text).font(.callout).textSelection(.enabled)
            Spacer(minLength: 4)
            if let title = notice.step.title {
                Button(title) { model.perform(notice) }.controlSize(.small)
            }
        }
    }
}

struct DownloadRow: View {
    let download: Download

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            VStack(alignment: .leading, spacing: 0) {
                Label {
                    Text(download.what).lineLimit(1).help(download.what)
                } icon: {
                    switch download.stage {
                    case .waiting: Image(systemName: "clock").foregroundStyle(.secondary)
                    case .running, .cancelling: ProgressView().controlSize(.small)
                    }
                }
                if let detail {
                    Text(detail).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            Spacer(minLength: 4)
            CancelDownloadButton(itemRef: download.itemRef)
                .disabled(download.stage == .cancelling)
        }
    }

    private var detail: String? {
        switch download.stage {
        case .waiting: "Waiting"
        case .cancelling: "Cancelling…"
        case .running: download.note.isEmpty ? nil : download.note
        }
    }
}

struct CancelDownloadButton: View {
    @Environment(SessionModel.self) private var model
    let itemRef: String

    var body: some View {
        Button {
            model.cancelDownload(itemRef)
        } label: {
            Image(systemName: "xmark.circle.fill")
        }
        .buttonStyle(.borderless)
        .foregroundStyle(.secondary)
        .help("Cancel download")
    }
}

/// The latest notice, briefly, with its step as a button.
struct ToastView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        ZStack {
            if let toast = model.toast {
                NoticeRow(notice: toast)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .frame(maxWidth: 560)
                    .background(.regularMaterial, in: .rect(cornerRadius: 10))
                    .shadow(radius: 6, y: 2)
                    .padding(.bottom, 16)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
                    .id(toast.id)
            }
        }
        .animation(.easeOut(duration: 0.2), value: model.toast?.id)
    }
}
