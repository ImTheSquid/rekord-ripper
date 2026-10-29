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
    enum Origin: UInt8 { case local, cloud, stream }

    let id: String
    let uuid: String
    let title: String
    let artist: String
    /// Hundredths of a beat per minute.
    let bpm: Int64?
    let key: String
    let lengthSecs: Int64?
    let cueCount: Int64
    let locked: Bool
    let fileType: String
    let artworkPath: String?
    let origin: Origin
    /// nil for a stream, or a file rekordbox cannot open.
    let lossless: Bool?
    /// nil when it cannot be told.
    let present: Bool?

    init(_ v: TrackRowViewRef) {
        id = v.id().toString()
        uuid = v.uuid().toString()
        title = v.title().toString()
        artist = v.artist().toString()
        bpm = v.bpm()
        key = v.key().toString()
        lengthSecs = v.length_secs()
        cueCount = v.cue_count()
        locked = v.locked()
        fileType = v.file_type().toString()
        let art = v.artwork_path().toString()
        artworkPath = art.isEmpty ? nil : art
        origin = Origin(rawValue: v.origin()) ?? .local
        lossless = switch v.lossless() {
        case 1: true
        case 0: false
        default: nil
        }
        present = switch v.present() {
        case 1: true
        case 0: false
        default: nil
        }
    }

    var displayTitle: String { title.isEmpty ? "(untitled)" : title }
    var bpmValue: Double { bpm.map { Double($0) / 100 } ?? 0 }
    var bpmText: String { bpm.map { String(format: "%.2f", Double($0) / 100) } ?? "—" }
    var lengthText: String {
        guard let s = lengthSecs else { return "—" }
        return String(format: "%d:%02d", s / 60, s % 60)
    }
    var missing: Bool { present == false }
    var analysed: Bool { locked || cueCount > 0 }
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
}

extension StatusMessage.Level {
    var tone: Tone {
        switch self {
        case .info: .plain
        case .warn: .warn
        case .error: .bad
        case .ok: .good
        }
    }

    var symbol: String {
        switch self {
        case .info: "info.circle"
        case .warn: "exclamationmark.triangle"
        case .error: "xmark.octagon"
        case .ok: "checkmark.circle"
        }
    }
}

/// The step a notice or queue entry points at.
enum Step: UInt8 {
    case none, openBuyPage, research, fillBasket, nextBasketBatch, importDownloads, checkDownloads, force,
         forget, review, allowWrites

    /// A button title, or nil where the app has nothing to offer: it never
    /// searches an empty selection, so an empty basket cannot come up.
    var title: String? {
        switch self {
        case .none, .fillBasket: nil
        case .nextBasketBatch: "Search Next Batch"
        case .allowWrites: "Allow Writes…"
        case .openBuyPage: "Open Buy Page"
        case .research: "Search Again"
        case .importDownloads: "Import"
        case .checkDownloads: "Check"
        case .force: "Force…"
        case .forget: "Forget"
        case .review: "Review…"
        }
    }
}

struct Notice: Identifiable, Equatable {
    let id: UInt64
    let level: StatusMessage.Level
    let text: String
    let step: Step

    init(id: UInt64, level: StatusMessage.Level, text: String, step: Step) {
        self.id = id
        self.level = level
        self.text = text
        self.step = step
    }

    init(_ v: NoticeViewRef) {
        id = v.id()
        level = StatusMessage.Level(rawValue: v.level()) ?? .info
        text = v.text().toString()
        step = Step(rawValue: v.action()) ?? .none
    }
}

/// What the library table shows.
enum Scope: Hashable {
    /// A `CollectionCount.code`.
    case collection(UInt8)
    case playlist(String)

    static let library = Scope.collection(0)
}

struct CollectionCount: Identifiable, Equatable {
    let code: UInt8
    let title: String
    let count: UInt32

    var id: UInt8 { code }

    init(_ v: CollectionViewRef) {
        code = v.code()
        title = v.title().toString()
        count = v.count()
    }

    var symbol: String {
        switch code {
        case 0: "music.note.list"
        case 1: "arrow.up.circle"
        case 2: "waveform.badge.exclamationmark"
        case 3: "questionmark.folder"
        default: "wand.and.stars"
        }
    }
}

