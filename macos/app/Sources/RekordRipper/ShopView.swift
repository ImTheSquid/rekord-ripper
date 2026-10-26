import SwiftUI

/// Search the backends for better copies: tracks on the left, offers grouped
/// by the track they were found for on the right.
struct ShopView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        @Bindable var model = model
        HSplitView {
            VStack(spacing: 0) {
                ColumnHeader(title: "Tracks", count: model.shopRows.count, query: $model.shopQuery) {
                    if model.shopSelection.count > 1 {
                        Text("\(model.shopSelection.count) in basket").foregroundStyle(.secondary)
                    }
                }
                ShopTrackTable()
            }
            .frame(minWidth: 340, idealWidth: 420)
            VSplitView {
                OfferList()
                    .frame(minHeight: 200)
                OfferDetail()
                    .frame(minHeight: 120, idealHeight: 200)
            }
            .frame(minWidth: 480)
        }
        .toolbar { ShopToolbar() }
    }
}

struct ShopToolbar: ToolbarContent {
    @Environment(SessionModel.self) private var model

    var body: some ToolbarContent {
        ToolbarItemGroup {
            Button {
                model.search(model.shopSelection)
            } label: {
                Label(model.shopSelection.count > 1 ? "Search Basket" : "Search", systemImage: "magnifyingglass")
            }
            .disabled(model.shopSelection.isEmpty)
            .help("Search every backend for the selected tracks, keeping results already found (s / S)")
            Button {
                model.research(model.selectedOffer?.srcId ?? model.shopSelection.first)
            } label: {
                Label("Search Again", systemImage: "arrow.clockwise")
            }
            .disabled(model.selectedOffer == nil && model.shopSelection.count != 1)
            .help("Re-run one track's search, replacing just its results (r)")
        }
        ToolbarItemGroup {
            Button {
                if let o = model.selectedOffer { model.openPage(o) }
            } label: {
                Label("Buy Page", systemImage: "safari")
            }
            .disabled(model.selectedOffer == nil)
            .help("Open the offer's page, where buying happens (o)")
            Button {
                if let o = model.selectedOffer { model.fetch(o) }
            } label: {
                Label("Download", systemImage: "arrow.down.circle")
            }
            .disabled(model.selectedOffer.map { $0.requiresPurchase || $0.fetch != .none } ?? true)
            .help("Download the offer and queue its transfer (f)")
        }
    }
}

struct ShopTrackTable: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Table(model.shopRows, selection: $model.shopSelection) {
            TableColumn("") { t in stateBadge(model.trackStates[t.id]) }
                .width(28)
            TableColumn("Title") { t in Text(t.displayTitle) }
                .width(min: 140, ideal: 200)
            TableColumn("Artist") { t in Text(t.artist.isEmpty ? "—" : t.artist) }
                .width(min: 80, ideal: 120)
            TableColumn("Type") { t in Text(t.fileType) }
                .width(min: 40, ideal: 50)
        }
        .contextMenu(forSelectionType: Track.ID.self) { ids in
            Button("Search") { model.search(ids) }
        } primaryAction: { ids in
            model.search(ids)
        }
    }

    @ViewBuilder
    private func stateBadge(_ state: Int64?) -> some View {
        switch state {
        case nil: EmptyView()
        case -1: Image(systemName: "clock").foregroundStyle(.secondary).help("Queued")
        case 0: Image(systemName: "xmark.circle").foregroundStyle(.secondary).help("Found nothing")
        case let n?: Text("\(n)").font(.caption).monospacedDigit().foregroundStyle(.green).help("\(n) offers found")
        }
    }
}

