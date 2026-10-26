import Foundation
import Observation
import RekordRipperBridge

enum Screen: String, CaseIterable, Identifiable {
    case transfer = "Transfer"
    case shop = "Shop"
    case queue = "Queue"

    var id: String { rawValue }

    var symbol: String {
        switch self {
        case .transfer: "arrow.left.arrow.right"
        case .shop: "cart"
        case .queue: "tray.full"
        }
    }
}

/// The one owner of the Rust session.
///
/// Main-actor only: the session holds a database connection that must stay on
/// one thread, the same way the TUI drives it from its single loop. A timer
/// polls it, and only what changed is written back, so views redraw on news
/// rather than on every tick.
@MainActor
@Observable
final class SessionModel {
    private(set) var openError: String?
    @ObservationIgnored private var core: CoreSession?
    @ObservationIgnored private var timer: Timer?

    var screen = Screen.transfer

    private(set) var tracks: [Track] = []
    @ObservationIgnored private var rowsRevision: UInt64 = .max
    private(set) var status = StatusMessage()
    private(set) var rekordboxRunning = false
    private(set) var bypassRekordboxCheck = false
    private(set) var confirmation: Confirmation?

    // Transfer screen.
    var srcQuery = "" { didSet { refilterTransfer() } }
    var dstQuery = "" { didSet { refilterDst() } }
    var autoFilter = false { didSet { refilterDst() } }
    var fuzzyFromSrc = false { didSet { refilterDst() } }
    var srcSelection: Track.ID? { didSet { refilterDst() } }
    var dstSelection = Set<Track.ID>()
    private(set) var srcRows: [Track] = []
    private(set) var dstRows: [Track] = []
    private(set) var copyReplace = false
    private(set) var copyLock = false

    // Shop screen.
    var shopQuery = "" { didSet { refilterShop() } }
    var shopSelection = Set<Track.ID>()
    var offerSelection: Offer.ID?
    private(set) var shopRows: [Track] = []
    private(set) var shopStatus = ShopStatus()
    private(set) var offers: [Offer] = []
    private(set) var offerGroups: [OfferGroup] = []
    private(set) var trackStates: [Track.ID: Int64] = [:]
    private(set) var fetchStatus = FetchStatus()
    private(set) var fetchQueue: [String] = []

    // Queue screen.
    var queueSelection: QueueEntry.ID?
    private(set) var queueEntries: [QueueEntry] = []

