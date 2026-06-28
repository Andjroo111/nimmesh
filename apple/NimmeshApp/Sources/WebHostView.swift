import Security
import SwiftUI
import WebKit
import NimmeshCore

/// Hosts the real `nimiq-ui` web wallet (`webui/index.html`, bundled as a folder
/// reference) in a `WKWebView` and bridges it to the Rust core (A1).
///
/// The bridge is **read-only**: `version`, `network`, `meshStatus`, `reachability` (G16),
/// `backupUrgency` (G19).
/// It signs nothing, broadcasts nothing, and never sees key/seed material — so it stays firmly
/// non-money-path. The testnet send path (sign with the Keychain key → broadcast) is the C1c
/// `sendTransaction` async bridge. The offline BLE mesh is the native `BleMeshRadio` (G5)
/// driving a `MeshNode`; `meshStatus`/`reachability` read it live (0 peers on the simulator,
/// real peers on device — the 2-phone interop test).
struct WebHostView: UIViewRepresentable {
    func makeCoordinator() -> Bridge { Bridge() }

    func makeUIView(context: Context) -> WKWebView {
        let controller = WKUserContentController()
        controller.add(context.coordinator, name: Bridge.channel)
        // Expose `window.nimmesh.call(...)` before any page script runs.
        controller.addUserScript(
            WKUserScript(source: Bridge.jsShim, injectionTime: .atDocumentStart, forMainFrameOnly: true)
        )

        let config = WKWebViewConfiguration()
        config.userContentController = controller

        // C1: prove the native Keychain Ed25519 signer interoperates with the Rust verifier
        // (CryptoKit ↔ ed25519-dalek). Logged once at launch for sim/device verification.
        print("nimmesh wallet self-test: address=\(Wallet.address() ?? "?") signedOk=\(Wallet.selfTest())")
        // C1c: prove live testnet RPC connectivity (the send path's head anchor) at launch.
        // NSLog (not print) so the async result lands in the unified log we can query post-launch.
        Task {
            let head = (try? await NimiqRpc.headHeight()).map(String.init) ?? "unreachable"
            NSLog("nimmesh testnet head=%@", head)
        }

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
    static let channel = "nimmesh"
    weak var webView: WKWebView?

    // G5: the offline BLE mesh — a CoreBluetooth radio driving a Rust `MeshNode`. Built once
    // (lazily, on the first meshStatus probe at launch); the node holds the radio strongly, the
    // radio holds the node weakly. On the simulator BLE is unsupported (0 peers, no crash); on a
    // real device it advertises + scans for real (the 2-phone interop test).
    private let bleRadio = BleMeshRadio()
    private lazy var node: MeshNode = {
        var sid = Data(count: 8)
        _ = sid.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 8, $0.baseAddress!) }
        let n = MeshNode(senderId: sid, radio: bleRadio)
        bleRadio.node = n
        return n
    }()

    /// Injected at document start. Exposes a tiny promise-based RPC the web UI calls:
    /// `await window.nimmesh.version()` etc. If the handler is ever absent (e.g. the
    /// same page opened in a desktop browser for screenshot verification), `window.nimmesh`
    /// is simply undefined and the UI degrades gracefully — it never throws on load.
    static let jsShim = """
    (function () {
      if (!(window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.nimmesh)) return;
      var pending = {}, seq = 0;
      function call(method, args) {
        return new Promise(function (resolve, reject) {
          var id = ++seq;
          pending[id] = { resolve: resolve, reject: reject };
          try { window.webkit.messageHandlers.nimmesh.postMessage({ id: id, method: method, args: args || null }); }
          catch (e) { delete pending[id]; reject(e); }
        });
      }
      window.__nimmeshResolve = function (id, ok, payload) {
        var p = pending[id]; if (!p) return; delete pending[id];
        if (ok) p.resolve(payload); else p.reject(new Error(String(payload)));
      };
      window.nimmesh = {
        call: call,
        version: function () { return call('version'); },
        network: function () { return call('network'); },
        meshStatus: function () { return call('meshStatus'); },
        // G16: the honest "will it send?" reach (online|meshed|offline). Read-only.
        reachability: function () { return call('reachability'); },
        // G19: how hard to nudge a backup, from the account's public state. Read-only —
        // no key/seed crosses; the Rust policy decides (none|gentle|important|critical).
        backupUrgency: function (s) { return call('backupUrgency', s || {}); },
        // C1: this device's wallet address (derived from the Keychain key — no seed crosses).
        walletAddress: function () { return call('walletAddress'); },
        // C1e: recovery-phrase wallet lifecycle (create / import / back up). The 24-word phrase
        // is shown only inside the app for backup; it never crosses to Rust, the mesh, or a log.
        walletExists: function () { return call('walletExists'); },
        createWallet: function () { return call('createWallet'); },
        importWallet: function (m) { return call('importWallet', { mnemonic: m }); },
        recoveryPhrase: function () { return call('recoveryPhrase'); },
        // Mainnet toggle (gated; default testnet). The app never auto-sends real funds.
        currentNetwork: function () { return call('currentNetwork'); },
        setNetwork: function (m) { return call('setNetwork', { mainnet: !!m }); },
        // C1c: live testnet — head height, balance, faucet, and the real send (sign+broadcast).
        headHeight: function () { return call('headHeight'); },
        walletBalance: function () { return call('walletBalance'); },
        // C1d: this wallet's real on-chain transaction history (read-only public data).
        walletHistory: function () { return call('walletHistory'); },
        fundFromFaucet: function () { return call('fundFromFaucet'); },
        sendTransaction: function (a) { return call('sendTransaction', a || {}); }
      };
    })();
    """

    func userContentController(_ ucc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? [String: Any],
              let id = body["id"] as? Int,
              let method = body["method"] as? String else { return }
        let args = body["args"]
        // C1c: the live-chain methods do network IO → run them off the main actor and resolve
        // when they complete. Everything else is synchronous (pure-core reads).
        switch method {
        case "headHeight", "walletBalance", "walletHistory", "fundFromFaucet", "sendTransaction":
            Task { let (ok, payload) = await self.handleAsync(method: method, args: args)
                self.resolve(id: id, ok: ok, payload: payload) }
        default:
            let (ok, payload) = handle(method: method, args: args)
            resolve(id: id, ok: ok, payload: payload)
        }
    }

    /// C1c: the testnet network methods — fetch head, balance, faucet, and the real send
    /// (sign with the Keychain key → broadcast). Testnet only; the seed never crosses (only the
    /// signed blob is sent). The recipient/amount are public.
    private func handleAsync(method: String, args: Any?) async -> (Bool, Any) {
        switch method {
        case "headHeight":
            guard let h = try? await NimiqRpc.headHeight() else { return (false, "head fetch failed") }
            return (true, ["height": Int(h)])
        case "walletBalance":
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            return (true, ["luna": Int(await NimiqRpc.balance(addr))])
        case "walletHistory":
            // Real on-chain history for this wallet, normalised for the UI: direction +
            // counterparty + confirmed are decided here (the seed never crosses; this is all
            // public chain data). Newest first, capped.
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            let selfCompact = addr.replacingOccurrences(of: " ", with: "").uppercased()
            let txs: [[String: Any]] = (await NimiqRpc.transactions(addr, max: 20)).map { t in
                let to = (t["to"] as? String ?? "").replacingOccurrences(of: " ", with: "").uppercased()
                let incoming = (to == selfCompact)
                return [
                    "hash": t["hash"] as? String ?? "",
                    "counterparty": incoming ? (t["from"] as? String ?? "") : (t["to"] as? String ?? ""),
                    "valueLuna": (t["value"] as? NSNumber)?.intValue ?? 0,
                    "timestamp": (t["timestamp"] as? NSNumber)?.doubleValue ?? 0,
                    "incoming": incoming,
                    "confirmed": ((t["blockNumber"] as? NSNumber)?.intValue ?? 0) > 0,
                ]
            }
            return (true, ["txs": txs])
        case "fundFromFaucet":
            guard !NimiqRpc.isMainnet else { return (false, "faucet is testnet only") }
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            await NimiqRpc.tapFaucet(addr)
            return (true, ["funded": true])
        case "sendTransaction":
            let a = args as? [String: Any] ?? [:]
            guard let recipient = a["recipient"] as? String, !recipient.isEmpty else {
                return (false, "missing recipient")
            }
            let amount = (a["amountLuna"] as? NSNumber)?.uint64Value ?? 0
            guard let signer = Wallet.signer else { return (false, "no wallet — create or import one first") }
            do {
                let head = try await NimiqRpc.headHeight()
                let intent = TransferIntent(
                    recipient: recipient, value: amount, validityStartHeight: head, network: NimiqRpc.network
                )
                let signed = try signer.signTransfer(intent: intent) // Keychain-derived key signs
                let hash = try await NimiqRpc.sendRawTransaction(signed.rawHex)
                return (true, ["txHash": hash])
            } catch {
                return (false, "\(error)")
            }
        default:
            return (false, "unknown async method: \(method)")
        }
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
            // G5: the live mesh reading from the BLE-backed node (constructing it here also
            // brings the radio up). Simulator: 0 peers (BLE unsupported); device: real peers.
            let peers = Int(node.peerCount())
            return (true, ["state": peers > 0 ? "meshed" : "offline", "peers": peers])
        case "reachability":
            // G16/G5: the live "will it send?" reach from the BLE-backed node (peers + a heard
            // gateway beacon). Simulator: offline (no BLE); device: meshed/online with peers.
            let r: String
            switch node.reachability() {
            case .online: r = "online"
            case .meshed: r = "meshed"
            case .offline: r = "offline"
            }
            return (true, ["reachability": r])
        case "walletAddress":
            // C1: the wallet's NQ address, derived from the Keychain Ed25519 public key. The
            // seed stays in the Keychain — only the public address leaves.
            return (true, ["address": Wallet.address() ?? ""])
        case "walletExists":
            // C1e: whether onboarding is done (a recovery-phrase wallet is stored).
            return (true, ["exists": Wallet.hasWallet()])
        case "createWallet":
            // C1e: generate a new 24-word wallet; return the phrase so the UI can show it for
            // backup (phrase stays in-app; never to Rust/mesh/log) + the derived address.
            guard let phrase = Wallet.createNew() else { return (false, "could not create wallet") }
            return (true, ["mnemonic": phrase, "address": Wallet.address() ?? ""])
        case "importWallet":
            // C1e: import an existing wallet from its recovery phrase. The derived address is
            // returned so the user can confirm it matches their real wallet before funding.
            let a = args as? [String: Any] ?? [:]
            guard Wallet.importMnemonic((a["mnemonic"] as? String) ?? "") else {
                return (false, "invalid recovery phrase")
            }
            return (true, ["address": Wallet.address() ?? ""])
        case "recoveryPhrase":
            // C1e: the stored phrase, for the in-app backup screen.
            guard let phrase = Wallet.recoveryPhrase() else { return (false, "no wallet") }
            return (true, ["mnemonic": phrase])
        case "currentNetwork":
            // The selected network (default testnet). Mainnet is the gated real-funds toggle.
            return (true, ["mainnet": NimiqRpc.isMainnet, "name": NimiqRpc.isMainnet ? "mainnet" : "testnet"])
        case "setNetwork":
            // Deliberate, persisted network switch (default testnet). The app never auto-sends;
            // a mainnet send is always a user action (docs/MAINNET-GATING.md).
            let a = args as? [String: Any] ?? [:]
            NimiqRpc.isMainnet = (a["mainnet"] as? Bool) ?? false
            return (true, ["mainnet": NimiqRpc.isMainnet])
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
        let js = "window.__nimmeshResolve(\(id), \(ok ? "true" : "false"), \(json));"
        DispatchQueue.main.async { webView.evaluateJavaScript(js) }
    }
}
