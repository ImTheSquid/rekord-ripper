import Foundation
import Observation
import RekordRipperBridge

/// The window's state, fed by `CoreActor` snapshots.
///
/// Main-actor only, and never touches Rust: every read arrives in a snapshot
/// and every action is sent to the actor, so a slow write cannot stall the UI.
@MainActor
@Observable
final class SessionModel {
    private(set) var openError: String?
    @ObservationIgnored private var core: CoreActor?
    @ObservationIgnored private var loop: Task<Void, Never>?
    @ObservationIgnored private var sleeper: Task<Void, Never>?

    // Library.
    private(set) var tracks: [Track] = []
    @ObservationIgnored private var trackIndex: [Track.ID: Int] = [:]
    @ObservationIgnored private var visibleIndices: [Int] = []
    private(set) var visibleTracks: [Track] = []
    private(set) var playlists: [PlaylistNode] = []
    private(set) var collections: [CollectionCount] = []
    var scope = Scope.library { didSet { if scope != oldValue { kick() } } }
    var query = "" { didSet { if query != oldValue { kick() } } }
    var selection = Set<Track.ID>() { didSet { if selection != oldValue { kick() } } }
    private(set) var inspection: Inspection?

    // Session state.
    private(set) var status = StatusMessage()
    private(set) var rekordboxRunning = false
    private(set) var bypassRekordboxCheck = false
    private(set) var copyReplace = false
    private(set) var copyLock = false
    private(set) var shop = ShopStatus()
    private(set) var offers: [Offer] = []
    @ObservationIgnored private var offersBySrc: [String: [Offer]] = [:]
    private(set) var shopStates: [String: Int64] = [:]
    @ObservationIgnored private var searched = Set<String>()
    private(set) var fetch = FetchStatus()
    private(set) var fetchQueue: [String] = []
    private(set) var entries: [QueueEntry] = []
    @ObservationIgnored private var entriesBySrc: [String: [QueueEntry]] = [:]
    @ObservationIgnored private var unresolvedErrors = false
    @ObservationIgnored private var workInFlight = ""

    // Notices.
    private(set) var notices: [Notice] = []
    private(set) var toast: Notice?
    @ObservationIgnored private var toastTimer: Task<Void, Never>?
    @ObservationIgnored private var polledOnce = false
    @ObservationIgnored private var localNotices: UInt64 = 0

    // Confirmations.
    private(set) var confirmations: [Confirmation] = []
    /// The one shown as a sheet. Others wait in the activity popover.
    var presented: Confirmation.Kind?
    /// Kinds the user just asked for, so theirs open as a sheet while one the
    /// pipeline opened waits to be looked at.
    @ObservationIgnored private var requested: [Confirmation.Kind: Date] = [:]

    var activityShown = false
    var inspectorShown = true
    private(set) var autoAdvance = true
    private(set) var importEnabled = true
    private(set) var configPath = ""

    init() {
        do {
            core = try CoreActor()
        } catch let e as OpenError {
            openError = e.message
            return
        } catch {
            openError = "\(error)"
            return
        }
        let defaults = UserDefaults.standard
        setAutoAdvance(defaults.object(forKey: "autoAdvance") as? Bool ?? true)
        if defaults.object(forKey: "replaceCues") != nil {
            setCopyOptions(replace: defaults.bool(forKey: "replaceCues"), lock: defaults.bool(forKey: "lockAfterCopy"))
        }
        Notifier.shared.onOpen = { [weak self] kind in
            if let kind { self?.present(kind) }
        }
        Notifier.shared.start()
        loop = Task { [weak self] in
            guard let core = self?.core else { return }
            let (enabled, path) = await core.run { ($0.import_enabled(), $0.config_path().toString()) }
            self?.importEnabled = enabled
            self?.configPath = path
            await self?.pollLoop()
        }
    }

    // MARK: Polling

    private func pollLoop() async {
        while !Task.isCancelled, let core {
            apply(await core.poll(request))
            let nap = Task { _ = try? await Task.sleep(for: .milliseconds(200)) }
            sleeper = nap
            await nap.value
        }
    }

    /// Polls now rather than at the next tick.
    private func kick() {
        sleeper?.cancel()
    }

    private var request: PollRequest {
        PollRequest(
            scope: scope,
            query: query,
            inspected: selection.count == 1 ? selection.first : nil,
            searched: searched
        )
    }

