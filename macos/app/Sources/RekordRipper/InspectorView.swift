import SwiftUI

struct InspectorView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        let selected = model.selectedTracks
        switch selected.count {
        case 0:
            ContentUnavailableView("No Selection", systemImage: "music.note",
                                   description: Text("Select a track to see its cues, better copies and twins."))
        case 1:
            TrackInspector(track: selected[0])
                .id(selected[0].id)
        default:
            MultiInspector(tracks: selected.sorted { $0.title < $1.title })
                .id(model.selection)
        }
    }
}

struct TrackInspector: View {
    @Environment(SessionModel.self) private var model
    let track: Track

    private var inspection: Inspection? {
        model.inspection?.trackId == track.id ? model.inspection : nil
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                TrackHeader(track: track)
                if let lengthSecs = track.lengthSecs, lengthSecs > 0 {
                    CueStrip(marks: inspection?.cueMarks ?? [], lengthMs: lengthSecs * 1000)
                        .frame(height: 22)
                }
                BetterCopiesSection(track: track)
                CopyAnalysisSection(track: track, suggestions: inspection?.suggestions)
                let entries = model.entries(for: track.id)
                if !entries.isEmpty {
                    PipelineSection(entries: entries)
                }
            }
            .padding()
        }
    }
}

struct TrackHeader: View {
    let track: Track

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            RoundedRectangle(cornerRadius: 6)
                .fill(.quaternary)
                .frame(width: 72, height: 72)
                .overlay { Image(systemName: "music.note").font(.title).foregroundStyle(.secondary) }
            VStack(alignment: .leading, spacing: 3) {
                Text(track.displayTitle).font(.headline).lineLimit(2)
                Text(track.artist).foregroundStyle(.secondary).lineLimit(1)
                HStack(spacing: 6) {
                    SourceBadge(track: track)
                    if track.missing { Tag("Missing", .orange) }
                    if track.locked { Tag("Locked", .secondary) }
                }
                .padding(.top, 2)
            }
        }
        Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 4) {
            GridRow {
                Fact("BPM", track.bpmText)
                Fact("Key", track.key.isEmpty ? "—" : track.key)
                Fact("Time", track.lengthText)
                Fact("Cues", track.cueCount > 0 ? "\(track.cueCount)" : "—")
            }
        }
    }
}

private struct Tag: View {
    let text: String
    let color: Color

    init(_ text: String, _ color: Color) {
        self.text = text
        self.color = color
    }

    var body: some View {
        Text(text)
            .font(.caption2.weight(.medium))
            .padding(.horizontal, 5)
            .padding(.vertical, 1)
            .foregroundStyle(color)
            .overlay(Capsule().strokeBorder(color.opacity(0.6)))
    }
}

private struct Fact: View {
    let label: String
    let value: String

    init(_ label: String, _ value: String) {
        self.label = label
        self.value = value
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label).font(.caption).foregroundStyle(.secondary)
            Text(value).monospacedDigit()
        }
    }
}

/// Cues along the track: hot cues green, memory cues red, loops as spans.
struct CueStrip: View {
    let marks: [CueMark]
    let lengthMs: Int64

    var body: some View {
        Canvas { ctx, size in
            ctx.fill(Path(roundedRect: CGRect(origin: .zero, size: size), cornerRadius: 4), with: .color(.secondary.opacity(0.15)))
            func x(_ ms: Int64) -> CGFloat { size.width * CGFloat(min(max(ms, 0), lengthMs)) / CGFloat(lengthMs) }
            for m in marks {
                let colour: Color = m.endMs != nil ? .orange : m.hot ? .green : .red
                if let end = m.endMs {
                    let span = CGRect(x: x(m.ms), y: 2, width: max(x(end) - x(m.ms), 2), height: size.height - 4)
                    ctx.fill(Path(span), with: .color(colour.opacity(0.35)))
                }
                ctx.fill(Path(CGRect(x: x(m.ms) - 1, y: 0, width: 2, height: size.height)), with: .color(colour))
            }
        }
        .accessibilityLabel("\(marks.count) cues")
        .help(marks.isEmpty ? "No cues" : "\(marks.count) cue(s)")
    }
}

private struct SectionBox<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct BetterCopiesSection: View {
    @Environment(SessionModel.self) private var model
    let track: Track

    var body: some View {
        SectionBox(title: "Better Copies") {
            let offers = model.offers(for: track.id)
            switch model.upgrade(for: track.id) {
            case .searching?:
                Label { Text("Searching…") } icon: { ProgressView().controlSize(.small) }
            case .notFound?:
                Text("Nothing better found.").foregroundStyle(.secondary)
                Button("Search Again") { model.research(track.id) }
            default:
                if offers.isEmpty {
                    Button("Find Better Copies") { model.findBetterCopies([track.id]) }
                } else {
                    ForEach(offers.prefix(6)) { offer in
                        OfferRow(offer: offer)
                    }
                    if offers.count > 6 {
                        Text("\(offers.count - 6) more").font(.caption).foregroundStyle(.secondary)
                    }
                    Button("Search Again") { model.research(track.id) }
                        .controlSize(.small)
                }
            }
        }
    }
}

