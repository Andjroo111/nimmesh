import Foundation

/// Read-only Polygon (mainnet, chain id 137) chain reads for the wallet-derived EVM swap
/// accounts, over native `URLSession`.
///
/// The webui runs on a `file://` origin where WKWebView blocks `fetch()` to the network
/// entirely — so EVERY chain read is a native bridge case, exactly like the CoinGecko `prices`
/// proxy in `WebHostView`. Nothing here signs, broadcasts, or sees a secret: it reads USDC
/// balances (`eth_call balanceOf`), the gas account's POL (`eth_getBalance`), and USDC
/// `Transfer` history (`eth_getLogs`) — all public data, addresses passed in by the page.
///
/// **Never-lose-history:** found transfers merge into a per-address `UserDefaults` cache
/// (union by `txHash`+`logIndex`, monotonic last-scanned block) — the same offline-continuity
/// contract as the NIM tx cache in `WebHostView`. An offline answer serves the cache; a fresh
/// scan only covers the new block range and unions in.
enum PolygonReads {
    /// Public Polygon **mainnet** JSON-RPC. Read-only; no key, no broadcast.
    static let rpcURL = URL(string: "https://polygon.drpc.org")!
    /// Native USDC on Polygon (the FiatTokenProxy) — 6 decimals.
    static let usdcContract = "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359"
    /// `keccak256("Transfer(address,address,uint256)")` — the ERC-20 Transfer topic0.
    static let transferTopic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
    /// `balanceOf(address)` selector.
    static let balanceOfSelector = "0x70a08231"
    /// These swap accounts were first funded ~2026-07-13/14 (Polygon block ≈ 90.2M). Anchor the
    /// very first per-address scan a little before that so nothing is missed, then advance.
    static let initialAnchorBlock = 90_180_000
    /// The `eth_getLogs` range cap observed live on the drpc free tier (`toBlock - fromBlock`
    /// must be ≤ this — a wider range errors `code 35`). History scans in chunks; a range error
    /// fails closed to a smaller step down to `minLogRange`.
    static let maxLogRange = 10_000
    static let minLogRange = 1_000

    struct RpcError: Error { let message: String }

    // MARK: - Bridge entry point

    /// Single entry point so `WebHostView`'s dispatch glue stays tiny (size-guard headroom).
    static func handle(method: String, args: Any?) async -> (Bool, Any) {
        switch method {
        case "usdcBalances": return await balances(args: args)
        case "usdcHistory": return await history(args: args)
        default: return (false, "unknown polygon method: \(method)")
        }
    }

    // MARK: - Balances

    /// `eth_call balanceOf` for each USDC account + `eth_getBalance` for the gas account.
    /// Args: `{ usdc: [addr…], gas: addr? }`.
    /// Answers `{ usdc: { <addr>: microInt }, polWei: { <gasAddr>: "<decimalString>" } }`.
    /// USDC micro fits `Int` for any realistic balance; POL wei (18 decimals) is a decimal
    /// STRING so the page formats it without precision loss.
    private static func balances(args: Any?) async -> (Bool, Any) {
        let a = args as? [String: Any] ?? [:]
        let usdcAddrs = (a["usdc"] as? [String] ?? []).map { $0.lowercased() }.filter { isAddr($0) }
        let gas = (a["gas"] as? String)?.lowercased()
        var usdc: [String: Any] = [:]
        var polWei: [String: Any] = [:]
        for addr in usdcAddrs {
            if let micro = try? await usdcBalanceOf(addr) { usdc[addr] = micro }
        }
        if let gas = gas, isAddr(gas), let wei = try? await nativeBalance(gas) {
            polWei[gas] = wei
        }
        // Only fail when asked for something and read NOTHING (offline) — a genuinely-unfunded
        // account reads 0 from a successful call, which is an honest 0.00, not a failure.
        let asked = !usdcAddrs.isEmpty || (gas != nil)
        if asked && usdc.isEmpty && polWei.isEmpty { return (false, "polygon unreachable") }
        return (true, ["usdc": usdc, "polWei": polWei])
    }

    private static func usdcBalanceOf(_ addr: String) async throws -> Int {
        let data = balanceOfSelector + pad32(addr)
        guard let hex = try await call("eth_call", [["to": usdcContract, "data": data], "latest"]) as? String
        else { throw RpcError(message: "eth_call balanceOf: bad shape") }
        return Int(clampedHex(hex))
    }

    /// The live USDC balance (micro) of one account, or `nil` if unreachable — the source-account
    /// picker in `PolygonSend` uses this to choose which funded account to send from. Read-only.
    static func usdcBalance(of addr: String) async -> Int? {
        guard isAddr(addr) else { return nil }
        return try? await usdcBalanceOf(addr.lowercased())
    }

