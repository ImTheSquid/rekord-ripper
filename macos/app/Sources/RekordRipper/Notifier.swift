import AppKit
import UserNotifications

/// System notifications for what finishes while the app is in the background.
@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    static let shared = Notifier()

    /// Called with the confirmation a clicked notification was about, if any.
    var onOpen: ((Confirmation.Kind?) -> Void)?
    private var authorised = false

    func start() {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
            Task { @MainActor in Notifier.shared.authorised = granted }
        }
    }

    /// Posts only while the app is not in front: in front, the toast says it.
    func post(_ title: String, _ body: String, review: Confirmation.Kind? = nil) {
        guard authorised, !NSApp.isActive else { return }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        if let review { content.userInfo = ["review": Int(review.rawValue)] }
        let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    nonisolated func userNotificationCenter(
        _: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let raw = response.notification.request.content.userInfo["review"] as? Int
        Task { @MainActor in
            NSApp.activate()
            Notifier.shared.onOpen?(raw.flatMap { Confirmation.Kind(rawValue: UInt8($0)) })
        }
        completionHandler()
    }
}