    init() {
        do {
            core = try CoreSession.open()
        } catch let e as RustString {
            openError = e.toString()
            return
        } catch {
            openError = "\(error)"
            return
        }
        refresh()
        let timer = Timer(timeInterval: 0.2, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    // MARK: Polling

    func refresh() {
        guard let core else { return }
        core.poll()

        let revision = core.rows_revision()
        if revision != rowsRevision {
            rowsRevision = revision
            tracks = core.rows().map(Track.init)
            let ids = Set(tracks.map(\.id))
            dstSelection.formIntersection(ids)
            shopSelection.formIntersection(ids)
            if let src = srcSelection, !ids.contains(src) { srcSelection = nil }
            refilterTransfer()
            refilterShop()
        }

        assign(\.status, StatusMessage(core.status()))
        assign(\.rekordboxRunning, core.rekordbox_running())
        assign(\.bypassRekordboxCheck, core.bypass_rekordbox_check())
        assign(\.copyReplace, core.copy_replace())
        assign(\.copyLock, core.copy_lock())
        assign(\.confirmation, readConfirmation(core))

        let shop = ShopStatus(core.shop_status())
        let newOffers = core.offers().map(Offer.init)
        let offersChanged = newOffers != offers
        assign(\.shopStatus, shop)
        if offersChanged {
            offers = newOffers
            offerGroups = OfferGroup.grouping(newOffers)
            if let id = offerSelection, !newOffers.contains(where: { $0.id == id }) {
                offerSelection = nil
            }
        }
        if offersChanged || shop.kind == .searching || shop.searchesOutstanding > 0 {
            refreshTrackStates()
        }
        assign(\.fetchStatus, FetchStatus(core.fetch_status()))
        assign(\.fetchQueue, core.fetch_queue().map { $0.as_str().toString() })
        assign(\.queueEntries, core.queue_entries().map(QueueEntry.init))
    }

    /// Writes only on change: an @Observable property notifies on every set.
    private func assign<T: Equatable>(_ key: ReferenceWritableKeyPath<SessionModel, T>, _ value: T) {
        if self[keyPath: key] != value { self[keyPath: key] = value }
    }

    private func readConfirmation(_ core: CoreSession) -> Confirmation? {
        let c = core.confirm()
        guard let kind = Confirmation.Kind(rawValue: c.kind) else { return nil }
        let lines = core.confirm_lines().map {
            Confirmation.Line(tone: Tone(rawValue: $0.tone()) ?? .plain, text: $0.text().toString())
        }
        return Confirmation(kind: kind, title: c.title.toString(), accept: c.accept.toString(), lines: lines)
    }

    // MARK: Filtering, in Rust so every frontend shares one query language.

    private func refilterTransfer() {
        guard let core else { return }
        srcRows = core.src_visible(srcQuery).map { tracks[Int($0)] }
        refilterDst()
    }

    private func refilterDst() {
        guard let core else { return }
        let visible = core.dst_visible(dstQuery, autoFilter, srcSelection ?? "", fuzzyFromSrc)
        dstRows = visible.map { tracks[Int($0)] }
    }

    private func refilterShop() {
        guard let core else { return }
        shopRows = core.src_visible(shopQuery).map { tracks[Int($0)] }
        refreshTrackStates()
    }

    private func refreshTrackStates() {
        guard let core else { return }
        var states: [Track.ID: Int64] = [:]
        for row in shopRows {
            let s = core.shop_track_state(row.id)
            if s != -2 { states[row.id] = s }
        }
        assign(\.trackStates, states)
    }

    func track(_ id: Track.ID?) -> Track? {
        guard let id else { return nil }
        return tracks.first { $0.id == id }
    }

    // MARK: Transfer

    var hiddenDstSelection: Int {
        let shown = Set(dstRows.map(\.id))
        return dstSelection.subtracting(shown).count
    }

    func preview(dst: Track.ID?) -> [String] {
        guard let core else { return [] }
        return core.pair_preview(srcSelection ?? "", dst ?? "").map { $0.as_str().toString() }
    }

    func setCopyOptions(replace: Bool, lock: Bool) {
        core?.set_copy_opts(replace, lock)
        refresh()
    }

    func reviewTransfer() {
        guard let src = srcSelection else { return }
        let dsts = dstSelection.filter { $0 != src }.sorted().joined(separator: ",")
        core?.build_pending(src, dsts)
        refresh()
    }

    func reload() {
        core?.reload_rows()
        refresh()
    }

    // MARK: Confirmations

    func accept() {
        guard let core, let kind = confirmation?.kind else { return }
        if core.accept_confirm(), kind == .transfer {
            dstSelection.removeAll()
        }
        refresh()
    }

    func cancelConfirmation() {
        core?.cancel_confirm()
        refresh()
    }

    func setBypassRekordboxCheck(_ on: Bool) {
        core?.set_bypass_rekordbox_check(on)
        refresh()
    }

    // MARK: Shop

    /// Cross to the shop with the transfer screen's source, and search for it.
    func shopForSource() {
        guard let src = srcSelection else { return }
        screen = .shop
        if !shopRows.contains(where: { $0.id == src }) { shopQuery = "" }
        shopSelection = [src]
        _ = core?.shop_track(src)
        refresh()
        focusCursorOffer()
    }

    func search(_ ids: Set<Track.ID>) {
        guard let core else { return }
        if ids.count == 1, let id = ids.first {
            _ = core.shop_track(id)
        } else {
            _ = core.shop_basket(ids.sorted().joined(separator: ","))
        }
        refresh()
        focusCursorOffer()
    }

    func research(_ id: Track.ID?) {
        _ = core?.research(id ?? "")
        refresh()
    }

    /// Select the offer the session points at, such as a newly arrived block.
    private func focusCursorOffer() {
        guard shopStatus.kind == .results, Int(shopStatus.offerCursor) < offers.count else { return }
        offerSelection = offers[Int(shopStatus.offerCursor)].id
    }

    var selectedOffer: Offer? {
        guard let id = offerSelection else { return nil }
        return offers.first { $0.id == id }
    }

    func fetch(_ offer: Offer) {
        _ = core?.start_fetch(offer.srcId, offer.itemRef, shopSelection.count == 1 ? shopSelection.first! : "")
        refresh()
    }

    func openPage(_ offer: Offer) {
        core?.open_offer(offer.srcId, offer.itemRef)
        refresh()
    }

    // MARK: Queue

    func reloadQueue() {
        core?.reload_queue()
        refresh()
    }

    func importDownloads() {
        core?.start_import()
        refresh()
    }

    func checkAndApply() {
        core?.start_apply()
        refresh()
    }

    func retry(_ id: QueueEntry.ID) {
        core?.retry_entry(id)
        refresh()
    }

    func forget(_ id: QueueEntry.ID) {
        core?.forget_entry(id)
        refresh()
    }

    func forceApply(_ id: QueueEntry.ID) {
        core?.start_force(id)
        refresh()
    }

    func clearQueue() {
        core?.start_clear_queue()
        refresh()
    }

    // MARK: Quitting

    /// What a quit would throw away, or nil when nothing would be lost.
    var quitWarning: String? {
        guard let core else { return nil }
        var bits: [String] = []
        if !dstSelection.isEmpty { bits.append("\(dstSelection.count) destination(s) selected") }
        if core.unresolved_errors() { bits.append("unresolved apply errors") }
        let work = core.work_in_flight().toString()
        if !work.isEmpty { bits.append(work) }
        return bits.isEmpty ? nil : bits.joined(separator: ", ")
    }
}
