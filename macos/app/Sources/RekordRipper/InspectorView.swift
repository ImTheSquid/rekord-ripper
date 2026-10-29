import AppKit
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
        InspectorScroll {
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

/// A scroll view that takes the toolbar's inset as padding instead.
///
/// Under the toolbar, SwiftUI draws scrolled content below the inset but matches
/// clicks as if there were none, so a button missed clicks by the toolbar's height.
private struct InspectorScroll<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        GeometryReader { geo in
            ScrollView {
                content.padding(.top, geo.safeAreaInsets.top)
            }
            .ignoresSafeArea(.container, edges: .top)
        }
    }
}

struct TrackHeader: View {
    let track: Track

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            ArtworkView(path: track.artworkPath, size: 72)
            VStack(alignment: .leading, spacing: 3) {
                Text(track.displayTitle).font(.headline).lineLimit(2)
                    .textSelection(.enabled)
                Text(track.artist).foregroundStyle(.secondary).lineLimit(1)
                    .textSelection(.enabled)
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
    @State private var showAll = false

    /// Rows shown before "Show More".
    private let fold = 4

    var body: some View {
        let offers = model.offers(for: track.id)
        let state = model.upgrade(for: track.id)
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Better Copies").font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
                Spacer()
                if state == .searching {
                    ProgressView().controlSize(.small)
                } else if state != nil || !offers.isEmpty {
                    Button("Search Again", systemImage: "arrow.clockwise") { model.research(track.id) }
                    .labelStyle(.iconOnly)
                    .controlSize(.small)
                    .help("Search Again")
                }
            }
            if offers.isEmpty {
                switch state {
                case .searching?:
                    Text("Searching…").foregroundStyle(.secondary)
                case .notFound?:
                    Text("Nothing better found.").foregroundStyle(.secondary)
                default:
                    Button("Find Better Copies") { model.findBetterCopies([track.id]) }
                }
            } else {
                VStack(spacing: 0) {
                    let shown = showAll ? offers[...] : offers.prefix(fold)
                    ForEach(Array(shown.enumerated()), id: \.element.id) { i, offer in
                        // Starts under the text, past the format badge.
                        if i > 0 { Divider().padding(.leading, 8 + FormatBadge.width + 8) }
                        OfferRow(offer: offer)
                            .padding(.horizontal, 8)
                            .padding(.vertical, 6)
                    }
                }
                .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
                if offers.count > fold {
                    Button(showAll ? "Show Fewer" : "Show \(offers.count - fold) More") {
                        withAnimation(.snappy) { showAll.toggle() }
                    }
                    .controlSize(.small)
                }
            }
            let answers = model.answers(for: track.id)
            if !answers.isEmpty {
                BackendAnswers(answers: answers)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// "Bandcamp 0 · SoundCloud 1 · Soulseek failed", with the failure on hover.
private struct BackendAnswers: View {
    let answers: [BackendAnswer]

    var body: some View {
        HStack(spacing: 4) {
            ForEach(Array(answers.enumerated()), id: \.element) { i, a in
                if i > 0 { Text("·") }
                if a.error.isEmpty {
                    Text("\(a.backend) \(a.offers)")
                } else {
                    Label("\(a.backend) failed", systemImage: "exclamationmark.triangle.fill")
                        .labelStyle(.titleAndIcon)
                        .foregroundStyle(.orange)
                        .help(a.error)
                }
            }
        }
        .font(.caption)
        .foregroundStyle(.secondary)
    }
}

struct OfferRow: View {
    @Environment(SessionModel.self) private var model
    let offer: Offer

    var body: some View {
        HStack(alignment: .center, spacing: 8) {
            FormatBadge(offer: offer)
            VStack(alignment: .leading, spacing: 1) {
                Text("\(offer.artist) — \(offer.title)")
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(details)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                if offer.matchScore < 100 {
                    Text("\(offer.matchScore)% match")
                        .font(.caption2)
                        .foregroundStyle(offer.matchScore < 60 ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
                }
                if !offer.note.isEmpty {
                    Text(offer.note).font(.caption2).foregroundStyle(.orange).lineLimit(2)
                }
            }
            // On the text only, so it never sits over the button.
            .contextMenu {
                Button("Open Store Page") { model.openPage(offer) }
                Button("Copy Link") { copy(offer.url) }
                    .disabled(offer.url.isEmpty)
                Button("Copy Artist and Title") { copy("\(offer.artist) - \(offer.title)") }
            }
            Spacer(minLength: 4)
            switch offer.fetch {
            case .none:
                if offer.requiresPurchase {
                    Button("Buy") { model.openPage(offer) }
                        .help("Open the store page")
                } else if offer.fetchable {
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
    }

    /// "Soulseek · free · album name"
    private var details: String {
        var parts = [offer.backend, offer.price]
        if !offer.ownership.isEmpty, offer.ownership != "n/a" { parts.append(offer.ownership) }
        if !offer.album.isEmpty, offer.album != offer.title { parts.append(offer.album) }
        return parts.filter { !$0.isEmpty }.joined(separator: " · ")
    }

    private func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

/// The best format on offer, green when lossless.
private struct FormatBadge: View {
    static let width: CGFloat = 52
    let offer: Offer

    var body: some View {
        let best = offer.formats.split(separator: " ").first.map(String.init) ?? "?"
        let colour: Color = offer.lossless == true ? .green : .secondary
        Text(best)
            .font(.caption2.weight(.semibold).monospaced())
            .lineLimit(1)
            .fixedSize()
            .frame(minWidth: Self.width)
            .padding(.vertical, 2)
            .foregroundStyle(colour)
            .background(colour.opacity(0.12), in: RoundedRectangle(cornerRadius: 4))
            .help(offer.formats.isEmpty ? "Formats not checked" : offer.formats)
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
        InspectorScroll {
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
