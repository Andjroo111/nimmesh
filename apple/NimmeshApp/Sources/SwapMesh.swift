import Foundation
import NimmeshCore

/// The over-the-mesh swap demo (Act 1): the REAL swap protocol — discovery, matching, the
/// signed Propose, funding proofs, preimage reveal, retransmit, refund safety — running over
/// the REAL Bluetooth mesh, with SIM transaction bytes (the core's `MockSigner`): no funds can
/// move. For the demo the app's node is swapped onto Albatross TESTNET (mainnet payments pause,
/// clearly labeled in the UI) and the normal mainnet node is restored when the sheet closes.
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

    /// Live demo status: this node's swaps (id + phase), discovery counters, peers.
    func swapMeshStatus() -> (Bool, Any) {
        let swaps: [[String: Any]] = node.activeSwaps().map {
            ["id": $0.swapId, "phase": String(describing: $0.phase)]
        }
        let m = node.discoveryMetrics()
        return (true, [
            "demo": swapDemoOn,
            "swaps": swaps,
            "seen": Int(m.seen), "matched": Int(m.matched), "readvertised": Int(m.readvertised),
            "peers": Int(node.peerCount()),
        ])
    }

    /// End the demo: restore the normal (mainnet) node.
    func swapMeshStop() -> (Bool, Any) {
        guard swapDemoOn else { return (true, ["ok": true]) }
        node.shutdown()
        node = makeNormalNode()
        swapDemoOn = false
        return (true, ["ok": true])
    }

    /// One-shot TESTNET head (the demo intent's expiry anchor); nil when offline. The main
    /// `NimiqRpc` is mainnet-only by design — this single read is the demo's only testnet IO.
    private func testnetHead() async -> UInt64? {
        var req = URLRequest(url: URL(string: "https://rpc.testnet.nimiqwatch.com")!)
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
