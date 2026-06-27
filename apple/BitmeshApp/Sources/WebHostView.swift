import SwiftUI
import WebKit
import BitmeshCore

/// Hosts the real `nimiq-ui` web wallet (`webui/index.html`, bundled as a folder
/// reference) in a `WKWebView` and bridges it to the Rust core (A1).
///
/// The bridge is **read-only**: `version`, `network`, `meshStatus`, `backupUrgency` (G19).
/// It signs nothing, broadcasts nothing, and never sees key/seed material — so it stays firmly
/// non-money-path. The signing path (Send → enclave → queue) is wired separately, behind
/// Andjroo, in the money-path slice (C1). The radio is not native yet (Phase D), so
/// `meshStatus` honestly reports `offline` / `0 peers` until CoreBluetooth lands.
struct WebHostView: UIViewRepresentable {
    func makeCoordinator() -> Bridge { Bridge() }

    func makeUIView(context: Context) -> WKWebView {
        let controller = WKUserContentController()
        controller.add(context.coordinator, name: Bridge.channel)
        // Expose `window.bitmesh.call(...)` before any page script runs.
        controller.addUserScript(
            WKUserScript(source: Bridge.jsShim, injectionTime: .atDocumentStart, forMainFrameOnly: true)
        )

        let config = WKWebViewConfiguration()
        config.userContentController = controller

        let webView = WKWebView(frame: .zero, configuration: config)
        // Match the wallet's light page background (#f8f8f8) behind the safe areas /
        // during load, instead of a white/black flash.
        webView.isOpaque = false
        webView.backgroundColor = UIColor(red: 0.973, green: 0.973, blue: 0.973, alpha: 1)
        webView.scrollView.backgroundColor = .clear
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        context.coordinator.webView = webView

        if let url = Bundle.main.url(forResource: "index", withExtension: "html", subdirectory: "webui") {
            webView.loadFileURL(url, allowingReadAccessTo: url.deletingLastPathComponent())
        } else {
            // Fail loud in debug if the folder reference didn't make it into the bundle.
            assertionFailure("webui/index.html not found in app bundle")
        }
        return webView
    }

    func updateUIView(_ uiView: WKWebView, context: Context) {}
}

/// The JS↔Swift↔Rust bridge. Receives `{id, method, args}` messages from the page,
/// answers from the Rust core, and resolves the page-side Promise by id.
final class Bridge: NSObject, WKScriptMessageHandler {
    static let channel = "bitmesh"
    weak var webView: WKWebView?

    /// Injected at document start. Exposes a tiny promise-based RPC the web UI calls:
    /// `await window.bitmesh.version()` etc. If the handler is ever absent (e.g. the
    /// same page opened in a desktop browser for screenshot verification), `window.bitmesh`
    /// is simply undefined and the UI degrades gracefully — it never throws on load.
    static let jsShim = """
    (function () {
      if (!(window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.bitmesh)) return;
      var pending = {}, seq = 0;
      function call(method, args) {
        return new Promise(function (resolve, reject) {
          var id = ++seq;
          pending[id] = { resolve: resolve, reject: reject };
          try { window.webkit.messageHandlers.bitmesh.postMessage({ id: id, method: method, args: args || null }); }
          catch (e) { delete pending[id]; reject(e); }
        });
      }
      window.__bitmeshResolve = function (id, ok, payload) {
        var p = pending[id]; if (!p) return; delete pending[id];
        if (ok) p.resolve(payload); else p.reject(new Error(String(payload)));
      };
      window.bitmesh = {
        call: call,
        version: function () { return call('version'); },
        network: function () { return call('network'); },
        meshStatus: function () { return call('meshStatus'); },
        // G19: how hard to nudge a backup, from the account's public state. Read-only —
        // no key/seed crosses; the Rust policy decides (none|gentle|important|critical).
        backupUrgency: function (s) { return call('backupUrgency', s || {}); }
      };
    })();
    """

    func userContentController(_ ucc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? [String: Any],
              let id = body["id"] as? Int,
              let method = body["method"] as? String else { return }
        let (ok, payload) = handle(method: method, args: body["args"])
        resolve(id: id, ok: ok, payload: payload)
    }

    /// All read-only. Anything that would sign/broadcast/keys is intentionally absent
    /// here and lives behind the money-path slice (C1).
    private func handle(method: String, args: Any?) -> (Bool, Any) {
        switch method {
        case "version":
            return (true, ["core": coreVersion()])
        case "network":
            let n = defaultNetwork()
            return (true, [
                "network": n == .testnet ? "testnet" : "mainnet",
                "wireId": Int(networkWireId(network: n)),
                "loopSafe": networkIsLoopSafe(network: n),
            ])
        case "meshStatus":
            // No native BLE radio yet (Phase D). Report the honest state, not a fake one.
            return (true, ["state": "offline", "peers": 0])
        case "backupUrgency":
            // G19: read-only — the Rust policy decides how hard to nudge a backup from the
            // account's public state. No key/seed is read here; only public facts cross.
            let a = args as? [String: Any] ?? [:]
            let state = BackupState(
                backedUp: a["backedUp"] as? Bool ?? false,
                balanceLuna: (a["balanceLuna"] as? NSNumber)?.uint64Value ?? 0,
                daysSinceFirstFunds: (a["daysSinceFirstFunds"] as? NSNumber)?.uint32Value ?? 0
            )
            let level: String
            switch backupUrgency(state: state) {
            case .none: level = "none"
            case .gentle: level = "gentle"
            case .important: level = "important"
            case .critical: level = "critical"
            }
            return (true, ["urgency": level])
        default:
            return (false, "unknown method: \(method)")
        }
    }

    private func resolve(id: Int, ok: Bool, payload: Any) {
        guard let webView = webView else { return }
        let json = (try? JSONSerialization.data(withJSONObject: payload, options: [.fragmentsAllowed]))
            .flatMap { String(data: $0, encoding: .utf8) } ?? "null"
        let js = "window.__bitmeshResolve(\(id), \(ok ? "true" : "false"), \(json));"
        DispatchQueue.main.async { webView.evaluateJavaScript(js) }
    }
}
