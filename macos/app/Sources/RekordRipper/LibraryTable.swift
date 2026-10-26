import SwiftUI

struct LibraryTable: View {
    @Environment(SessionModel.self) private var model
    @State private var sortOrder: [KeyPathComparator<Track>] = []

    var body: some View {
        @Bindable var model = model
        Table(of: Track.self, selection: $model.selection, sortOrder: $sortOrder) {
            TableColumn("") { t in
                ArtworkView(path: t.artworkPath, size: 20)
            }
            .width(24)
            TableColumn("Title", value: \.title) { t in
                HStack(spacing: 4) {
                    if t.missing {
                        Image(systemName: "exclamationmark.circle")
                            .foregroundStyle(.orange)
                            .help("The file is not on this Mac")
                    }
                    Text(t.displayTitle)
                        .foregroundStyle(t.missing ? .secondary : .primary)
                }
            }
            .width(min: 160, ideal: 280)
            TableColumn("Artist", value: \.artist)
                .width(min: 100, ideal: 180)
            TableColumn("BPM", value: \.bpmValue) { t in
                Text(t.bpmText).monospacedDigit()
            }
            .width(56)
            TableColumn("Key", value: \.key)
                .width(40)
            TableColumn("Time", value: \.lengthSort) { t in
                Text(t.lengthText).monospacedDigit()
            }
            .width(48)
            TableColumn("Source", value: \.sourceText) { t in
                SourceBadge(track: t)
            }
            .width(min: 64, ideal: 90)
            TableColumn("Cues", value: \.cueCount) { t in
                HStack(spacing: 2) {
                    Text(t.cueCount > 0 ? "\(t.cueCount)" : "—").monospacedDigit()
                    if t.locked {
                        Image(systemName: "lock.fill").font(.caption2).foregroundStyle(.secondary)
                    }
                }
            }
            .width(48)
            TableColumn("Upgrade") { t in
                UpgradeCell(upgrade: model.upgrade(for: t.id))
            }
            .width(min: 120, ideal: 220)
        } rows: {
            ForEach(rows) { t in
                TableRow(t)
                    .draggable(TrackDrop.prefix + t.id)
                    .dropDestination(for: TrackDrop.self) { model.drop($0, on: t.id) }
            }
        }
        .contextMenu(forSelectionType: Track.ID.self) { ids in
            if !ids.isEmpty {
                Button(ids.count == 1 ? "Find Better Copies" : "Find Better Copies for \(ids.count) Tracks") {
                    model.findBetterCopies(ids)
                }
            }
        } primaryAction: { _ in
            model.inspectorShown = true
        }
        .overlay {
            if rows.isEmpty, !model.tracks.isEmpty {
                ContentUnavailableView.search(text: model.query)
            }
        }
    }

    private var rows: [Track] {
        sortOrder.isEmpty ? model.visibleTracks : model.visibleTracks.sorted(using: sortOrder)
    }
}

extension Track {
    var lengthSort: Int64 { lengthSecs ?? 0 }

    var sourceText: String {
        switch origin {
        case .stream: fileType
        case .cloud, .local: fileType
        }
    }
}

struct SourceBadge: View {
    let track: Track

    var body: some View {
        HStack(spacing: 4) {
            switch track.origin {
            case .stream:
                Image(systemName: "dot.radiowaves.left.and.right").help("Streamed: \(track.fileType)")
            case .cloud:
                Image(systemName: "icloud").help("Cloud Library Sync")
            case .local:
                EmptyView()
            }
            Text(track.fileType)
                .lineLimit(1)
                .foregroundStyle(color)
        }
        .font(.caption)
    }

    private var color: Color {
        switch track.lossless {
        case true?: .green
        case false?: .orange
        case nil: .secondary
        }
    }
}

struct UpgradeCell: View {
    let upgrade: Upgrade?

    var body: some View {
        if let upgrade {
            HStack(spacing: 4) {
                switch upgrade {
                case .searching, .downloading:
                    ProgressView().controlSize(.mini)
                case .entry(let e) where e.inFlight:
                    ProgressView().controlSize(.mini)
                default:
                    EmptyView()
                }
                Text(upgrade.text)
                    .lineLimit(1)
                    .foregroundStyle(upgrade.tone.color)
            }
            .font(.callout)
        }
    }
}
