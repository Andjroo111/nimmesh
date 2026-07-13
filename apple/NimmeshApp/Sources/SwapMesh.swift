import Foundation
import NimmeshCore

// G10b: the LIVE testnet endpoints + the deployed `NimmeshHtlc` v2 (docs/swap/AMOY.md).
// TESTNET/Amoy only — the core's `guard_testnet`/`guard_amoy` refuse anything else at
// construction, and there is no mainnet parameter to pass.
private let liveNimRpcUrl = "https://rpc.testnet.nimiqwatch.com"
private let liveAmoyRpcUrl = "https://rpc-amoy.polygon.technology"
private let liveHtlcAddress = "0xb3B3703E07AC897B7E3e864C113a2Fa547D76736"
/// The Amoy testnet USDC token the responder's HTLC escrows (mac-node's default, docs/swap/AMOY.md).
/// TESTNET/Amoy only — mainnet is a separate, Andjroo-gated guard-lift, never a parameter here.
private let liveUsdcAddress = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582"

// §8.3 MAINNET endpoints (real funds) — used ONLY when `mainnetSwapArmed()` is true (off on any
// shipped build). The escrow HTLC + USDC token are pinned in the CORE (`MAINNET_HTLC_ADDRESS` +
// `NATIVE_USDC_POLYGON_MAINNET`), so no mainnet contract address is passed from here — the mainnet
// ctors ignore `config.htlcAddress`/`config.usdcAddress`. Only the RPC urls come from the app:
/// A Nimiq **mainnet** JSON-RPC (no "testnet" fragment → `HttpGatewayRpc::new_mainnet` admits it).
private let mainnetNimRpcUrl = "https://rpc.nimiqwatch.com"
/// An allow-listed Polygon **mainnet** RPC (`HttpPolygonRpc::new_mainnet` admits only the two
/// cross-read hosts). The mainnet EVM chain id (137) + native USDC are pinned in the core.
private let mainnetPolygonRpcUrl = "https://polygon.drpc.org"
/// UserDefaults key for the persisted NIM-lock book (never-strand across relaunches).
private let liveLocksKey = "nimmesh.swap.liveLocks"

/// Decode a (0x-)hex string to bytes; `nil` on any malformed input.
private func dataFromHex(_ hex: String) -> Data? {
    var s = hex.lowercased()
    if s.hasPrefix("0x") { s.removeFirst(2) }
    guard s.count % 2 == 0 else { return nil }
    var out = Data(capacity: s.count / 2)
    var idx = s.startIndex
    while idx < s.endIndex {
        let next = s.index(idx, offsetBy: 2)
        guard let b = UInt8(s[idx..<next], radix: 16) else { return nil }
        out.append(b)
        idx = next
    }
    return out
}