struct OfferRow: View {
    @Environment(SessionModel.self) private var model
    let offer: Offer

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text("\(offer.artist) — \(offer.title)").lineLimit(1)
                HStack(spacing: 4) {
                    if offer.lossless == true {
                        Image(systemName: "waveform").foregroundStyle(.green).help("Lossless")
                    }
                    Text(offer.summary)
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                if !offer.note.isEmpty {
                    Text(offer.note).font(.caption2).foregroundStyle(.orange).lineLimit(2)
                }
            }
            Spacer(minLength: 4)
            switch offer.fetch {
            case .none:
                if offer.requiresPurchase {
                    Button("Buy") { model.openPage(offer) }
                        .help("Open the store page")
                } else {
                    Button("Download") { model.download(offer) }
                }
            case .queued:
                Image(systemName: "clock").foregroundStyle(.secondary).help("Queued")
            case .running:
                ProgressView().controlSize(.small)
            case .done:
                Image(systemName: "checkmark.circle.fill").foregroundStyle(.green).help("Downloaded")
            }
        }
        .controlSize(.small)
        .contextMenu {
            Button("Open Store Page") { model.openPage(offer) }
        }
    }
}

struct CopyAnalysisSection: View {
    @Environment(SessionModel.self) private var model
    let track: Track
    let suggestions: [Suggestion]?
    @State private var checked = Set<Track.ID>()

    var body: some View {
        SectionBox(title: "Copy Analysis") {
            if let suggestions {
                let outgoing = suggestions.filter(\.outgoing)
                let incoming = suggestions.filter { !$0.outgoing }
                if suggestions.isEmpty {
                    Text("No twin in your library. Select two tracks to pair them by hand.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                ForEach(incoming) { s in
                    if let twin = model.track(s.id) {
                        HStack {
                            TwinLabel(track: twin)
                            Spacer()
                            Button("Copy From") { model.reviewTransfer(from: twin.id, to: [track.id]) }
                                .controlSize(.small)
                        }
                    }
                }
                if !outgoing.isEmpty {
                    ForEach(outgoing) { s in
                        if let twin = model.track(s.id) {
                            Toggle(isOn: Binding(
                                get: { checked.contains(s.id) },
                                set: { if $0 { checked.insert(s.id) } else { checked.remove(s.id) } }
                            )) {
                                TwinLabel(track: twin)
                            }
                        }
                    }
                    Button("Copy To \(checked.count) Track(s)…") {
                        model.reviewTransfer(from: track.id, to: checked)
                    }
                    .disabled(checked.isEmpty)
                    .onAppear { checked = Set(outgoing.map(\.id)) }
                }
            } else {
                ProgressView().controlSize(.small)
            }
        }
    }
}

private struct TwinLabel: View {
    let track: Track

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(track.displayTitle).lineLimit(1)
            HStack(spacing: 6) {
                SourceBadge(track: track)
                Text(track.cueCount > 0 ? "\(track.cueCount) cues" : "no cues")
                Text(track.bpmText)
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
    }
}

struct PipelineSection: View {
    @Environment(SessionModel.self) private var model
    let entries: [QueueEntry]

    var body: some View {
        SectionBox(title: "Downloads") {
            ForEach(entries) { e in
                EntryRow(entry: e)
            }
        }
    }
}

struct EntryRow: View {
    @Environment(SessionModel.self) private var model
    let entry: QueueEntry

    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 1) {
                Text(entry.fileName).lineLimit(1).truncationMode(.middle)
                HStack(spacing: 4) {
                    if entry.inFlight { ProgressView().controlSize(.mini) }
                    Text(entry.state).foregroundStyle(entry.tone.color)
                }
                .font(.caption)
            }
            Spacer(minLength: 4)
            switch entry.step {
            case .importDownloads:
                Button("Import") { model.importDownloads() }
            case .checkDownloads:
                Button("Check") { model.checkDownloads() }
            default:
                EmptyView()
            }
            Menu {
                Button("Retry") { model.retry(entry.id) }
                Button("Apply Without Checking…") { model.force(entry.id) }
                Divider()
                Button("Forget", role: .destructive) { model.forget(entry.id) }
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
        }
        .controlSize(.small)
        .disabled(entry.inFlight)
    }
}

struct MultiInspector: View {
    @Environment(SessionModel.self) private var model
    let tracks: [Track]
    @State private var source: Track.ID?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Text("\(tracks.count) Tracks").font(.headline)
                SectionBox(title: "Better Copies") {
                    Button("Find Better Copies for \(tracks.count) Tracks") {
                        model.findBetterCopies(Set(tracks.map(\.id)))
                    }
                }
                SectionBox(title: "Copy Analysis") {
                    Picker("From", selection: $source) {
                        ForEach(tracks) { t in
                            Text("\(t.displayTitle) (\(t.cueCount) cues)").tag(Optional(t.id))
                        }
                    }
                    Button("Copy To the Other \(tracks.count - 1)…") {
                        if let source { model.reviewTransfer(from: source, to: tracks.map(\.id)) }
                    }
                    .disabled(source == nil)
                }
            }
            .padding()
        }
        .onAppear { source = tracks.max { ($0.analysed ? 1 : 0, $0.cueCount) < ($1.analysed ? 1 : 0, $1.cueCount) }?.id }
    }
}