struct OfferList: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Group {
            switch model.shopStatus.kind {
            case .idle:
                ContentUnavailableView("No searches yet", systemImage: "cart",
                    description: Text("Select tracks and press Search."))
            case .searching:
                VStack(spacing: 8) {
                    ProgressView()
                    Text("Searching for \(model.shopStatus.text)…")
                    Text("\(model.shopStatus.elapsedSecs)s").foregroundStyle(.secondary).monospacedDigit()
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .failed:
                ContentUnavailableView("Search failed", systemImage: "exclamationmark.triangle",
                    description: Text(model.shopStatus.text))
            case .results:
                List(selection: $model.offerSelection) {
                    ForEach(model.offerGroups) { group in
                        Section {
                            if group.offers.isEmpty {
                                Text("No offers").foregroundStyle(.secondary)
                            }
                            ForEach(group.offers) { offer in
                                OfferRow(offer: offer).tag(offer.id)
                            }
                        } header: {
                            Text(group.label)
                        }
                    }
                }
                .contextMenu(forSelectionType: Offer.ID.self) { _ in } primaryAction: { ids in
                    if let id = ids.first, let o = model.offers.first(where: { $0.id == id }), !o.requiresPurchase {
                        model.fetch(o)
                    }
                }
            }
        }
    }
}

struct OfferRow: View {
    let offer: Offer

    var body: some View {
        HStack(spacing: 8) {
            fetchIcon.frame(width: 16)
            losslessIcon.frame(width: 16)
            Text(offer.backend)
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(width: 80, alignment: .leading)
            Text("\(offer.artist) — \(offer.title)")
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text(offer.formats)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .frame(width: 150, alignment: .leading)
            Text(offer.price)
                .monospacedDigit()
                .foregroundStyle(offer.requiresPurchase ? .primary : Color.green)
                .frame(width: 110, alignment: .trailing)
        }
    }

    @ViewBuilder private var fetchIcon: some View {
        switch offer.fetch {
        case .none: Color.clear
        case .queued: Image(systemName: "clock").foregroundStyle(.secondary).help("Queued")
        case .running: Image(systemName: "arrow.down.circle").foregroundStyle(.blue).help("Downloading")
        case .done: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green).help("Downloaded")
        }
    }

    @ViewBuilder private var losslessIcon: some View {
        switch offer.lossless {
        case true?: Image(systemName: "waveform").foregroundStyle(.green).help("Lossless")
        case false?: Color.clear
        case nil: Image(systemName: "questionmark").foregroundStyle(.secondary)
            .help("Formats not checked — only the top offers are probed")
        }
    }
}

struct OfferDetail: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                DownloadPanel()
                if let o = model.selectedOffer {
                    Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 12, verticalSpacing: 4) {
                        row("Artist", o.artist)
                        row("Title", o.title)
                        row("Album", o.album.isEmpty ? "—" : o.album)
                        row("Formats", o.formats)
                        row("Price", o.price)
                        row("Owned", o.ownership)
                        row("URL", o.url)
                        row("Ref", o.itemRef)
                        if !o.note.isEmpty { row("Note", o.note) }
                    }
                    .textSelection(.enabled)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(10)
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        GridRow {
            Text(label).foregroundStyle(.secondary)
            Text(value)
        }
    }
}

/// The download in flight, or the last one's outcome.
struct DownloadPanel: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        let f = model.fetchStatus
        switch f.kind {
        case .idle:
            EmptyView()
        case .running:
            VStack(alignment: .leading, spacing: 2) {
                HStack {
                    ProgressView().controlSize(.small)
                    Text("Downloading \(f.text)")
                    Text("\(f.elapsedSecs)s").foregroundStyle(.secondary).monospacedDigit()
                    if model.fetchQueue.count > 1 {
                        Text("+\(model.fetchQueue.count - 1) queued").foregroundStyle(.secondary)
                    }
                }
                if !f.note.isEmpty { Text(f.note).font(.caption).foregroundStyle(.secondary) }
            }
        case .done:
            VStack(alignment: .leading, spacing: 2) {
                ForEach(f.text.split(separator: "\n").map(String.init), id: \.self) {
                    Label("Saved \($0)", systemImage: "checkmark.circle").foregroundStyle(.green)
                }
                Text(f.queuedEntry >= 0
                    ? "Queued transfer #\(f.queuedEntry). Import it from the Queue."
                    : "No source track was selected, so no transfer was queued.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        case .failed:
            Label("Download failed: \(f.text)", systemImage: "xmark.octagon").foregroundStyle(.red)
        }
    }
}