/// The over-the-mesh swap (two modes, honestly labeled):
/// - **Act 1 sim** (default): the REAL swap protocol — discovery, matching, the signed
///   Propose, funding proofs, preimage reveal, retransmit, refund safety — over the REAL
///   Bluetooth mesh, with SIM transaction bytes (the core's `MockSigner`): no funds can move.
/// - **G10 live** (`real: true`): the SAME protocol carrying the LIVE testnet money path —
///   the wallet's enclave key funds a real NIM HTLC on Albatross TESTNET and the claimed
///   USDC lands on a wallet-derived Amoy address. Real TEST coins move; mainnet never does
///   (the constructors are testnet-pinned in the core, C1-asserted).
/// Either way the app's node is swapped onto TESTNET for the sheet's lifetime (mainnet
/// payments pause, clearly labeled) and the normal mainnet node is restored on close.
extension Bridge {
    /// The normal shipping node: a mainnet gateway when the framework carries the HTTP
    /// client, a plain mainnet node otherwise. (Extracted from the lazy initializer so the
    /// swap demo can restore it after.)
    func makeNormalNode() -> MeshNode {
        var sid = Data(count: 8)
        _ = sid.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 8, $0.baseAddress!) }
        let n: MeshNode
        do {
            n = try MeshNode.newGatewayMainnet(
                senderId: sid, radio: bleRadio, rpcUrl: "https://rpc.nimiqwatch.com")
        } catch {
            // Framework built without gateway-rpc (or a refused URL): plain mainnet node.
            n = MeshNode.newOnNetwork(senderId: sid, radio: bleRadio, network: NimiqRpc.network)
        }
        bleRadio.node = n
        return n
    }

    /// Start the demo: replace the live node with a TESTNET swap participant advertising
    /// "gives `nimLuna` NIM, wants `counterSat` sats". The intent/Propose identity is an
    /// ephemeral key from fresh randomness (never the wallet key — it stays in the Keychain).
    func swapMeshStart(args: Any?) async -> (Bool, Any) {
        let a = args as? [String: Any] ?? [:]
        if (a["respond"] as? Bool) == true {
            return await swapRespondStart(a) // phone-as-responder: gives USDC, receives NIM
        }
        if (a["real"] as? Bool) == true {
            return await swapLiveStart(a) // G10b: the real-testnet path
        }
        let nimLuna = (a["nimLuna"] as? NSNumber)?.uint64Value ?? 0
        let counterSat = (a["counterSat"] as? NSNumber)?.uint64Value ?? 0
        guard nimLuna > 0, counterSat > 0 else { return (false, "amounts required") }
        // Anchor the ad's expiry to the TESTNET head (best-effort; offline keeps the huge
        // fallback and the core reads an unheard head as fresh either way).
        let head = await testnetHead() ?? 0

        var seed = Data(count: 32)
        _ = seed.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!) }
        var pk = Data([0x02])
        var rnd = Data(count: 32)
        _ = rnd.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!) }
        pk.append(rnd) // 33-byte sim placeholder claimant pubkey (no real BTC key in Act 1)

        let intent = FfiStandingIntent(
            gives: .nim, counterAsset: .btc,
            nimAmount: nimLuna, counterAmount: counterSat,
            expiryHeight: head > 0 ? head + 20_000 : UInt64.max / 2,
            minNim: 0, maxNim: UInt64.max)
        let cfg = FfiParticipantConfig(
            btcPubkey: pk, btcAddress: Data("tb1q-sim-demo-phone".utf8),
            maxConcurrentSwaps: 0, deltaSafeBlocks: 600, minClaimWindowBlocks: 600,
            standingIntent: intent, intentSeed: seed)

        node.shutdown()
        do {
            var sid = Data(count: 8)
            _ = sid.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 8, $0.baseAddress!) }
            let n = try MeshNode.newSwapParticipant(
                senderId: sid, radio: bleRadio, config: cfg, gatewayRpcUrl: nil)
            node = n
            bleRadio.node = n
            swapDemoOn = true
            return (true, ["ok": true, "head": Int(head)])
        } catch {
            let n = makeNormalNode() // never leave the wallet without a node
            node = n
            return (false, "\(error)")
        }
    }

    /// G10b: start the LIVE testnet⇄Amoy swap — the phone is the NIM-giver/initiator. The
    /// wallet's enclave key funds the real NIM HTLC (seed never crosses); the claimed USDC
    /// pays out to a wallet-derived Amoy receive address; a wallet-derived Amoy GAS account
    /// pays the `withdraw(S)` (it needs a little POL — its address is surfaced for topping
    /// up). Every real NIM lock is recorded for the refund path before anything can strand.
    private func swapLiveStart(_ a: [String: Any]) async -> (Bool, Any) {
        let nimLuna = (a["nimLuna"] as? NSNumber)?.uint64Value ?? 0
        let usdcMicro = (a["usdcMicro"] as? NSNumber)?.uint64Value ?? 0
        guard nimLuna > 0, usdcMicro > 0 else { return (false, "amounts required") }
        guard let nimKey = Wallet.enclaveKey, let evm = Wallet.swapEvmSecrets() else {
            return (false, "no wallet yet — create or import one first")
        }
        let claimAddr: String
        do {
            claimAddr = try evmAddressForSecret(secret: evm.claim)
        } catch {
            return (false, "this build lacks the live swap stack: \(error)")
        }
        guard let claimBytes = dataFromHex(claimAddr), claimBytes.count == 20 else {
            return (false, "claim address derivation failed")
        }
        let gasAddr = (try? evmAddressForSecret(secret: evm.gas)) ?? ""

        // §8.3: when the mainnet swap path is ARMED (off on any shipped build), real MAINNET funds
        // move — the mainnet ctor + endpoints are used and the escrow HTLC/USDC are pinned in the
        // core. Unarmed → today's TESTNET path, byte-identical.
        let mainnet = NimmeshCore.mainnetSwapArmed()
        let head = mainnet ? (await mainnetHead() ?? 0) : (await testnetHead() ?? 0)
        var seed = Data(count: 32)
        _ = seed.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!) }
        let cfg = FfiLiveInitiatorConfig(
            nimLuna: nimLuna, usdcMicro: usdcMicro,
            expiryHeight: head > 0 ? head + 20_000 : UInt64.max / 2,
            intentSeed: seed,
            evmClaimAddress: claimBytes,
            evmGasSecret: evm.gas,
            nimRpcUrl: mainnet ? mainnetNimRpcUrl : liveNimRpcUrl,
            amoyRpcUrl: mainnet ? mainnetPolygonRpcUrl : liveAmoyRpcUrl,
            htlcAddress: mainnet ? "" : liveHtlcAddress, // mainnet: the core pins MAINNET_HTLC_ADDRESS
            deltaSafeBlocks: 0, minClaimWindowBlocks: 0) // 0 = the core's safe defaults
        let book = LiveLockBook()

        node.shutdown()
        do {
            var sid = Data(count: 8)
            _ = sid.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 8, $0.baseAddress!) }
            let n: MeshNode
            if mainnet {
                n = try MeshNode.newLiveSwapInitiatorMainnet(
                    senderId: sid, radio: bleRadio, nimFundingKey: nimKey, lockBook: book,
                    config: cfg, gatewayRpcUrl: nil)
            } else {
                n = try MeshNode.newLiveSwapInitiator(
                    senderId: sid, radio: bleRadio, nimFundingKey: nimKey, lockBook: book,
                    config: cfg, gatewayRpcUrl: nil)
            }
            node = n
            bleRadio.node = n
            liveLockBook = book
            liveGasAddress = gasAddr
            swapDemoOn = true
            swapLiveOn = true
            swapMainnetOn = mainnet
            return (true, [
                "ok": true, "mode": mainnet ? "live-mainnet" : "live", "head": Int(head),
                "mainnet": mainnet, "claimAddress": claimAddr, "gasAddress": gasAddr,
            ])
        } catch {
            let n = makeNormalNode() // never leave the wallet without a node
            node = n
            return (false, "\(error)")
        }
    }

    /// Phone-as-RESPONDER (gives USDC, receives NIM): swap the live node onto
    /// `MeshNode.newLiveSwapResponder` — the SAME app-facing FFI ctor the Mac rig's
    /// `--swap-responder-live` uses (docs/swap/G10-RECEIPTS.md). It advertises "gives USDC,
    /// wants NIM" and funds NOTHING until its real `NimHtlcVerifier` sees the counterparty's
    /// NIM HTLC on-chain at depth, then escrows REAL Amoy USDC and claims the NIM leg with the
    /// revealed secret. REAL TEST coins move (Albatross testnet ⇄ Polygon Amoy); mainnet is
    /// never touched — the ctor is testnet/Amoy-guarded, C1-asserted, no mainnet parameter.
    /// The escrow + gas ride a wallet-DERIVED Amoy account (surfaced so it can be funded);
    /// the claimed NIM lands on a wallet-derived claim address (recoverable from the phrase).
    private func swapRespondStart(_ a: [String: Any]) async -> (Bool, Any) {
        let nimLuna = (a["nimLuna"] as? NSNumber)?.uint64Value ?? 0
        let usdcMicro = (a["usdcMicro"] as? NSNumber)?.uint64Value ?? 0
        guard nimLuna > 0, usdcMicro > 0 else { return (false, "amounts required") }
        guard let rs = Wallet.swapResponderSecrets() else {
            return (false, "no wallet yet — create or import one first")
        }
        let fundAddr: String
        do {
            fundAddr = try evmAddressForSecret(secret: rs.fund)
        } catch {
            return (false, "this build lacks the live swap stack: \(error)")
        }

        // §8.3: ARMED (off on any shipped build) → real MAINNET funds via the mainnet ctor +
        // endpoints; the escrow HTLC + NATIVE USDC token are pinned in the core. The fund address
        // is DERIVED from the same secret and is chain-agnostic (an EVM address is the keccak of the
        // pubkey — no chain id), so the MAINNET funding address is IDENTICAL to the testnet one.
        let mainnet = NimmeshCore.mainnetSwapArmed()
        let head = mainnet ? (await mainnetHead() ?? 0) : (await testnetHead() ?? 0)
        let cfg = FfiLiveResponderConfig(
            usdcMicro: usdcMicro, nimLuna: nimLuna,
            expiryHeight: head > 0 ? head + 20_000 : UInt64.max / 2,
            nimClaimSeed: rs.nimClaim,        // owns the NIM claim address (derived, recoverable)
            evmFundingSecret: rs.fund,        // escrows the USDC + pays its gas
            nimRpcUrl: mainnet ? mainnetNimRpcUrl : liveNimRpcUrl,
            amoyRpcUrl: mainnet ? mainnetPolygonRpcUrl : liveAmoyRpcUrl,
            htlcAddress: mainnet ? "" : liveHtlcAddress,   // mainnet: the core pins MAINNET_HTLC_ADDRESS
            usdcAddress: mainnet ? "" : liveUsdcAddress,   // mainnet: the core pins the NATIVE USDC
            deltaSafeBlocks: 0, minClaimWindowBlocks: 0) // 0 = the core's safe defaults

        node.shutdown()
        do {
            var sid = Data(count: 8)
            _ = sid.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 8, $0.baseAddress!) }
            let n: MeshNode
            if mainnet {
                n = try MeshNode.newLiveSwapResponderMainnet(
                    senderId: sid, radio: bleRadio, config: cfg, gatewayRpcUrl: nil)
            } else {
                n = try MeshNode.newLiveSwapResponder(
                    senderId: sid, radio: bleRadio, config: cfg, gatewayRpcUrl: nil)
            }
            node = n
            bleRadio.node = n
            liveFundAddress = fundAddr
            swapDemoOn = true
            swapLiveOn = true
            swapRespondOn = true
            swapMainnetOn = mainnet
            return (true, [
                "ok": true, "mode": mainnet ? "respond-mainnet" : "respond", "head": Int(head),
                "mainnet": mainnet, "fundAddress": fundAddr,
            ])
        } catch {
            let n = makeNormalNode() // never leave the wallet without a node
            node = n
            return (false, "\(error)")
        }
    }

    /// Live demo status: this node's swaps (id + phase), discovery counters, peers — plus,
    /// in live mode, the honest mode label, any REAL NIM locks (mirrored to UserDefaults so
    /// the refund path survives a relaunch), and the gas account to top up.
    func swapMeshStatus() -> (Bool, Any) {
        let swaps: [[String: Any]] = node.activeSwaps().map {
            ["id": $0.swapId, "phase": String(describing: $0.phase)]
        }
        let m = node.discoveryMetrics()
        let baseMode = swapRespondOn ? "respond" : (swapLiveOn ? "live" : "sim")
        var payload: [String: Any] = [
            "demo": swapDemoOn,
            // §8.3: a "-mainnet" suffix on the live modes tells the UI to show the loud
            // "REAL MAINNET FUNDS" labels (off on any shipped build — `swapMainnetOn` is false).
            "mode": (swapMainnetOn && swapLiveOn) ? "\(baseMode)-mainnet" : baseMode,
            "mainnet": swapMainnetOn,
            "swaps": swaps,
            "seen": Int(m.seen), "matched": Int(m.matched), "readvertised": Int(m.readvertised),
            "peers": Int(node.peerCount()),
        ]
        if swapRespondOn, let fund = liveFundAddress {
            payload["fundAddress"] = fund
        }
        if let book = liveLockBook {
            persistLiveLocks(book.locks())
            payload["gasAddress"] = liveGasAddress ?? ""
        }
        let locks = persistedLiveLocks()
        if !locks.isEmpty {
            payload["locks"] = locks.map {
                [
                    "contract": $0.contract, "value": Int($0.value),
                    "timeoutMs": Int($0.timeoutMs), "fundingTxHash": $0.fundingTxHash,
                ] as [String: Any]
            }
        }
        return (true, payload)
    }

    /// End the demo: restore the normal (mainnet) node. Live locks stay persisted — only a
    /// chain-truth `AlreadyResolved` (via `swapMeshRefund`) ever forgets one.
    func swapMeshStop() -> (Bool, Any) {
        guard swapDemoOn else { return (true, ["ok": true]) }
        if let book = liveLockBook { persistLiveLocks(book.locks()) }
        node.shutdown()
        node = makeNormalNode()
        swapDemoOn = false
        swapLiveOn = false
        swapRespondOn = false
        swapMainnetOn = false
        liveFundAddress = nil
        liveLockBook = nil
        return (true, ["ok": true])
    }

    /// G10b (never-strand): try to refund every persisted REAL NIM lock via the core's
    /// `NimHtlcRefunder` — idempotent: `still-locked` before the timeout, `refund-broadcast`
    /// once past it, and only the chain-truth `resolved` (contract emptied) forgets a lock.
    func swapMeshRefund() async -> (Bool, Any) {
        guard let nimKey = Wallet.enclaveKey else { return (false, "no wallet yet") }
        var locks = persistedLiveLocks()
        if let book = liveLockBook {
            for l in book.locks() where !locks.contains(where: { $0.contract == l.contract }) {
                locks.append(l)
            }
        }
        guard !locks.isEmpty else { return (true, ["ok": true, "results": [], "remaining": 0]) }
        do {
            let refunder = try NimHtlcRefunder(nimKey: nimKey, nimRpcUrl: liveNimRpcUrl)
            var results: [[String: Any]] = []
            var remaining: [FfiNimLock] = []
            for lock in locks {
                do {
                    switch try refunder.refund(lock: lock) {
                    case .refunded(let txHash):
                        // Broadcast ≠ landed: keep the lock until a later pass reads the
                        // contract empty (the A2c verified-refund rule).
                        results.append(["contract": lock.contract, "status": "refund-broadcast", "txHash": txHash])
                        remaining.append(lock)
                    case .alreadyResolved:
                        results.append(["contract": lock.contract, "status": "resolved"])
                    case .stillLocked(let untilMs):
                        results.append(["contract": lock.contract, "status": "still-locked", "untilMs": Int(untilMs)])
                        remaining.append(lock)
                    }
                } catch {
                    results.append(["contract": lock.contract, "status": "error", "reason": "\(error)"])
                    remaining.append(lock)
                }
            }
            storeLiveLocks(remaining)
            return (true, ["ok": true, "results": results, "remaining": remaining.count])
        } catch {
            return (false, "\(error)")
        }
    }

    // MARK: live-lock persistence (UserDefaults JSON; no secrets — contracts + amounts only)

    private func persistedLiveLocks() -> [FfiNimLock] {
        guard let data = UserDefaults.standard.data(forKey: liveLocksKey),
              let rows = (try? JSONSerialization.jsonObject(with: data)) as? [[String: Any]]
        else { return [] }
        return rows.compactMap { r in
            guard let contract = r["contract"] as? String,
                  let value = r["value"] as? NSNumber,
                  let timeoutMs = r["timeoutMs"] as? NSNumber
            else { return nil }
            return FfiNimLock(
                contract: contract, value: value.uint64Value, timeoutMs: timeoutMs.uint64Value,
                fundingTxHash: (r["fundingTxHash"] as? String) ?? "")
        }
    }

    private func storeLiveLocks(_ locks: [FfiNimLock]) {
        let rows: [[String: Any]] = locks.map {
            [
                "contract": $0.contract, "value": Int($0.value),
                "timeoutMs": Int($0.timeoutMs), "fundingTxHash": $0.fundingTxHash,
            ]
        }
        if let data = try? JSONSerialization.data(withJSONObject: rows) {
            UserDefaults.standard.set(data, forKey: liveLocksKey)
        }
    }

    /// Merge the book's freshly-recorded locks into the persisted set (keyed by contract).
    private func persistLiveLocks(_ fresh: [FfiNimLock]) {
        var locks = persistedLiveLocks()
        for l in fresh where !locks.contains(where: { $0.contract == l.contract }) {
            locks.append(l)
        }
        storeLiveLocks(locks)
    }

    /// One-shot TESTNET head (the demo intent's expiry anchor); nil when offline. The main
    /// `NimiqRpc` is mainnet-only by design — this single read is the demo's only testnet IO.
    private func testnetHead() async -> UInt64? {
        await headOf(url: "https://rpc.testnet.nimiqwatch.com")
    }

    /// One-shot MAINNET head (the §8.3 mainnet swap intent's expiry anchor); nil when offline.
    /// Used only when `mainnetSwapArmed()` is true.
    private func mainnetHead() async -> UInt64? {
        await headOf(url: mainnetNimRpcUrl)
    }

    /// Read `getBlockNumber` from an Albatross JSON-RPC url (nil on any failure/offline).
    private func headOf(url: String) async -> UInt64? {
        guard let u = URL(string: url) else { return nil }
        var req = URLRequest(url: u)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: [
            "jsonrpc": "2.0", "method": "getBlockNumber", "params": [], "id": 1,
        ])
        guard let (data, _) = try? await URLSession.shared.data(for: req),
              let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else { return nil }
        if let r = json["result"] as? [String: Any], let n = r["data"] as? NSNumber {
            return n.uint64Value
        }
        if let n = json["result"] as? NSNumber { return n.uint64Value }
        return nil
    }
}