    private func apply(_ s: Snapshot) {
        if let t = s.tracks {
            tracks = t
            trackIndex = Dictionary(uniqueKeysWithValues: t.enumerated().map { ($1.id, $0) })
            let ids = Set(trackIndex.keys)
            if !selection.isSubset(of: ids) { selection.formIntersection(ids) }
        }
        if let p = s.playlists, p != playlists { playlists = p }
        if let c = s.collections, c != collections { collections = c }
        if let v = s.visible { visibleIndices = v }
        if s.tracks != nil || s.visible != nil {
            visibleTracks = visibleIndices.compactMap { $0 < tracks.count ? tracks[$0] : nil }
        }
        if case let .some(i) = s.inspection, i != inspection { inspection = i }

        assign(\.status, s.status)
        assign(\.rekordboxRunning, s.rekordboxRunning)
        assign(\.bypassRekordboxCheck, s.bypassRekordboxCheck)
        assign(\.copyReplace, s.copyReplace)
        assign(\.copyLock, s.copyLock)
        assign(\.shop, s.shop)
        if s.offers != offers {
            offers = s.offers
            offersBySrc = Dictionary(grouping: s.offers, by: \.srcId)
        }
        assign(\.shopStates, s.shopStates)
        if fetch.kind == .running, s.fetch.kind == .done {
            Notifier.shared.post("Download finished", s.fetch.text)
        } else if fetch.kind == .running, s.fetch.kind == .failed {
            Notifier.shared.post("Download failed", s.fetch.text)
        }
        assign(\.fetch, s.fetch)
        assign(\.fetchQueue, s.fetchQueue)
        if s.entries != entries {
            notifySettled(old: entries, new: s.entries)
            entries = s.entries
            entriesBySrc = Dictionary(grouping: s.entries, by: \.srcId)
        }
        unresolvedErrors = s.unresolvedErrors
        workInFlight = s.workInFlight

        receive(s.notices)
        receive(s.confirmations)
        polledOnce = true
    }

    /// Writes only on change: an @Observable property notifies on every set.
    private func assign<T: Equatable>(_ key: ReferenceWritableKeyPath<SessionModel, T>, _ value: T) {
        if self[keyPath: key] != value { self[keyPath: key] = value }
    }