struct PlaylistNode: Identifiable, Hashable {
    let id: String
    let name: String
    let folder: Bool
    let smart: Bool
    /// nil for a playlist, so an outline shows no disclosure triangle.
    var children: [PlaylistNode]?

    /// Nests the flat, parent-first list the session reports.
    static func tree(_ list: RustVec<PlaylistView>) -> [PlaylistNode] {
        var byParent: [String: [(String, String, Bool, Bool)]] = [:]
        for v in list {
            let entry = (v.id().toString(), v.name().toString(), v.folder(), v.smart())
            byParent[v.parent().toString(), default: []].append(entry)
        }
        func build(_ parent: String) -> [PlaylistNode] {
            (byParent[parent] ?? []).map { id, name, folder, smart in
                PlaylistNode(id: id, name: name, folder: folder, smart: smart,
                             children: folder ? build(id) : nil)
            }
        }
        // Top-level nodes report an empty parent.
        return build("")
    }
}

/// One `config.toml` key, as the core describes it.
struct Setting: Identifiable, Equatable {
    enum Kind: UInt8 {
        case bool, integer, number, text, path, list, choice
    }

    struct Choice: Hashable {
        let value: String
        let label: String
    }

    /// `section.key`, as in the file.
    let id: String
    let group: String
    let label: String
    let help: String
    let kind: Kind
    let range: ClosedRange<Double>
    let choices: [Choice]
    let value: String

    init(_ v: SettingViewRef) {
        id = v.key().toString()
        group = v.group().toString()
        label = v.label().toString()
        help = v.help().toString()
        kind = Kind(rawValue: v.kind()) ?? .text
        range = v.min()...max(v.min(), v.max())
        choices = zip(v.choices(), v.choice_labels()).map {
            Choice(value: $0.as_str().toString(), label: $1.as_str().toString())
        }
        value = v.value().toString()
    }
}

/// One `credentials.toml` key. A secret's value never leaves the core.
struct CredentialSetting: Identifiable, Equatable {
    let id: String
    let group: String
    let label: String
    let secret: Bool
    let set: Bool
    /// An environment variable or a file is used instead of what is saved.
    let overridden: Bool
    let note: String
    let value: String

    init(_ v: CredentialViewRef) {
        id = v.key().toString()
        group = v.group().toString()
        label = v.label().toString()
        secret = v.secret()
        set = v.set()
        overridden = v.overridden()
        note = v.note().toString()
        value = v.value().toString()
    }
}

struct Suggestion: Identifiable, Equatable {
    let id: String
    /// Analysis would go from the inspected track to this one.
    let outgoing: Bool
}

struct CueMark: Equatable {
    let ms: Int64
    let endMs: Int64?
    /// 0 for a memory cue, otherwise a hot cue slot.
    let kind: Int64

    init(_ v: CueMarkViewRef) {
        ms = v.ms()
        let end = v.end_ms()
        endMs = end >= 0 ? end : nil
        kind = v.kind()
    }

    var hot: Bool { kind != 0 }
}

/// What the inspector shows about one track beyond its row.
struct Inspection: Equatable {
    let trackId: String
    var suggestions: [Suggestion] = []
    var cueMarks: [CueMark] = []
}

struct ShopStatus: Equatable {
    enum Kind: UInt8 { case idle, searching, results, failed }
    var kind = Kind.idle
    var text = ""
    var done: UInt32 = 0
    var total: UInt32 = 0
    var searchesOutstanding: UInt32 = 0
    var elapsedSecs: UInt64 = 0

    init() {}

    init(_ s: ShopStatusFfi) {
        kind = Kind(rawValue: s.kind) ?? .idle
        text = s.text.toString()
        done = s.done
        total = s.total
        searchesOutstanding = s.searches_outstanding
        elapsedSecs = s.elapsed_secs
    }

    var busy: Bool { kind == .searching || searchesOutstanding > 0 }
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
    let fetchable: Bool
    let fetch: Fetch
    let matchScore: Int64

    init(_ v: OfferViewRef) {
        srcId = v.src_id().toString()
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
        fetchable = v.fetchable()
        fetch = Fetch(rawValue: v.fetch()) ?? .none
        matchScore = v.match_score()
    }