    private static func nativeBalance(_ addr: String) async throws -> String {
        guard let hex = try await call("eth_getBalance", [addr, "latest"]) as? String
        else { throw RpcError(message: "eth_getBalance: bad shape") }
        return hexToDecimalString(hex)
    }

    // MARK: - History (eth_getLogs Transfer scan + never-lose cache)

    /// Args: `{ addresses: [addr…] }`. Scans USDC `Transfer` logs in/out of each address,
    /// merges into the per-address cache, and answers the union `{ txs: [row…] }` (newest
    /// first). Each row: `hash, from, to, valueMicro, blockNumber, logIndex, timestamp?` (ms;
    /// omitted when the RPC gives no block timestamp — the page shows the block number, never a
    /// faked time).
    private static func history(args: Any?) async -> (Bool, Any) {
        let a = args as? [String: Any] ?? [:]
        let addrs = (a["addresses"] as? [String] ?? []).map { $0.lowercased() }.filter { isAddr($0) }
        // One head read caps every scan; offline (nil) → answer purely from cache (never blank).
        let head = try? await blockNumber()
        var rows: [[String: Any]] = []
        for addr in addrs { rows.append(contentsOf: scanAndMerge(address: addr, head: head)) }
        // De-dup across accounts (a transfer BETWEEN two of our accounts appears for both).
        var byKey: [String: [String: Any]] = [:]
        for r in rows { byKey[rowKey(r)] = r }
        let merged = byKey.values.sorted {
            let ba = ($0["blockNumber"] as? Int ?? 0), bb = ($1["blockNumber"] as? Int ?? 0)
            if ba != bb { return ba > bb }
            return ($0["logIndex"] as? Int ?? 0) > ($1["logIndex"] as? Int ?? 0)
        }
        return (true, ["txs": Array(merged.prefix(100))])
    }

    /// Scan `[lastScanned … head]` for one address (both directions), union into its cache, and
    /// advance `lastScanned` MONOTONICALLY over cleanly-scanned ranges only (a failing chunk
    /// never advances the watermark, so nothing is skipped on the next pass).
    private static func scanAndMerge(address: String, head: Int?) -> [[String: Any]] {
        let scanKey = "nimmesh.cache.usdcscan." + address
        let txsKey = "nimmesh.cache.usdctxs." + address
        var cache: [String: [String: Any]] = [:]
        if let d = UserDefaults.standard.data(forKey: txsKey),
           let arr = (try? JSONSerialization.jsonObject(with: d)) as? [[String: Any]] {
            for r in arr { cache[rowKey(r)] = r }
        }
        guard let head = head else { return Array(cache.values) } // offline: cache only

        let anchored = UserDefaults.standard.object(forKey: scanKey) as? Int ?? initialAnchorBlock
        var cursor = min(anchored, head)
        var step = maxLogRange
        var scannedTo = cursor - 1 // nothing cleanly scanned this pass yet
        // Fetch synchronously via a semaphore so the whole scan is one awaitable unit; each RPC
        // is still a normal async URLSession call underneath.
        while cursor <= head {
            let to = min(cursor + step - 1, head)
            let result = blockingLogs(address: address, from: cursor, to: to)
            switch result {
            case .success(let logs):
                for r in logs { cache[rowKey(r)] = r }
                scannedTo = to
                cursor = to + 1
            case .failure:
                if step > minLogRange { step = max(minLogRange, step / 2); continue }
                cursor = head + 1 // give up cleanly; keep the watermark at the last good chunk
            }
        }
        let rows = Array(cache.values)
        if let d = try? JSONSerialization.data(withJSONObject: rows) {
            UserDefaults.standard.set(d, forKey: txsKey)
        }
        if scannedTo >= anchored {
            UserDefaults.standard.set(max(anchored, scannedTo + 1), forKey: scanKey)
        }
        return rows
    }

    /// Both-direction Transfer logs for a `[from…to]` chunk, decoded to rows. Runs the two async
    /// `eth_getLogs` calls to completion (semaphore) so `scanAndMerge` can stay a plain loop.
    private static func blockingLogs(address: String, from: Int, to: Int) -> Result<[[String: Any]], Error> {
        let sem = DispatchSemaphore(value: 0)
        var out: Result<[[String: Any]], Error> = .success([])
        Task {
            do {
                let incoming = try await transferLogs(address: address, from: from, to: to, incoming: true)
                let outgoing = try await transferLogs(address: address, from: from, to: to, incoming: false)
                out = .success(incoming + outgoing)
            } catch { out = .failure(error) }
            sem.signal()
        }
        sem.wait()
        return out
    }

