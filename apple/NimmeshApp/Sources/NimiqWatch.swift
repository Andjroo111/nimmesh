import Foundation

/// The nimiq.watch block-explorer REST API, used as a READ fallback when the JSON-RPC node
/// is unreachable (#43).
///
/// ### Why this and not a second RPC node
///
/// There is no second public Nimiq JSON-RPC endpoint. Every candidate checked on 2026-08-24
/// (`rpc.nimiq.watch`, `mainnet.nimiq.watch`, `rpc.nimiq.com`, `nimiq.mopsus.com`,
/// `rpc.zeromox.com`, `albatross.nimiq.network`) fails to resolve at all, so an ordered list
/// of RPC nodes would have nothing to fall back TO. The explorer's API is the one independent
/// mainnet source that is actually up, and it was up throughout the outage that prompted this.
///
/// ### What it can and cannot do
///
/// Reads only: head height, balance, transaction history. **It cannot broadcast**, because
/// that needs a real node. So an online send has no fallback and fails loudly rather than
/// appearing to work. The offline mesh send is unaffected: it anchors to a gateway beacon
/// heard over BLE and never touches HTTP.
///
/// Deliberately mirrors `android/app/src/main/kotlin/com/nimmesh/app/net/NimiqWatch.kt`; the
/// two apps share `webui/`, so they must answer the same shapes.
enum NimiqWatch {

    private static let base = "https://api.nimiq.watch/api/v1"

    private static func get(_ path: String) async throws -> Any {
        guard let url = URL(string: base + path) else {
            throw NimiqRpc.RpcError(message: "nimiq.watch: bad url for \(path)")
        }
        let (data, _) = try await URLSession.shared.data(from: url)
        return try JSONSerialization.jsonObject(with: data)
    }

    /// The head height, from the newest indexed block.
    static func headHeight() async throws -> UInt32 {
        guard let blocks = try await get("/latest/1") as? [[String: Any]],
              let height = blocks.first?["height"] as? NSNumber
        else { throw NimiqRpc.RpcError(message: "nimiq.watch: no height in /latest/1") }
        return height.uint32Value
    }

    static func balance(_ address: String) async throws -> UInt64 {
        let compact = address.replacingOccurrences(of: " ", with: "")
        guard let account = try await get("/account/\(compact)") as? [String: Any] else {
            throw NimiqRpc.RpcError(message: "nimiq.watch: unexpected account shape")
        }
        if account["error"] as? Bool == true {
            throw NimiqRpc.RpcError(message: "nimiq.watch: \(account["statusMessage"] ?? "error")")
        }
        guard let balance = account["balance"] as? NSNumber else {
            throw NimiqRpc.RpcError(message: "nimiq.watch: no balance for \(address)")
        }
        return balance.uint64Value
    }

    /// Recent transactions, remapped into the JSON-RPC field names so the ONE place that
    /// decides direction, counterparty and confirmation stays the bridge's `walletHistory`.
    /// Two sources deciding "incoming" separately is two places for it to be wrong.
    static func transactions(_ address: String, max: Int = 20) async throws -> [[String: Any]] {
        let compact = address.replacingOccurrences(of: " ", with: "")
        guard let rows = try await get("/account-transactions/\(compact)/\(max)") as? [[String: Any]] else {
            throw NimiqRpc.RpcError(message: "nimiq.watch: unexpected transactions shape")
        }
        return rows.map { t in
            [
                "hash": t["hash"] as? String ?? "",
                "from": t["sender_address"] as? String ?? "",
                "to": t["receiver_address"] as? String ?? "",
                "value": (t["value"] as? NSNumber) ?? 0,
                // ⚠ The explorer reports SECONDS; the node and the page's
                // `new Date(t.timestamp)` both use MILLISECONDS. Miss this and every
                // transaction renders as 1970.
                "timestamp": NSNumber(value: ((t["timestamp"] as? NSNumber)?.doubleValue ?? 0) * 1000),
                "blockNumber": (t["block_height"] as? NSNumber) ?? 0,
            ]
        }.sorted {
            (($0["blockNumber"] as? NSNumber)?.intValue ?? 0) > (($1["blockNumber"] as? NSNumber)?.intValue ?? 0)
        }
    }
}
