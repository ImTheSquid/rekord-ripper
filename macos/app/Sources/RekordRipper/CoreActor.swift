import Foundation
import RekordRipperBridge

struct OpenError: Error {
    let message: String
}

/// What the window currently needs read, beyond what it always needs.
struct PollRequest: Equatable {
    var scope = Scope.library
    var query = ""
    var inspected: String?
    /// Tracks whose search state is worth asking after.
    var searched: Set<String> = []
}

/// One pass over the session, taken off the main thread.
///
/// The large parts are sent only when they changed, so the main actor never
/// compares or copies the whole library on a tick.
struct Snapshot {
    /// Set when the rows were reloaded.
    var tracks: [Track]?
    var playlists: [PlaylistNode]?
    var collections: [CollectionCount]?
    /// Indices into the newest `tracks`; set when they changed.
    var visible: [Int]?
    /// Set when the inspected track or its data changed.
    var inspection: Inspection??
    var notices: [Notice] = []
    var status = StatusMessage()
    var rekordboxRunning = false
    var bypassRekordboxCheck = false
    var copyReplace = false
    var copyLock = false
    var confirmations: [Confirmation] = []
    var shop = ShopStatus()
    var offers: [Offer] = []
    var shopStates: [String: Int64] = [:]
    var fetch = FetchStatus()
    var fetchQueue: [String] = []
    var entries: [QueueEntry] = []
    var unresolvedErrors = false
    var workInFlight = ""
}

/// The one owner of the Rust session.
///
/// Its own serial queue rather than the shared pool: an apply backs up
/// master.db first, and that must not hold a cooperative thread while it runs.
actor CoreActor {
    private let queue = DispatchSerialQueue(label: "rekord-ripper.core", qos: .userInitiated)
    nonisolated var unownedExecutor: UnownedSerialExecutor { queue.asUnownedSerialExecutor() }

    private let core: CoreSession
    private var revision: UInt64 = .max
    private var lastNotice: UInt64 = 0
    private var latestNotice: Notice?
    private var visibleKey: VisibleKey?
    private var visible: [Int]?
    private var inspectionKey: InspectionKey?
    private var reviews: [UInt8: (lines: [Confirmation.Line], plans: [PlanReview])] = [:]

    private struct VisibleKey: Equatable {
        let revision: UInt64
        let scope: Scope
        let query: String
    }

    private struct InspectionKey: Equatable {
        let revision: UInt64
        let id: String?
    }

    init() throws {
        do {
            core = try CoreSession.open()
        } catch let e as RustString {
            throw OpenError(message: e.toString())
        }
    }

    /// Runs `body` against the session, between polls.
    func run<T: Sendable>(_ body: @Sendable (CoreSession) -> T) -> T {
        body(core)
    }

    func poll(_ req: PollRequest) -> Snapshot {
        core.poll()
        var s = Snapshot()

        let rev = core.rows_revision()
        if rev != revision {
            revision = rev
            s.tracks = core.rows().map(Track.init)
            s.playlists = PlaylistNode.tree(core.playlists())
            s.collections = core.collections().map(CollectionCount.init)
        }

        let vkey = VisibleKey(revision: rev, scope: req.scope, query: req.query)
        if vkey != visibleKey {
            visibleKey = vkey
            let raw = switch req.scope {
            case let .collection(code): core.collection_visible(code, req.query)
            case let .playlist(id): core.playlist_visible(id, req.query)
            }
            let v = raw.map { Int($0) }
            if v != visible || s.tracks != nil {
                visible = v
                s.visible = v
            }
        }

        let ikey = InspectionKey(revision: rev, id: req.inspected)
        if ikey != inspectionKey {
            inspectionKey = ikey
            s.inspection = .some(req.inspected.map(inspect))
        }

        s.notices = core.notices(lastNotice).map(Notice.init)
        lastNotice = s.notices.last?.id ?? lastNotice
        // A notice can be reworded in place, which keeps its id.
        if let latest = core.latest_notice().map(Notice.init).first, latest != latestNotice {
            latestNotice = latest
            if s.notices.last?.id != latest.id { s.notices.append(latest) }
        }
        s.status = StatusMessage(core.status())
        s.rekordboxRunning = core.rekordbox_running()
        s.bypassRekordboxCheck = core.bypass_rekordbox_check()
        s.copyReplace = core.copy_replace()
        s.copyLock = core.copy_lock()
        s.confirmations = core.open_confirms().compactMap(confirmation)
        s.shop = ShopStatus(core.shop_status())
        s.offers = core.offers().map(Offer.init)
        for id in req.searched {
            s.shopStates[id] = core.shop_track_state(id)
        }
        s.fetch = FetchStatus(core.fetch_status())
        s.fetchQueue = core.fetch_queue().map { $0.as_str().toString() }
        s.entries = core.queue_entries().map(QueueEntry.init)
        s.unresolvedErrors = core.unresolved_errors()
        s.workInFlight = core.work_in_flight().toString()
        return s
    }

    private func inspect(_ id: String) -> Inspection {
        Inspection(
            trackId: id,
            suggestions: core.suggestions(id).map { Suggestion(id: $0.id().toString(), outgoing: $0.outgoing()) },
            cueMarks: core.cue_marks(id).map(CueMark.init)
        )
    }

    private func confirmation(_ code: UInt8) -> Confirmation? {
        let c = core.confirm_for(code)
        guard let kind = Confirmation.Kind(rawValue: c.kind) else { return nil }
        let lines = core.confirm_lines_for(code).map {
            Confirmation.Line(tone: Tone(rawValue: $0.tone()) ?? .plain, text: $0.text().toString())
        }
        // The lines name every track and warning, so equal lines mean the same
        // batch, and its cues need not be read again.
        let plans: [PlanReview]
        if let cached = reviews[code], cached.lines == lines {
            plans = cached.plans
        } else {
            plans = core.plan_reviews(code).map(PlanReview.init)
            reviews[code] = (lines, plans)
        }
        return Confirmation(kind: kind, title: c.title.toString(), accept: c.accept.toString(), lines: lines,
                            plans: plans)
    }
}
