import LocalAuthentication
import Security
import SwiftUI
import UIKit
import WebKit
import NimmeshCore

/// Hosts the real `nimiq-ui` web wallet (`webui/index.html`, bundled as a folder
/// reference) in a `WKWebView` and bridges it to the Rust core (A1).
///
/// The bridge is **read-only**: `version`, `meshStatus`, `reachability` (G16),
/// `backupUrgency` (G19).
/// It signs nothing, broadcasts nothing, and never sees key/seed material — so it stays firmly
/// non-money-path. The send path (sign with the Keychain key → broadcast, MAINNET) is the C1c
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
        // C1c: prove live RPC connectivity (the send path's head anchor) at launch.
        // NSLog (not print) so the async result lands in the unified log we can query post-launch.
        Task {
            let head = (try? await NimiqRpc.headHeight()).map(String.init) ?? "unreachable"
            NSLog("nimmesh head=%@", head)
        }

        let webView = WKWebView(frame: .zero, configuration: config)
        // JS dialogs (alert/confirm) DO NOT EXIST in a WKWebView unless the app implements
        // WKUIDelegate — without it `confirm()` silently returns false, which made every
        // confirm-gated action (delete wallet, log out, mainnet switch) a no-op on device.
        webView.uiDelegate = context.coordinator
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

    /// The iOS Keychain survives an app uninstall; UserDefaults does not. So a wallet that is
    /// already present on the very FIRST launch of this install belongs to a previous install —
    /// flag it so the UI asks "keep it or start fresh" instead of silently adopting it.
    private static let hasLaunchedKey = "nimmesh.hasLaunched"
    private static let recoveredWalletKey = "nimmesh.recoveredWallet"
    private static let langKey = "nimmesh.lang"
    private static let backedUpKey = "nimmesh.backedUp"

    override init() {
        super.init()
        let defaults = UserDefaults.standard
        if !defaults.bool(forKey: Bridge.hasLaunchedKey) {
            defaults.set(true, forKey: Bridge.hasLaunchedKey)
            if Wallet.hasWallet() { defaults.set(true, forKey: Bridge.recoveredWalletKey) }
        }
    }

    // G5: the offline BLE mesh — a CoreBluetooth radio driving a Rust `MeshNode`. Built once
    // (lazily, on the first meshStatus probe at launch); the node holds the radio strongly, the
    // radio holds the node weakly. On the simulator BLE is unsupported (0 peers, no crash); on a
    // real device it advertises + scans for real (the 2-phone interop test).
    // The phone is a GATEWAY node whenever the framework carries the HTTP client:
    // when this phone has internet it broadcasts other people's mesh txs, answers
    // balance/history queries, and beacons the head — any online phone becomes an
    // exit for everyone around it. Offline it self-gates: every RPC call fails, so
    // it answers nothing and emits no receipt (another gateway can still carry the
    // tx) and behaves exactly like the plain relay it falls back to. It holds no
    // keys for others and signs nothing — broadcast-only, same as the Mac.
    // (Construction lives in `makeNormalNode` — SwapMesh.swift — so the swap demo can
    // temporarily replace the node with a TESTNET participant and restore it after.)
    let bleRadio = BleMeshRadio()
    lazy var node: MeshNode = makeNormalNode()
    /// Whether the over-the-mesh swap demo owns the node right now (TESTNET participant).
    var swapDemoOn = false
    /// G10b: whether the CURRENT swap participant is the LIVE one (real testnet/Amoy coins
    /// moving) rather than the Act-1 sim. Drives the honest labels + the lock reporting.
    var swapLiveOn = false
    /// G10b: the caller-held book of REAL NIM HTLC locks the live initiator funded — kept
    /// for the never-strand refund path (mirrored into UserDefaults on every status read).
    var liveLockBook: LiveLockBook?
    /// G10b: the derived Amoy gas account (0x…) shown so it can be topped up with POL.
    var liveGasAddress: String?
    /// Whether the node is currently a LIVE swap RESPONDER (gives USDC, receives NIM) rather
    /// than an initiator/proposer — drives the responder panel + honest labels.
    var swapRespondOn = false
    /// §8.3: whether the CURRENT live swap is on **mainnet** (real funds) rather than
    /// testnet/Amoy — set only when `mainnetSwapArmed()` was true at start. Drives the loud
    /// "REAL MAINNET FUNDS" labels + the mainnet endpoints/ctors. `false` on any shipped build.
    var swapMainnetOn = false
    /// The derived Amoy account (0x…) the responder escrows USDC from + pays gas — shown so
    /// the owner can fund it with test USDC + POL before it can answer a swap.
    var liveFundAddress: String?
    /// The ~3 s in-flight settlement heartbeat (`pollSwapFast`) — runs only while the swap
    /// demo owns the node; started/stopped in SwapMesh.swift. Idle-free + rate-limited core-side.
    var swapFastTimer: DispatchSourceTimer?

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
        // Reinstall-aware status: the Keychain outlives an uninstall, so a wallet can already
        // exist on a first launch — `recovered` marks it as a previous install's wallet and the
        // UI asks "keep it or start fresh" instead of silently adopting it.
        walletStatus: function () { return call('walletStatus'); },
        resolveRecovered: function (keep) { return call('resolveRecovered', { keep: !!keep }); },
        // Account menu: remove the wallet from this device (the UI confirms + reminds that
        // without the 24 words it cannot be recovered).
        deleteWallet: function () { return call('deleteWallet'); },
        // UI language preference, persisted in UserDefaults so it survives relaunches.
        getLang: function () { return call('getLang'); },
        setLang: function (l) { return call('setLang', { lang: l }); },
        // Native camera QR scanner (Send bar). Resolves { text } or rejects on cancel.
        scanQr: function () { return call('scanQr'); },
        // Share/invite: open the native share sheet so a user can pull friends into the mesh
        // (a mesh is only as useful as it is populated). Resolves { shared } / { shared:false }.
        share: function (text, url) { return call('share', { text: text, url: url }); },
        // Diagnostics: the BLE radio's live role/counter state (for the on-device mesh test).
        meshDebug: function () { return call('meshDebug'); },
        // Keepalive: emit a head beacon to peers so the BLE link doesn't idle-timeout (~every
        // 15s from the UI). Real mesh traffic (G9 beacon), not filler.
        keepalive: function () { return call('keepalive'); },
        // "Unlock your Backup": Face ID / device passcode. Resolves { ok }.
        authenticate: function () { return call('authenticate'); },
        // Backup: the wallet's two-code XOR backup + whether ANY backup was completed
        // (drives the G19 nudge off). Codes are derived natively; the phrase never crosses.
        backupCodes: function () { return call('backupCodes'); },
        importBackupCodes: function (a, b) { return call('importBackupCodes', { code1: a, code2: b }); },
        getBackedUp: function () { return call('getBackedUp'); },
        setBackedUp: function (v) { return call('setBackedUp', { backedUp: !!v }); },
        // Live chain (MAINNET-only) — head height, balance, history, and the real send
        // (sign with the Keychain key + broadcast). The app never auto-sends.
        headHeight: function () { return call('headHeight'); },
        walletBalance: function () { return call('walletBalance'); },
        walletHistory: function () { return call('walletHistory'); },
        sendTransaction: function (a) { return call('sendTransaction', a || {}); },
        // Offline mesh send (TESTNET proof): sign anchored to the mesh-heard gateway head
        // beacon, hand the signed tx to the BLE mesh, and poll the receipt that a gateway
        // floods back once it broadcasts. No RPC anywhere on this path.
        meshSendInfo: function () { return call('meshSendInfo'); },
        meshSendTransaction: function (a) { return call('meshSendTransaction', a || {}); },
        meshPaymentStatus: function (t) { return call('meshPaymentStatus', { meshTxId: t }); },
        // Fiat price data proxied through native URLSession: the page runs on a file://
        // origin and WKWebView blocks its fetch() to the network, so CoinGecko is fetched
        // natively. Whitelisted coins/currencies only - no arbitrary-URL surface.
        prices: function (c) { return call('prices', { currency: c }); },
        market: function (coin, c) { return call('market', { coin: coin, currency: c }); },
        // G15 balance-over-mesh: ask the mesh for this wallet's on-chain balance (any
        // internet-bearing gateway answers over BLE), then read the last-heard answer.
        // Public address + public balance only - no keys, no history.
        meshQueryBalance: function () { return call('meshQueryBalance'); },
        meshCachedBalance: function () { return call('meshCachedBalance'); },
        // Transactions over the mesh: ask a gateway for this wallet's recent history rows
        // (the answer rides the fragmenter over BLE), then read the last-heard answer.
        meshQueryHistory: function () { return call('meshQueryHistory'); },
        meshCachedHistory: function () { return call('meshCachedHistory'); },
        // Over-the-mesh swap: the real swap protocol over real Bluetooth. Default = the
        // Act-1 DEMO (TESTNET, SIM tx bytes — no funds move). Pass { real: true } for the
        // G10 LIVE initiator path (gives NIM, receives USDC), or { respond: true } for the
        // LIVE RESPONDER path (gives USDC, receives NIM). Either LIVE path moves real TEST
        // coins (NIM on Albatross testnet ⇄ USDC on Amoy), honestly labeled; mainnet is never
        // touched. swapMeshRefund sweeps any expired real NIM lock back to the wallet.
        swapMeshStart: function (a) { return call('swapMeshStart', a || {}); },
        swapMeshStatus: function () { return call('swapMeshStatus'); },
        swapMeshStop: function () { return call('swapMeshStop'); },
        swapMeshRefund: function () { return call('swapMeshRefund'); },
        // §8.3 probe: is the mainnet swap path ARMED (flag + recorded HTLC)? Resolves
        // { armed, reason }. `false` on any shipped build → the UI keeps its testnet path +
        // labels; when Andjroo's arming release flips it, the swap sheet shows the loud
        // "REAL MAINNET FUNDS" labels and drives the mainnet ctors.
        mainnetSwapArmed: function () { return call('mainnetSwapArmed'); },
        // The wallet-derived EVM swap accounts (gas/claim/fund addresses) — fundable up front.
        swapEvmAddresses: function () { return call('swapEvmAddresses'); },
        // Read-only Polygon (mainnet) reads for the USDC card/drill-in: live balances (USDC per
        // account + POL for the gas account) and USDC Transfer history. Native URLSession —
        // file:// blocks fetch(). No keys, no broadcast (see PolygonReads.swift).
        usdcBalances: function (a) { return call('usdcBalances', a || {}); },
        usdcHistory: function (a) { return call('usdcHistory', a || {}); },
        // The standalone USDC send (OWNER-GATED, real mainnet funds — see PolygonSend.swift).
        sendUsdc: function (a) { return call('sendUsdc', a || {}); },
        // Public mesh chat (0x50): broadcast text over BLE; the rolling heard/sent log.
        sendChat: function (nick, text) { return call('sendChat', { nickname: nick, text: text }); },
        chatMessages: function () { return call('chatMessages'); },
        bitchatStatus: function () { return call('bitchatStatus'); },
        bitchatSetEnabled: function (a) { return call('bitchatSetEnabled', a || {}); },
        // Cashlinks: NIM as a URL (official hub format — any browser claims). The wallet
        // funds a fresh single-use key; the link is cash — whoever holds it can claim.
        cashlinkCreate: function (a) { return call('cashlinkCreate', a || {}); },
        cashlinkList: function () { return call('cashlinkList'); },
        cashlinkStatus: function (addr) { return call('cashlinkStatus', { address: addr }); },
        cashlinkPeek: function (a) { return call('cashlinkPeek', a || {}); },
        cashlinkClaim: function (a) { return call('cashlinkClaim', a || {}); }
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
        case "headHeight", "walletBalance", "walletHistory", "sendTransaction", "meshSendTransaction",
             "prices", "market", "swapMeshStart", "swapMeshRefund", "cashlinkCreate", "cashlinkStatus",
             "usdcBalances", "usdcHistory", "sendUsdc", "cashlinkPeek", "cashlinkClaim":
            Task { let (ok, payload) = await self.handleAsync(method: method, args: args)
                self.resolve(id: id, ok: ok, payload: payload) }
        case "authenticate":
            // The Keyguard's "Unlock your Backup" equivalent: Face ID / device passcode via
            // LocalAuthentication. Devices with no passcode set have nothing to unlock WITH,
            // so they pass through (the device itself is unprotected either way).
            let ctx = LAContext()
            var err: NSError?
            guard ctx.canEvaluatePolicy(.deviceOwnerAuthentication, error: &err) else {
                resolve(id: id, ok: true, payload: ["ok": true, "method": "none"])
                return
            }
            ctx.evaluatePolicy(
                .deviceOwnerAuthentication,
                localizedReason: "Unlock your backup"
            ) { success, _ in
                self.resolve(id: id, ok: true, payload: ["ok": success])
            }
        case "scanQr":
            // The native camera scanner (Send bar's scan button). Resolves with the decoded
            // string, or rejects on cancel / denial — the page treats that as a quiet no-op.
            DispatchQueue.main.async {
                guard let top = Bridge.topmostViewController() else {
                    self.resolve(id: id, ok: false, payload: "cancelled"); return
                }
                QrScannerViewController.scan(from: top) { text in
                    if let text = text, !text.isEmpty {
                        self.resolve(id: id, ok: true, payload: ["text": text])
                    } else {
                        self.resolve(id: id, ok: false, payload: "cancelled")
                    }
                }
            }
        case "share":
            // The native share sheet (UIActivityViewController) — the mesh growth loop: invite
            // friends to install and join, so more phones = a stronger, denser mesh.
            let a = args as? [String: Any] ?? [:]
            let text = (a["text"] as? String) ?? ""
            let url = URL(string: (a["url"] as? String) ?? "")
            DispatchQueue.main.async {
                guard let top = Bridge.topmostViewController() else {
                    self.resolve(id: id, ok: true, payload: ["shared": false]); return
                }
                var items: [Any] = [text]
                if let url = url { items.append(url) }
                let vc = UIActivityViewController(activityItems: items, applicationActivities: nil)
                // iPad: anchor the popover so it doesn't crash on a nil sourceView.
                if let pop = vc.popoverPresentationController {
                    pop.sourceView = top.view
                    pop.sourceRect = CGRect(x: top.view.bounds.midX, y: top.view.bounds.midY, width: 0, height: 0)
                    pop.permittedArrowDirections = []
                }
                vc.completionWithItemsHandler = { _, completed, _, _ in
                    self.resolve(id: id, ok: true, payload: ["shared": completed])
                }
                top.present(vc, animated: true)
            }
        default:
            let (ok, payload) = handle(method: method, args: args)
            resolve(id: id, ok: ok, payload: payload)
        }
    }

    /// The view controller to present native UI (dialogs, the scanner) from.
    static func topmostViewController() -> UIViewController? {
        let window = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap { $0.windows }
            .first { $0.isKeyWindow }
        guard var top = window?.rootViewController else { return nil }
        while let presented = top.presentedViewController { top = presented }
        return top
    }

    /// C1c: the live-chain network methods — fetch head, balance, history, and the real send
    /// (sign with the Keychain key → broadcast). Testnet only; the seed never crosses (only the
    /// signed blob is sent). The recipient/amount are public.
    private func handleAsync(method: String, args: Any?) async -> (Bool, Any) {
        switch method {
        case "headHeight":
            guard let h = try? await NimiqRpc.headHeight() else { return (false, "head fetch failed") }
            return (true, ["height": Int(h)])
        case "walletBalance":
            // Offline continuity lives NATIVELY: a successful read updates the UserDefaults
            // cache; a failed read (offline) answers with the last-known balance instead of
            // pretending the wallet is empty. NimiqRpc.balance now THROWS on failure — it
            // used to return 0, which made Bluetooth-only mode render "0 NIM" as if real.
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            let balKey = "nimmesh.cache.balance." + addr.replacingOccurrences(of: " ", with: "")
            do {
                let luna = try await NimiqRpc.balance(addr)
                UserDefaults.standard.set(Int(luna), forKey: balKey)
                return (true, ["luna": Int(luna)])
            } catch {
                if UserDefaults.standard.object(forKey: balKey) != nil {
                    return (true, ["luna": UserDefaults.standard.integer(forKey: balKey), "cached": true])
                }
                return (false, "\(error)")
            }
        case "walletHistory":
            // Real on-chain history for this wallet, normalised for the UI: direction +
            // counterparty + confirmed are decided here (the seed never crosses; this is all
            // public chain data). Newest first, capped. Same native offline continuity as
            // walletBalance: NimiqRpc.transactions now THROWS on failure (it used to return
            // [], which rendered — and cached — an empty history whenever the network died).
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            let selfCompact = addr.replacingOccurrences(of: " ", with: "").uppercased()
            let txsKey = "nimmesh.cache.txs." + addr.replacingOccurrences(of: " ", with: "")
            do {
                let txs: [[String: Any]] = try await NimiqRpc.transactions(addr, max: 20).map { t in
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
                if let d = try? JSONSerialization.data(withJSONObject: txs) {
                    UserDefaults.standard.set(d, forKey: txsKey)
                }
                return (true, ["txs": txs])
            } catch {
                if let d = UserDefaults.standard.data(forKey: txsKey),
                   let cached = (try? JSONSerialization.jsonObject(with: d)) as? [[String: Any]] {
                    return (true, ["txs": cached, "cached": true])
                }
                return (false, "\(error)")
            }
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
        case "meshSendTransaction":
            // The offline mesh send: NO RPC anywhere on this path. The intent is anchored to
            // the freshest gateway head beacon heard over BLE (G9 — never pre-date a tx),
            // signed with the same Keychain key, and flooded to the mesh as a real nimiqTx.
            // A gateway node (the Mac) broadcasts it and floods the receipt back (G8/G17);
            // `meshPaymentStatus` polls that settlement. The anchored intent carries the
            // NODE's network — mainnet on the real app (Andjroo-gated, authorized
            // 2026-07-06), so the USER is signing real funds exactly like the online Send;
            // the mesh only changes the delivery, never who signs.
            let a = args as? [String: Any] ?? [:]
            guard let recipient = a["recipient"] as? String, !recipient.isEmpty else {
                return (false, "missing recipient")
            }
            let amount = (a["amountLuna"] as? NSNumber)?.uint64Value ?? 0
            guard amount > 0 else { return (false, "missing amount") }
            guard let signer = Wallet.signer else { return (false, "no wallet yet") }
            guard let intent = node.anchoredIntent(recipient: recipient, value: amount) else {
                return (false, "no gateway head heard yet")
            }
            do {
                let signed = try signer.signTransfer(intent: intent)
                let meshTxId = node.submitSignedTransfer(signedTransfer: signed)
                guard !meshTxId.isEmpty else { return (false, "could not encode the signed tx") }
                return (true, [
                    "meshTxId": meshTxId.map { String(format: "%02x", $0) }.joined(),
                    "txHash": signed.txHash,
                    "network": intent.network == .mainnet ? "mainnet" : "testnet",
                ])
            } catch {
                return (false, "\(error)")
            }
        case "prices":
            // CoinGecko simple/price via native URLSession — the webui page runs on a
            // file:// origin and WKWebView blocks its fetch() to the network. Read-only
            // public data; the currency is whitelisted.
            let a = args as? [String: Any] ?? [:]
            let currency = (a["currency"] as? String ?? "usd").lowercased()
            guard ["usd", "mxn", "eur", "brl"].contains(currency) else { return (false, "bad currency") }
            guard let url = URL(string: "https://api.coingecko.com/api/v3/simple/price?ids=nimiq-2,bitcoin,usd-coin&vs_currencies=\(currency)") else {
                return (false, "bad url")
            }
            do {
                let (data, _) = try await URLSession.shared.data(from: url)
                guard let j = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
                    return (false, "bad response")
                }
                let price: (String) -> Any = { id in
                    ((j[id] as? [String: Any])?[currency] as? NSNumber)?.doubleValue ?? NSNull()
                }
                return (true, ["nim": price("nimiq-2"), "btc": price("bitcoin"), "usdc": price("usd-coin")])
            } catch {
                return (false, "\(error)")
            }
        case "market":
            // CoinGecko market_chart (the 24h sparkline series), same native proxy; coin +
            // currency whitelisted, and only the price series crosses the bridge.
            let a = args as? [String: Any] ?? [:]
            let coin = a["coin"] as? String ?? ""
            let currency = (a["currency"] as? String ?? "usd").lowercased()
            guard ["nimiq-2", "bitcoin"].contains(coin), ["usd", "mxn", "eur", "brl"].contains(currency) else {
                return (false, "bad coin or currency")
            }
            guard let url = URL(string: "https://api.coingecko.com/api/v3/coins/\(coin)/market_chart?vs_currency=\(currency)&days=1") else {
                return (false, "bad url")
            }
            do {
                let (data, _) = try await URLSession.shared.data(from: url)
                guard let j = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let raw = j["prices"] as? [[Any]] else { return (false, "bad response") }
                let series = raw.compactMap { row in (row.count > 1 ? row[1] : nil) as? NSNumber }
                    .map { $0.doubleValue }
                return (true, ["prices": series])
            } catch {
                return (false, "\(error)")
            }
        case "swapMeshStart":
            // The over-the-mesh swap: swaps the node onto TESTNET as a participant (sim by
            // default; { real: true } = the G10 live testnet⇄Amoy money path).
            return await swapMeshStart(args: args)
        case "swapMeshRefund":
            // Never-strand: refund expired REAL NIM locks back to the wallet (chain IO).
            return await swapMeshRefund()
        case "cashlinkCreate", "cashlinkStatus", "cashlinkPeek", "cashlinkClaim":
            return await cashlinkHandle(method: method, args: args)
        case "usdcBalances", "usdcHistory":
            // Read-only Polygon (mainnet, 137) reads for the wallet-derived USDC swap accounts.
            return await PolygonReads.handle(method: method, args: args)
        case "sendUsdc":
            return await PolygonSend.handle(method: method, args: args)
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
        case "meshStatus":
            // G5: the live mesh reading from the BLE-backed node (constructing it here also
            // brings the radio up). Simulator: 0 peers (BLE unsupported); device: real peers.
            let peers = Int(node.peerCount())
            return (true, ["state": peers > 0 ? "meshed" : "offline", "peers": peers])
        case "meshDebug":
            // The BLE radio's live role/counter state — makes the phone's Bluetooth visible on
            // the Network screen during the 2-node test (constructing node ensures the radio is up).
            return (true, ["debug": bleRadio.debugSummary() + " node-peers:\(node.peerCount())"])
        case "keepalive":
            // Emit a head beacon to connected peers — periodic BLE traffic that keeps iOS from
            // idle-dropping the mesh link (the ~50s flap). No-op if there are no peers.
            node.pollBeacon()
            return (true, ["ok": true])
        case "swapMeshStatus":
            return swapMeshStatus()
        case "swapMeshStop":
            return swapMeshStop()
        case "mainnetSwapArmed":
            // §8.3 probe: is the mainnet swap path armed (flag + recorded HTLC)? `false` on any
            // shipped build → the UI keeps its testnet path/labels. `reason` labels the state.
            return (true, ["armed": NimmeshCore.mainnetSwapArmed(), "reason": NimmeshCore.mainnetSwapReason()])
        case "swapEvmAddresses":
            // The wallet-DERIVED EVM accounts for BOTH swap roles, readable BEFORE any swap
            // starts, so they can be funded up front (the initiator's GAS account pays its
            // Polygon `withdraw(S)` claim — an empty one stalls the swap at reveal and only
            // the timelock refund recovers it). Public addresses only; no secret crosses the
            // bridge, nothing touches the node.
            guard let evm = Wallet.swapEvmSecrets(), let rs = Wallet.swapResponderSecrets()
            else { return (false, "no wallet") }
            let gas = (try? NimmeshCore.evmAddressForSecret(secret: evm.gas)) ?? ""
            let claim = (try? NimmeshCore.evmAddressForSecret(secret: evm.claim)) ?? ""
            let fund = (try? NimmeshCore.evmAddressForSecret(secret: rs.fund)) ?? ""
            return (true, ["gas": gas, "claim": claim, "fund": fund])
        case "cashlinkList":
            return cashlinkList()
        case "sendChat", "chatMessages", "bitchatStatus", "bitchatSetEnabled":
            // Mesh chat + the Bitchat interop toggle — one family seam (BitchatChat.swift).
            return chatHandle(method: method, args: args)
        case "meshQueryHistory":
            // Transactions over the mesh: flood a nimiqTxHistoryQuery — the Mac gateway
            // answers up to 10 compact rows through the fragmenter. Fire-and-forget.
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            node.queryTxHistory(address: addr)
            return (true, ["ok": true])
        case "meshCachedHistory":
            // The freshest history heard over the mesh (unverified/last-known, same trust
            // model as the mesh balance), shaped exactly like walletHistory's rows.
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            let rows = node.cachedTxHistory(address: addr)
            let txs: [[String: Any]] = rows.map { r in
                [
                    "hash": r.hash,
                    "counterparty": r.counterparty,
                    "valueLuna": Int(r.valueLuna),
                    "timestamp": Double(r.timestampMs),
                    "incoming": r.incoming,
                    "confirmed": r.confirmed,
                ]
            }
            return (true, ["txs": txs, "headHeight": Int(rows.first?.headHeight ?? 0)])
        case "meshQueryBalance":
            // G15: flood a nimiqBalanceQuery for this wallet — the Mac gateway answers a
            // nimiqBalanceResponse over BLE. Fire-and-forget (non-blocking enqueue); the
            // answer lands in the node's cache, read via meshCachedBalance. No keys.
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            node.queryBalance(address: addr)
            return (true, ["ok": true])
        case "meshCachedBalance":
            // The last balance heard over the mesh for this wallet (unverified/last-known,
            // per the core's G15 contract), or has:false until a gateway has answered.
            guard let addr = Wallet.address() else { return (false, "no wallet") }
            if let c = node.cachedBalance(address: addr) {
                return (true, ["has": true, "luna": Int(c.balance), "headHeight": Int(c.headHeight)])
            }
            return (true, ["has": false])
        case "meshSendInfo":
            // Whether an offline mesh send is possible RIGHT NOW: a gateway head beacon has
            // been heard (the anchor for validityStartHeight) and at least one live peer is
            // connected to hand the signed tx to. Read-only, no keys. `network` labels the
            // send row honestly (MAINNET on the real app; the node only caches beacons
            // matching its own network, so a heard head IS a head on that network).
            let head = node.cachedHeadHeight()
            return (true, [
                "headHeard": head != nil,
                "head": Int(head ?? 0),
                "peers": Int(node.peerCount()),
                "network": NimiqRpc.network == .mainnet ? "mainnet" : "testnet",
            ])
        case "meshPaymentStatus":
            // Poll a mesh-submitted payment: pending (still relaying) → settled (a gateway
            // broadcast it and the receipt came back over BLE) or failed (gateway rejected).
            let a = args as? [String: Any] ?? [:]
            let hex = a["meshTxId"] as? String ?? ""
            var txId = Data(); txId.reserveCapacity(hex.count / 2)
            var i = hex.startIndex
            while i < hex.endIndex, let nx = hex.index(i, offsetBy: 2, limitedBy: hex.endIndex) {
                guard let b = UInt8(hex[i..<nx], radix: 16) else { break }
                txId.append(b); i = nx
            }
            guard txId.count == 32 else { return (false, "bad meshTxId") }
            let s: String
            switch node.paymentStatus(txId: txId) {
            case .pending: s = "pending"
            case .settled: s = "settled"
            case .failed: s = "failed"
            }
            return (true, ["status": s])
        case "reachability":
            // Honest reach now that the phone node is itself a gateway (a self-gateway
            // node's reachability() is always Online): online = a real RPC round-trip
            // succeeded in the last 30s; meshed = live BLE peers; else offline.
            let rpcLive = NimiqRpc.lastSuccessAt.map { Date().timeIntervalSince($0) < 30 } ?? false
            let r = rpcLive ? "online" : (node.peerCount() > 0 ? "meshed" : "offline")
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
        case "walletStatus":
            // Reinstall-aware existence: `recovered` = this wallet predates the current install
            // (Keychain survived an uninstall) and the user hasn't chosen keep/start-fresh yet.
            return (true, [
                "exists": Wallet.hasWallet(),
                "recovered": UserDefaults.standard.bool(forKey: Bridge.recoveredWalletKey),
            ])
        case "resolveRecovered":
            // The user's keep/start-fresh choice for a previous install's wallet. "Start fresh"
            // deletes it (the UI confirmed + reminded about the 24 words first).
            let a = args as? [String: Any] ?? [:]
            if !(a["keep"] as? Bool ?? true) { Wallet.delete() }
            UserDefaults.standard.set(false, forKey: Bridge.recoveredWalletKey)
            return (true, ["exists": Wallet.hasWallet()])
        case "deleteWallet":
            // Account-menu log-out. The UI confirms; without the words the wallet is gone.
            Wallet.delete()
            UserDefaults.standard.set(false, forKey: Bridge.recoveredWalletKey)
            UserDefaults.standard.set(false, forKey: Bridge.backedUpKey)
            // The cached last-known balance/history belong to the removed wallet.
            for key in UserDefaults.standard.dictionaryRepresentation().keys
            where key.hasPrefix("nimmesh.cache.") {
                UserDefaults.standard.removeObject(forKey: key)
            }
            return (true, ["deleted": true])
        case "backupCodes":
            // The two XOR backup codes (either alone is useless; both recover the wallet).
            guard let codes = Wallet.backupCodes() else { return (false, "no wallet") }
            return (true, ["code1": codes.code1, "code2": codes.code2])
        case "importBackupCodes":
            let a = args as? [String: Any] ?? [:]
            guard Wallet.importBackupCodes((a["code1"] as? String) ?? "", (a["code2"] as? String) ?? "")
            else { return (false, "invalid backup codes") }
            return (true, ["address": Wallet.address() ?? ""])
        case "getBackedUp":
            return (true, ["backedUp": UserDefaults.standard.bool(forKey: Bridge.backedUpKey)])
        case "setBackedUp":
            let a = args as? [String: Any] ?? [:]
            UserDefaults.standard.set(a["backedUp"] as? Bool ?? false, forKey: Bridge.backedUpKey)
            return (true, ["ok": true])
        case "getLang":
            return (true, ["lang": UserDefaults.standard.string(forKey: Bridge.langKey) ?? ""])
        case "setLang":
            let a = args as? [String: Any] ?? [:]
            UserDefaults.standard.set((a["lang"] as? String) ?? "", forKey: Bridge.langKey)
            return (true, ["ok": true])
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

/// Native JS dialogs. A WKWebView has NO built-in `alert()`/`confirm()` — without these the
/// page's confirm-gated actions (delete wallet, log out, the mainnet switch) silently no-op.
/// Each completion handler MUST be called exactly once, including when no presenter exists.
extension Bridge: WKUIDelegate {
    private func present(_ alert: UIAlertController) -> Bool {
        guard let top = Bridge.topmostViewController() else { return false }
        top.present(alert, animated: true)
        return true
    }

    func webView(
        _ webView: WKWebView,
        runJavaScriptAlertPanelWithMessage message: String,
        initiatedByFrame frame: WKFrameInfo,
        completionHandler: @escaping () -> Void
    ) {
        let alert = UIAlertController(title: nil, message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in completionHandler() })
        if !present(alert) { completionHandler() }
    }

    func webView(
        _ webView: WKWebView,
        runJavaScriptConfirmPanelWithMessage message: String,
        initiatedByFrame frame: WKFrameInfo,
        completionHandler: @escaping (Bool) -> Void
    ) {
        let alert = UIAlertController(title: nil, message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel) { _ in completionHandler(false) })
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in completionHandler(true) })
        if !present(alert) { completionHandler(false) }
    }

    func webView(
        _ webView: WKWebView,
        runJavaScriptTextInputPanelWithPrompt prompt: String,
        defaultText: String?,
        initiatedByFrame frame: WKFrameInfo,
        completionHandler: @escaping (String?) -> Void
    ) {
        let alert = UIAlertController(title: nil, message: prompt, preferredStyle: .alert)
        alert.addTextField { $0.text = defaultText }
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel) { _ in completionHandler(nil) })
        alert.addAction(UIAlertAction(title: "OK", style: .default) { [weak alert] _ in
            completionHandler(alert?.textFields?.first?.text ?? "")
        })
        if !present(alert) { completionHandler(nil) }
    }
}
