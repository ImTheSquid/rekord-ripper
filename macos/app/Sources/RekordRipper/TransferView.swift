import SwiftUI

/// Copy one track's analysis onto others: sources on the left, destinations on
/// the right, as in the TUI.
struct TransferView: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        @Bindable var model = model
        VSplitView {
            HSplitView {
                VStack(spacing: 0) {
                    ColumnHeader(title: "Sources", count: model.srcRows.count, query: $model.srcQuery)
                    TrackTable(rows: model.srcRows, selection: Binding(
                        get: { model.srcSelection.map { [$0] } ?? [] },
                        set: { model.srcSelection = $0.first }
                    ))
                }
                .frame(minWidth: 360)
                VStack(spacing: 0) {
                    ColumnHeader(title: "Destinations", count: model.dstRows.count, query: $model.dstQuery) {
                        if !model.dstSelection.isEmpty {
                            Text(model.hiddenDstSelection > 0
                                ? "\(model.dstSelection.count) selected, \(model.hiddenDstSelection) hidden"
                                : "\(model.dstSelection.count) selected")
                                .foregroundStyle(model.hiddenDstSelection > 0 ? .orange : .secondary)
                        }
                    }
                    TrackTable(rows: model.dstRows, selection: $model.dstSelection)
                }
                .frame(minWidth: 360)
            }
            PairPreview()
                .frame(minHeight: 80, idealHeight: 100, maxHeight: 160)
        }
        .toolbar { TransferToolbar() }
    }
}

struct TransferToolbar: ToolbarContent {
    @Environment(SessionModel.self) private var model

    var body: some ToolbarContent {
        @Bindable var model = model
        ToolbarItemGroup {
            Toggle(isOn: $model.autoFilter) {
                Label("Unanalysed only", systemImage: "wand.and.stars")
            }
            .help("Only destinations that have no cues and are not locked (a)")
            Toggle(isOn: $model.fuzzyFromSrc) {
                Label("Match source", systemImage: "link")
            }
            .help("Only destinations with the source's title and length (f)")
            Toggle(isOn: Binding(get: { model.copyReplace }, set: { model.setCopyOptions(replace: $0, lock: model.copyLock) })) {
                Label("Replace cues", systemImage: "arrow.triangle.2.circlepath")
            }
            .help("Overwrite cues on destinations that already have some (r)")
            Toggle(isOn: Binding(get: { model.copyLock }, set: { model.setCopyOptions(replace: model.copyReplace, lock: $0) })) {
                Label("Lock", systemImage: "lock")
            }
            .help("Set the lock bit so rekordbox will not re-analyse (l)")
        }
        ToolbarItemGroup {
            Button {
                model.shopForSource()
            } label: {
                Label("Shop", systemImage: "cart")
            }
            .disabled(model.srcSelection == nil)
            .help("Search every backend for the source track (s)")
            Button {
                model.reviewTransfer()
            } label: {
                Label("Apply…", systemImage: "square.and.arrow.down.on.square")
            }
            .disabled(model.srcSelection == nil || model.dstSelection.subtracting([model.srcSelection ?? ""]).isEmpty)
            .help("Review, then copy the source's analysis onto every selected destination (Enter)")
        }
    }
}

struct ColumnHeader<Accessory: View>: View {
    let title: String
    let count: Int
    @Binding var query: String
    @ViewBuilder var accessory: Accessory

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(title).font(.headline)
                Text("\(count)").foregroundStyle(.secondary)
                Spacer()
                accessory
            }
            TextField("Search — burial -remix p:\"jack night\" bpm:120-130", text: $query)
                .textFieldStyle(.roundedBorder)
        }
        .padding(8)
    }
}

extension ColumnHeader where Accessory == EmptyView {
    init(title: String, count: Int, query: Binding<String>) {
        self.init(title: title, count: count, query: query) { EmptyView() }
    }
}

struct TrackTable: View {
    let rows: [Track]
    @Binding var selection: Set<Track.ID>

    var body: some View {
        Table(rows, selection: $selection) {
            TableColumn("Title") { t in
                HStack(spacing: 4) {
                    if t.locked { Image(systemName: "lock.fill").foregroundStyle(.secondary) }
                    Text(t.displayTitle)
                }
            }
            .width(min: 140, ideal: 220)
            TableColumn("Artist") { t in Text(t.artist.isEmpty ? "—" : t.artist) }
                .width(min: 80, ideal: 140)
            TableColumn("BPM") { t in Text(t.bpmText).monospacedDigit() }
                .width(min: 50, ideal: 60)
            TableColumn("Length") { t in Text(t.lengthText).monospacedDigit() }
                .width(min: 50, ideal: 60)
            TableColumn("Cues") { t in Text("\(t.cueCount)").monospacedDigit() }
                .width(min: 36, ideal: 40)
            TableColumn("Type") { t in Text(t.fileType) }
                .width(min: 40, ideal: 50)
        }
    }
}

struct PairPreview: View {
    @Environment(SessionModel.self) private var model

    var body: some View {
        let dst = model.dstSelection.count == 1 ? model.dstSelection.first : nil
        VStack(alignment: .leading, spacing: 2) {
            ForEach(Array(model.preview(dst: dst).enumerated()), id: \.offset) { _, line in
                Text(line).font(.system(.callout, design: .monospaced))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .padding(8)
        .textSelection(.enabled)
    }
}
