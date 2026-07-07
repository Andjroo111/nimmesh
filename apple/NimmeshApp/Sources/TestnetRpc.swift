import Foundation
import NimmeshCore

/// A minimal Nimiq JSON-RPC client (URLSession) for the online send path.
///
/// All cryptography stays in the Rust core (`AppSigner` over the Keychain `EnclaveKey`); this
/// only does network IO — fetch the head for `validityStartHeight`, broadcast the signed blob,
/// poll for inclusion, read a balance and history.
///
/// **MAINNET ONLY.** Andjroo removed the network toggle entirely (2026-07-02: "get rid of the
/// testnet completely") — the owner IS the mainnet gate (`docs/MAINNET-GATING.md`). The app
/// still never auto-sends: a send is always a deliberate user action. The Rust core's tests
/// and tools remain testnet-pinned; only this app surface is mainnet.
enum NimiqRpc {
    /// The network the signer anchors every tx to.
    static let network: NetworkId = .mainnet

    /// The public mainnet JSON-RPC endpoint (nimiqwatch; verified: getBlockNumber,
    /// getAccountByAddress, getTransactionsByAddress and sendRawTransaction all served).
    static let rpcURL = URL(string: "https://rpc.nimiqwatch.com")!

    struct RpcError: Error { let message: String }

    /// One JSON-RPC 2.0 call, unwrapping the Albatross `{ result: { data } }` envelope (the
    /// same shape the proven Rust `HttpGatewayRpc` parses). Returns the unwrapped `data`
    /// (or a bare `result`); `NSNull`/absent → `nil`. Throws on transport / RPC error (the
    /// node returns `error` as a string OR an object — both handled).
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
        // Honest-reachability signal: a completed round-trip proves live internet NOW.
        lastSuccessAt = Date()
        // Albatross wraps the payload under result.data; tolerate a bare result too.
        if let dict = json["result"] as? [String: Any] {
            let d = dict["data"]
            return (d is NSNull) ? nil : d
        }
        let r = json["result"]
        return (r is NSNull) ? nil : r
    }

    /// When the last RPC round-trip succeeded — the app's live-internet signal (the mesh
    /// node can't provide this once the phone itself is a gateway: a self-gateway node
    /// always reports Online regardless of actual connectivity).
    static var lastSuccessAt: Date?

    /// The current head height — the `validityStartHeight` a fresh tx anchors to.
    static func headHeight() async throws -> UInt32 {
        guard let n = try await call("getBlockNumber", []) as? NSNumber else {
            throw RpcError(message: "getBlockNumber: unexpected result shape")
        }
        return n.uint32Value
    }

    /// Broadcast a raw signed transaction (hex); returns the tx hash on accept.
    static func sendRawTransaction(_ rawHex: String) async throws -> String {
        guard let h = try await call("sendRawTransaction", [rawHex]) as? String else {
            throw RpcError(message: "sendRawTransaction: unexpected result shape")
        }
        return h
    }

    /// Whether a tx hash is on-chain yet (honours unconfirmed-until-inclusion).
    static func isIncluded(_ hash: String) async -> Bool {
        guard let obj = (try? await call("getTransactionByHash", [hash])) as? [String: Any],
              let bn = obj["blockNumber"], !(bn is NSNull)
        else { return false }
        return true
    }

    /// An account's balance in luna. **Throws on any transport/RPC failure** — offline must
    /// be distinguishable from "0 NIM", or the UI shows an empty wallet whenever the network
    /// is down (the bug Andjroo hit in the Bluetooth-only test). A genuinely unfunded/unknown
    /// account still reads as 0 from a successful call. Uses `getAccountByAddress`.
    static func balance(_ address: String) async throws -> UInt64 {
        let compact = address.replacingOccurrences(of: " ", with: "")
        guard let obj = (try await call("getAccountByAddress", [compact])) as? [String: Any],
              let b = obj["balance"] as? NSNumber
        else { throw RpcError(message: "getAccountByAddress: unexpected result shape") }
        return b.uint64Value
    }

    /// An address's recent transactions, newest first. **Throws on any transport/RPC
    /// failure** (same honesty as `balance` — offline is not "no transactions"). Uses
    /// `getTransactionsByAddress(address, max, start_at)` — `start_at` is `null` for the
    /// head. Each element is the raw RPC tx object (hash/from/to/value/timestamp/…).
    static func transactions(_ address: String, max: Int = 20) async throws -> [[String: Any]] {
        let compact = address.replacingOccurrences(of: " ", with: "")
        guard let arr = (try await call("getTransactionsByAddress", [compact, max, NSNull()])) as? [[String: Any]]
        else { throw RpcError(message: "getTransactionsByAddress: unexpected result shape") }
        return arr
    }

}
