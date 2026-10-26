import RekordRipperBridge
import SwiftUI

// Plain Swift copies of what the session reports, so views never hold a Rust
// pointer and SwiftUI can diff them.

enum Tone: UInt8 {
    case plain, strong, active, warn, good, bad, muted

    var color: Color {
        switch self {
        case .plain, .strong: .primary
        case .active: .blue
        case .warn: .orange
        case .good: .green
        case .bad: .red
        case .muted: .secondary
        }
    }
}

struct Track: Identifiable, Hashable {
    let id: String
    let title: String
    let artist: String
    /// Hundredths of a beat per minute.
    let bpm: Int64?
    let lengthSecs: Int64?
    let cueCount: Int64
    let locked: Bool
    let fileType: String

    init(_ v: TrackRowViewRef) {
        id = v.id().toString()
        title = v.title().toString()
        artist = v.artist().toString()
        bpm = v.bpm()
        lengthSecs = v.length_secs()
        cueCount = v.cue_count()
        locked = v.locked()
        fileType = v.file_type().toString()
    }

    var displayTitle: String { title.isEmpty ? "(untitled)" : title }
    var bpmText: String { bpm.map { String(format: "%.2f", Double($0) / 100) } ?? "—" }
    var lengthText: String {
        guard let s = lengthSecs else { return "—" }
        return String(format: "%d:%02d", s / 60, s % 60)
    }
}

struct StatusMessage: Equatable {
    enum Level: UInt8 { case info, warn, error, ok }
    var text = ""
    var level = Level.info

    init() {}

    init(_ s: StatusFfi) {
        text = s.text.toString()
        level = Level(rawValue: s.level) ?? .info
    }

    var tone: Tone {
        switch level {
        case .info: .plain
        case .warn: .warn
        case .error: .bad
        case .ok: .good
        }
    }
}

struct ShopStatus: Equatable {
    enum Kind: UInt8 { case idle, searching, results, failed }
    var kind = Kind.idle
    var text = ""
    var done: UInt32 = 0
    var total: UInt32 = 0
    var offerCursor: UInt32 = 0
    var searchesOutstanding: UInt32 = 0
    var elapsedSecs: UInt64 = 0

    init() {}

    init(_ s: ShopStatusFfi) {
        kind = Kind(rawValue: s.kind) ?? .idle
        text = s.text.toString()
        done = s.done
        total = s.total
        offerCursor = s.offer_cursor
        searchesOutstanding = s.searches_outstanding
        elapsedSecs = s.elapsed_secs
    }
}

struct FetchStatus: Equatable {
    enum Kind: UInt8 { case idle, running, done, failed }
    var kind = Kind.idle
    var text = ""
    var note = ""
    var queuedEntry: Int64 = -1
    var elapsedSecs: UInt64 = 0

    init() {}

    init(_ s: FetchStatusFfi) {
        kind = Kind(rawValue: s.kind) ?? .idle
        text = s.text.toString()
        note = s.note.toString()
        queuedEntry = s.queued_entry
        elapsedSecs = s.elapsed_secs
    }
}

struct Offer: Identifiable, Hashable {
    enum Fetch: UInt8 { case none, queued, running, done }

    /// Track and item together: one offer can answer two tracks' searches.
    var id: String { "\(srcId)|\(itemRef)" }
    let srcId: String
    let groupLabel: String
    let itemRef: String
    let backend: String
    let artist: String
    let title: String
    let album: String
    let formats: String
    let price: String
    let ownership: String
    let url: String
    let note: String
    /// nil when its formats have not been checked.
    let lossless: Bool?
    let requiresPurchase: Bool
    let fetch: Fetch
    let matchScore: Int64

    init(_ v: OfferViewRef) {
        srcId = v.src_id().toString()
        groupLabel = v.group_label().toString()
        itemRef = v.item_ref().toString()
        backend = v.backend().toString()
        artist = v.artist().toString()
        title = v.title().toString()
        album = v.album().toString()
        formats = v.formats().toString()
        price = v.price().toString()
        ownership = v.ownership().toString()
        url = v.url().toString()
        note = v.note().toString()
        lossless = switch v.lossless() {
        case 1: true
        case 0: false
        default: nil
        }
        requiresPurchase = v.requires_purchase()
        fetch = Fetch(rawValue: v.fetch()) ?? .none
        matchScore = v.match_score()
    }
}

struct OfferGroup: Identifiable, Equatable {
    let id: String
    let label: String
    let offers: [Offer]

    static func grouping(_ offers: [Offer]) -> [OfferGroup] {
        var groups: [OfferGroup] = []
        for offer in offers {
            if let last = groups.last, last.id == offer.srcId {
                groups[groups.count - 1] = OfferGroup(id: last.id, label: last.label, offers: last.offers + [offer])
            } else {
                groups.append(OfferGroup(id: offer.srcId, label: offer.groupLabel, offers: [offer]))
            }
        }
        return groups
    }
}

struct QueueEntry: Identifiable, Hashable {
    let id: Int64
    let title: String
    let artist: String
    let fileName: String
    let state: String
    let tone: Tone
    let inFlight: Bool

    init(_ v: QueueEntryViewRef) {
        id = v.id()
        title = v.title().toString()
        artist = v.artist().toString()
        fileName = v.file_name().toString()
        state = v.state().toString()
        tone = Tone(rawValue: v.tone()) ?? .plain
        inFlight = v.in_flight()
    }
}

struct Confirmation: Equatable, Identifiable {
    enum Kind: UInt8 { case transfer = 1, importRows, forceApply, clearQueue }
    struct Line: Equatable, Hashable {
        let tone: Tone
        let text: String
    }

    let kind: Kind
    let title: String
    let accept: String
    let lines: [Line]

    var id: String { "\(kind.rawValue)|\(title)" }
    var isDestructive: Bool { kind == .forceApply || kind == .clearQueue }
}