    private static func transferLogs(address: String, from: Int, to: Int, incoming: Bool) async throws -> [[String: Any]] {
        let addrTopic = "0x" + pad32(address)
        let topics: [Any] = incoming
            ? [transferTopic, NSNull(), addrTopic]  // topic2 = to  → incoming
            : [transferTopic, addrTopic, NSNull()]  // topic1 = from → outgoing
        let filter: [String: Any] = [
            "fromBlock": hex(from), "toBlock": hex(to),
            "address": usdcContract, "topics": topics,
        ]
        guard let arr = try await call("eth_getLogs", [filter]) as? [[String: Any]] else {
            throw RpcError(message: "eth_getLogs: bad shape")
        }
        return arr.compactMap { decodeTransfer($0) }
    }

    /// Decode one ERC-20 Transfer log into a UI row. `from`/`to` are the last 20 bytes of the
    /// indexed topics; `data` is the uint256 value (USDC = 6 decimals). `blockTimestamp` is used
    /// only when the RPC includes it (drpc does) — never synthesised.
    private static func decodeTransfer(_ log: [String: Any]) -> [String: Any]? {
        guard let topics = log["topics"] as? [String], topics.count >= 3,
              let hash = log["transactionHash"] as? String else { return nil }
        var row: [String: Any] = [
            "hash": hash,
            "from": "0x" + String(topics[1].suffix(40)).lowercased(),
            "to": "0x" + String(topics[2].suffix(40)).lowercased(),
            "valueMicro": Int(clampedHex(log["data"] as? String ?? "0x0")),
            "blockNumber": Int(clampedHex(log["blockNumber"] as? String ?? "0x0")),
            "logIndex": Int(clampedHex(log["logIndex"] as? String ?? "0x0")),
        ]
        if let ts = log["blockTimestamp"] as? String {
            row["timestamp"] = Int(clampedHex(ts)) * 1000 // seconds → ms for the JS Date()
        }
        return row
    }

    // MARK: - JSON-RPC + hex helpers

    private static func call(_ method: String, _ params: [Any]) async throws -> Any? {
        var req = URLRequest(url: rpcURL)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONSerialization.data(withJSONObject: [
            "jsonrpc": "2.0", "method": method, "params": params, "id": 1,
        ])
        let (data, _) = try await URLSession.shared.data(for: req)
        let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        if let err = json["error"], !(err is NSNull) {
            throw RpcError(message: "\(method): \(err)")
        }
        return json["result"]
    }

    private static func blockNumber() async throws -> Int {
        guard let h = try await call("eth_blockNumber", []) as? String else {
            throw RpcError(message: "eth_blockNumber: bad shape")
        }
        return Int(clampedHex(h))
    }

    /// A stable per-row cache/dedup key: `txHash-logIndex` (a tx can emit several Transfers).
    private static func rowKey(_ r: [String: Any]) -> String {
        "\(r["hash"] as? String ?? "")-\(r["logIndex"] as? Int ?? 0)"
    }

    private static func isAddr(_ s: String) -> Bool {
        let a = s.hasPrefix("0x") ? String(s.dropFirst(2)) : s
        return a.count == 40 && a.allSatisfy { $0.isHexDigit }
    }

    /// A 20-byte EVM address left-padded to a 32-byte (64 hex) word, no `0x` prefix.
    private static func pad32(_ addr: String) -> String {
        let a = (addr.hasPrefix("0x") ? String(addr.dropFirst(2)) : addr).lowercased()
        return String(repeating: "0", count: max(0, 64 - a.count)) + a
    }

    private static func hex(_ n: Int) -> String { "0x" + String(n, radix: 16) }

    /// Parse a `0x` hex integer, clamped to `Int64` range (USDC micro / block numbers /
    /// timestamps all fit; a pathological 256-bit value clamps instead of overflowing).
    private static func clampedHex(_ s: String) -> Int64 {
        let h = s.hasPrefix("0x") ? String(s.dropFirst(2)) : s
        var v: UInt64 = 0
        for c in h {
            guard let d = c.hexDigitValue else { return 0 }
            let (m, o1) = v.multipliedReportingOverflow(by: 16)
            let (a, o2) = m.addingReportingOverflow(UInt64(d))
            if o1 || o2 { return Int64.max }
            v = a
        }
        return v > UInt64(Int64.max) ? Int64.max : Int64(v)
    }

    /// A `0x` hex integer of arbitrary width → its exact decimal string (schoolbook base-16→10,
    /// no bignum dependency). Used for POL wei (18 decimals, beyond `Int64`).
    private static func hexToDecimalString(_ s: String) -> String {
        let h = s.hasPrefix("0x") ? String(s.dropFirst(2)) : s
        var dec = [0] // little-endian decimal digits
        for c in h {
            guard let nib = c.hexDigitValue else { continue }
            var carry = nib
            for i in 0..<dec.count {
                let val = dec[i] * 16 + carry
                dec[i] = val % 10
                carry = val / 10
            }
            while carry > 0 { dec.append(carry % 10); carry /= 10 }
        }
        return String(dec.reversed().map { Character(String($0)) })
    }
}