    /// "FLAC · £1.00 · bandcamp"
    var summary: String {
        [formats, price, backend].filter { !$0.isEmpty }.joined(separator: " · ")
    }
}

/// One backend's part in a track's search.
struct BackendAnswer: Hashable {
    let backend: String
    let offers: Int
    /// Why it failed; empty when it answered.
    let error: String

    init(_ v: BackendAnswerViewRef) {
        backend = v.backend().toString()
        offers = Int(v.offers())
        error = v.error().toString()
    }
}

struct QueueEntry: Identifiable, Hashable {
    let id: Int64
    let srcId: String
    let title: String
    let artist: String
    let fileName: String
    let state: String
    let tone: Tone
    let step: Step
    let inFlight: Bool

    init(_ v: QueueEntryViewRef) {
        id = v.id()
        srcId = v.src_id().toString()
        title = v.title().toString()
        artist = v.artist().toString()
        fileName = v.file_name().toString()
        state = v.state().toString()
        tone = Tone(rawValue: v.tone()) ?? .plain
        step = Step(rawValue: v.action()) ?? .none
        inFlight = v.in_flight()
    }
}

/// One track's change under a transfer, as before and after.
struct PlanReview: Identifiable, Equatable {
    struct Change: Identifiable, Equatable {
        let label: String
        let before: String
        let after: String

        var id: String { label }
        var changed: Bool { before != after }
    }

    let srcId: String
    let srcTitle: String
    let srcArtist: String
    let dstId: String
    let dstTitle: String
    let dstArtist: String
    let changes: [Change]
    let warnings: [String]
    let lengthMs: Int64?
    let cuesBefore: [CueMark]
    let cuesAfter: [CueMark]

    var id: String { dstId }

    init(_ v: PlanReviewViewRef) {
        srcId = v.src_id().toString()
        srcTitle = v.src_title().toString()
        srcArtist = v.src_artist().toString()
        dstId = v.dst_id().toString()
        dstTitle = v.dst_title().toString()
        dstArtist = v.dst_artist().toString()
        changes = v.changes().map {
            Change(label: $0.label().toString(), before: $0.before().toString(), after: $0.after().toString())
        }
        warnings = v.warnings().map { $0.as_str().toString() }
        let len = v.length_ms()
        lengthMs = len > 0 ? len : nil
        cuesBefore = v.cues_before().map(CueMark.init)
        cuesAfter = v.cues_after().map(CueMark.init)
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
    /// For a transfer or force-apply: what each track would become.
    var plans: [PlanReview] = []

    var id: Kind { kind }
    var isDestructive: Bool { kind == .forceApply || kind == .clearQueue }

    var waitingLabel: String {
        switch kind {
        case .transfer: "Transfer ready to review"
        case .importRows: "Downloads ready to import"
        case .forceApply: "Unverified transfer ready to review"
        case .clearQueue: "Clearing the queue"
        }
    }
}

/// Where a track has got to in finding and fitting a better copy.
enum Upgrade: Equatable {
    case searching
    case notFound
    case found(Offer, count: Int)
    case downloading
    case entry(QueueEntry)

    var text: String {
        switch self {
        case .searching: "Searching…"
        case .notFound: "No better copy found"
        case let .found(offer, count): count > 1 ? "\(offer.summary) +\(count - 1)" : offer.summary
        case .downloading: "Downloading…"
        case let .entry(e): e.state
        }
    }

    var tone: Tone {
        switch self {
        case .searching, .downloading: .active
        case .notFound: .muted
        case let .found(offer, _): offer.requiresPurchase ? .plain : .good
        case let .entry(e): e.tone
        }
    }
}

/// What can be dropped on a track row.
enum TrackDrop: Transferable {
    case track(Track.ID)
    case file(URL)

    /// Marks a dragged track id, so dropped text is not taken for one.
    static let prefix = "rekord-ripper-track:"

    struct NotATrack: Error {}

    static var transferRepresentation: some TransferRepresentation {
        ProxyRepresentation(importing: { (url: URL) in TrackDrop.file(url) })
        ProxyRepresentation(importing: { (text: String) in
            guard text.hasPrefix(prefix) else { throw NotATrack() }
            return TrackDrop.track(String(text.dropFirst(prefix.count)))
        })
    }
}
