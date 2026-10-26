import SwiftUI

/// Every write a confirmation would make, drawn rather than printed.
struct ReviewSheet: View {
    @Environment(SessionModel.self) private var model
    let confirmation: Confirmation

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            if confirmation.kind == .forceApply {
                Callout(tone: .bad, text: waiver)
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    switch confirmation.kind {
                    case .transfer, .forceApply:
                        ForEach(confirmation.plans) { PlanCard(plan: $0) }
                        if !skipped.isEmpty {
                            Text("Skipped").font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
                            ForEach(skipped, id: \.self) { Text($0).font(.callout).textSelection(.enabled) }
                        }
                    case .importRows:
                        ImportList(lines: confirmation.lines)
                    case .clearQueue:
                        ForEach(confirmation.lines, id: \.self) { Text($0.text) }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            if model.status.level == .error {
                Callout(tone: .bad, text: model.status.text)
            }
            buttons
        }
        .padding(20)
        .frame(minWidth: 640, idealWidth: 720, minHeight: 380, idealHeight: 580)
    }

    private var header: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol)
                .font(.title2)
                .foregroundStyle(confirmation.isDestructive ? .red : .accentColor)
            VStack(alignment: .leading, spacing: 2) {
                Text(confirmation.title).font(.headline)
                if let src = confirmation.plans.first, confirmation.kind == .transfer {
                    Text("From “\(src.srcTitle)” — \(src.srcArtist)").foregroundStyle(.secondary)
                }
            }
        }
    }

    private var buttons: some View {
        HStack {
            Button("Later") { model.presented = nil }
                .help("Keep this waiting in Activity")
            Spacer()
            Button("Cancel", role: .cancel) { model.cancel(confirmation.kind) }
                .keyboardShortcut(.cancelAction)
            Button(confirmation.accept, role: confirmation.isDestructive ? .destructive : nil) {
                model.accept(confirmation.kind)
            }
            .keyboardShortcut(confirmation.isDestructive ? nil : .defaultAction)
        }
    }

    private var symbol: String {
        switch confirmation.kind {
        case .transfer: "arrow.triangle.branch"
        case .importRows: "square.and.arrow.down.on.square"
        case .forceApply: "exclamationmark.octagon"
        case .clearQueue: "trash"
        }
    }

    /// The fingerprint verdict and what skipping it risks, ahead of the plan.
    private var waiver: String {
        confirmation.lines.prefix(2).map(\.text).joined(separator: "\n\n")
    }

    /// Destinations a plan could not be built for, as the session lists them.
    private var skipped: [String] {
        guard let i = confirmation.lines.firstIndex(where: { $0.text == "Skipped:" }) else { return [] }
        return confirmation.lines[(i + 1)...].map { $0.text.trimmingCharacters(in: .whitespaces) }
    }
}

struct Callout: View {
    let tone: Tone
    let text: String

    var body: some View {
        Label {
            Text(text).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
        } icon: {
            Image(systemName: tone == .bad ? "exclamationmark.octagon.fill" : "exclamationmark.triangle.fill")
                .foregroundStyle(tone.color)
        }
        .font(.callout)
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tone.color.opacity(0.1), in: .rect(cornerRadius: 8))
    }
}

struct PlanCard: View {
    let plan: PlanReview

    var body: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 12) {
                VStack(alignment: .leading, spacing: 1) {
                    Text(plan.dstTitle.isEmpty ? "(untitled)" : plan.dstTitle).font(.headline)
                    Text(plan.dstArtist).foregroundStyle(.secondary)
                }
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 5) {
                    GridRow {
                        Color.clear.gridCellUnsizedAxes([.horizontal, .vertical])
                        Text("Now")
                        Color.clear.gridCellUnsizedAxes([.horizontal, .vertical])
                        Text("After")
                    }
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    ForEach(plan.changes) { c in
                        GridRow {
                            Text(c.label).foregroundStyle(.secondary)
                            Text(c.before.isEmpty ? "—" : c.before)
                            Image(systemName: "arrow.right")
                                .font(.caption)
                                .foregroundStyle(c.changed ? AnyShapeStyle(.tint) : AnyShapeStyle(.quaternary))
                            Text(c.after.isEmpty ? "—" : c.after)
                                .fontWeight(c.changed ? .semibold : .regular)
                                .foregroundStyle(c.changed ? .primary : .secondary)
                        }
                        .monospacedDigit()
                    }
                }
                if let length = plan.lengthMs {
                    Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 6) {
                        GridRow {
                            Text("Now").font(.caption).foregroundStyle(.secondary)
                            CueStrip(marks: plan.cuesBefore, lengthMs: length).frame(height: 18)
                        }
                        GridRow {
                            Text("After").font(.caption).foregroundStyle(.secondary)
                            CueStrip(marks: plan.cuesAfter, lengthMs: length).frame(height: 18)
                        }
                    }
                }
                ForEach(plan.warnings, id: \.self) { Callout(tone: .warn, text: $0) }
            }
            .padding(6)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// One row per file to import, with what was read from it.
struct ImportList: View {
    let lines: [Confirmation.Line]

    private struct Row: Identifiable {
        let id: Int
        let title: String
        var details: [String]
    }

    var body: some View {
        ForEach(rows) { row in
            GroupBox {
                VStack(alignment: .leading, spacing: 3) {
                    Text(row.title).font(.headline)
                    ForEach(row.details, id: \.self) { Text($0).font(.callout).foregroundStyle(.secondary) }
                }
                .textSelection(.enabled)
                .padding(4)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    /// The session writes each file as a heading line and indented details.
    private var rows: [Row] {
        var out: [Row] = []
        for line in lines {
            if line.tone == .good || out.isEmpty {
                out.append(Row(id: out.count, title: line.text, details: []))
            } else {
                out[out.count - 1].details.append(line.text.trimmingCharacters(in: .whitespaces))
            }
        }
        return out
    }
}
