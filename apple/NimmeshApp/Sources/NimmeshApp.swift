import SwiftUI

/// nimiq.nimmesh — the iOS app is a thin native host around two shared pieces:
///   1. the **web `nimiq-ui` layer** (`webui/`), rendered in a `WKWebView` (A1), and
///   2. the **Rust core** (`nimmesh-core` via UniFFI), reached over a JS↔Swift bridge.
/// The native shell owns only what must be native: the WebView host now, the
/// CoreBluetooth radio shim later (Phase D / ADR-0002). No UI is hand-built in Swift —
/// the rejected SwiftUI home was replaced by the real wallet UI behind this host.
@main
struct NimmeshApp: App {
    var body: some Scene {
        WindowGroup {
            WebHostView()
                .ignoresSafeArea()
                // The web UI is the light wallet; force a dark status bar over it.
                .preferredColorScheme(.light)
        }
    }
}
