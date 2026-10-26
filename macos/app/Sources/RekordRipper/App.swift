import RekordRipperBridge
import SwiftUI

@main
struct RekordRipperApp: App {
    var body: some Scene {
        WindowGroup {
            SpikeView()
        }
    }
}

struct SpikeView: View {
    @State private var text = "Opening…"

    var body: some View {
        Text(text)
            .padding()
            .task {
                do {
                    let session = try Session.open()
                    text = "\(session.row_count()) tracks"
                } catch let e as RustString {
                    text = e.toString()
                } catch {
                    text = "\(error)"
                }
            }
    }
}