    private func receive(_ new: [Notice]) {
        guard !new.isEmpty else { return }
        notices = Array((notices + new).suffix(50))
        // What was said before the window opened is history, not news.
        guard polledOnce, let latest = new.last else { return }
        toast = latest
        toastTimer?.cancel()
        toastTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(latest.step.title == nil ? 4 : 8))
            guard !Task.isCancelled else { return }
            if self?.toast?.id == latest.id { self?.toast = nil }
        }
    }

    private func receive(_ open: [Confirmation]) {
        let before = Set(confirmations.map(\.kind))
        if open != confirmations { confirmations = open }
        if let p = presented, !open.contains(where: { $0.kind == p }) { presented = nil }
        for c in open where !before.contains(c.kind) {
            let asked = requested.removeValue(forKey: c.kind).map { -$0.timeIntervalSinceNow < 120 } ?? false
            if asked, presented == nil {
                presented = c.kind
            } else if !asked {
                Notifier.shared.post(c.waitingLabel, c.title, review: c.kind)
            }
        }
    }

    /// Entries the pipeline just applied or rejected.
    private func notifySettled(old: [QueueEntry], new: [QueueEntry]) {
        let before = Dictionary(uniqueKeysWithValues: old.map { ($0.id, $0.state) })
        for e in new where before[e.id] != nil && before[e.id] != e.state {
            // The core's stored-state labels lead with the state's name.
            let name = e.title.isEmpty ? e.fileName : e.title
            if e.state.hasPrefix("applied") {
                Notifier.shared.post("Better copy applied", name)
            } else if e.state.hasPrefix("rejected") {
                Notifier.shared.post("Better copy rejected", "\(name): \(e.state)")
            }
        }
    }

    /// A notice from the app itself rather than the session.
    private func say(_ text: String, _ level: StatusMessage.Level) {
        localNotices += 1
        let notice = Notice(id: .max - localNotices, level: level, text: text, step: .none)
        notices = Array((notices + [notice]).suffix(50))
        toast = notice
    }

    // MARK: Reading

    func track(_ id: Track.ID?) -> Track? {
        guard let id, let i = trackIndex[id] else { return nil }
        return tracks[i]
    }

    var selectedTracks: [Track] { selection.compactMap(track) }

    var presentedConfirmation: Confirmation? {
        confirmations.first { $0.kind == presented }
    }

    /// Open confirmations not on screen.
    var waiting: [Confirmation] {
        confirmations.filter { $0.kind != presented }
    }

    var busy: Bool {
        shop.busy || fetch.kind == .running || entries.contains(where: \.inFlight)
    }

    func offers(for id: Track.ID) -> [Offer] { offersBySrc[id] ?? [] }

    func entries(for id: Track.ID) -> [QueueEntry] { entriesBySrc[id] ?? [] }

    func upgrade(for id: Track.ID) -> Upgrade? {
        if let e = entriesBySrc[id]?.max(by: { $0.id < $1.id }) { return .entry(e) }
        let found = offersBySrc[id] ?? []
        if found.contains(where: { $0.fetch == .running || $0.fetch == .queued }) { return .downloading }
        switch shopStates[id] {
        case .some(-1): return .searching
        case .some(0): return .notFound
        default: break
        }
        if let best = found.first { return .found(best, count: found.count) }
        return nil
    }

    // MARK: Actions

    private func send(_ body: @escaping @Sendable (CoreSession) -> Void) {
        guard let core else { return }
        Task {
            await core.run(body)
            kick()
        }
    }

    private func ask(_ kind: Confirmation.Kind) {
        requested[kind] = Date()
    }

    func reload() {
        send { $0.reload_rows() }
    }

    func findBetterCopies(_ ids: Set<Track.ID>) {
        searched.formUnion(ids)
        let csv = ids.sorted().joined(separator: ",")
        let one = ids.count == 1 ? ids.first : nil
        send { core in
            if let one { _ = core.shop_track(one) } else { _ = core.shop_basket(csv) }
        }
    }

    func research(_ id: Track.ID) {
        searched.insert(id)
        send { _ = $0.research(id) }
    }

    func download(_ offer: Offer) {
        send { _ = $0.start_fetch(offer.srcId, offer.itemRef, "") }
    }

    func openPage(_ offer: Offer) {
        send { $0.open_offer(offer.srcId, offer.itemRef) }
    }

    /// Ask to copy `src`'s analysis onto `dsts`.
    func reviewTransfer(from src: Track.ID, to dsts: some Sequence<Track.ID>) {
        let csv = dsts.filter { $0 != src }.sorted().joined(separator: ",")
        guard !csv.isEmpty else { return }
        ask(.transfer)
        send { $0.build_pending(src, csv) }
    }

    func importDownloads() {
        ask(.importRows)
        send { $0.start_import() }
    }

    func checkDownloads() {
        send { $0.start_apply() }
    }

    func retry(_ id: QueueEntry.ID) {
        send { $0.retry_entry(id) }
    }

    func forget(_ id: QueueEntry.ID) {
        send { $0.forget_entry(id) }
    }

    func force(_ id: QueueEntry.ID) {
        ask(.forceApply)
        send { $0.start_force(id) }
    }

    func clearQueue() {
        ask(.clearQueue)
        send { $0.start_clear_queue() }
    }

    func perform(_ notice: Notice) {
        guard let core else { return }
        let id = notice.id
        if notice.step == .importDownloads { ask(.importRows) }
        Task {
            let review = await core.run { $0.perform_notice(id) }
            if let kind = Confirmation.Kind(rawValue: review) { presented = kind }
            kick()
        }
    }

    func present(_ kind: Confirmation.Kind) {
        presented = kind
        activityShown = false
    }

    func accept(_ kind: Confirmation.Kind) {
        let code = kind.rawValue
        send { _ = $0.accept(code) }
    }

    func cancel(_ kind: Confirmation.Kind) {
        if presented == kind { presented = nil }
        let code = kind.rawValue
        send { $0.cancel(code) }
    }

    func setCopyOptions(replace: Bool, lock: Bool) {
        UserDefaults.standard.set(replace, forKey: "replaceCues")
        UserDefaults.standard.set(lock, forKey: "lockAfterCopy")
        send { $0.set_copy_opts(replace, lock) }
    }

    func setBypassRekordboxCheck(_ on: Bool) {
        send { $0.set_bypass_rekordbox_check(on) }
    }

    func setAutoAdvance(_ on: Bool) {
        autoAdvance = on
        UserDefaults.standard.set(on, forKey: "autoAdvance")
        send { $0.set_auto_advance(on) }
    }

    /// Something dropped on `target`: another track means copy its analysis
    /// here; an audio file means it is a better copy of this one.
    func drop(_ items: [TrackDrop], on target: Track.ID) {
        let tracks = items.compactMap { if case let .track(id) = $0 { id } else { nil } }
        let files = items.compactMap { if case let .file(url) = $0 { url } else { nil } }
        if let file = files.first {
            guard files.count == 1 else { return say("Drop one file at a time.", .warn) }
            let path = file.path
            send { _ = $0.queue_file(target, path) }
        } else if let src = tracks.first {
            guard tracks.count == 1 else { return say("Drop one track at a time onto the track to copy it to.", .warn) }
            guard src != target else { return }
            reviewTransfer(from: src, to: [target])
        }
    }

    // MARK: Quitting

    /// What a quit would throw away, or nil when nothing would be lost.
    var quitWarning: String? {
        var bits: [String] = []
        if !confirmations.isEmpty { bits.append("\(confirmations.count) confirmation(s) waiting") }
        if unresolvedErrors { bits.append("unresolved apply errors") }
        if !workInFlight.isEmpty { bits.append(workInFlight) }
        return bits.isEmpty ? nil : bits.joined(separator: ", ")
    }
}
