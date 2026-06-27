import Foundation
import NimmeshCore

/// A minimal Nimiq JSON-RPC client (URLSession) for the online send path.
///
/// All cryptography stays in the Rust core (`AppSigner` over the Keychain `EnclaveKey`); this
/// only does network IO — fetch the head for `validityStartHeight`, broadcast the signed blob,
/// poll for inclusion, read a balance, tap the faucet.
///
/// **Network is a gated toggle, default TESTNET** (persisted in `UserDefaults`). Mainnet is
/// opt-in for the real-funds phone test (`docs/DEVICE-TEST.md`); it is never the default and the
/// app never auto-sends — a mainnet send is a deliberate user action. The faucet is testnet-only.
enum NimiqRpc {
    private static let mainnetKey = "nimmesh.network.mainnet"

    /// Whether the app is pointed at **mainnet** (real funds). Default `false` (testnet).
    static var isMainnet: Bool {
        get { UserDefaults.standard.bool(forKey: mainnetKey) }
        set { UserDefaults.standard.set(newValue, forKey: mainnetKey) }
    }

    /// The selected network as the Rust `NetworkId` the signer anchors the tx to.
    static var network: NetworkId { isMainnet ? .mainnet : .testnet }

    /// The JSON-RPC endpoint for the selected network (both are nimiqwatch public nodes).
    static var rpcURL: URL {
        URL(string: isMainnet ? "https://rpc.nimiqwatch.com" : "https://rpc.testnet.nimiqwatch.com")!
    }

    /// The testnet faucet (testnet only — mainnet has none; fund from your own wallet).
    static let faucetURL = URL(string: "https://faucet.pos.nimiq-testnet.com/tapit")!

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
        // Albatross wraps the payload under result.data; tolerate a bare result too.
        if let dict = json["result"] as? [String: Any] {
            let d = dict["data"]
            return (d is NSNull) ? nil : d
        }
        let r = json["result"]
        return (r is NSNull) ? nil : r
    }

    /// The current testnet head height — the `validityStartHeight` a fresh tx anchors to.
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

    /// An account's balance in luna (0 if unknown / unfunded). Uses `getAccountByAddress`
    /// (the endpoint rejects `getAccount`).
    static func balance(_ address: String) async -> UInt64 {
        let compact = address.replacingOccurrences(of: " ", with: "")
        guard let obj = (try? await call("getAccountByAddress", [compact])) as? [String: Any],
              let b = obj["balance"] as? NSNumber
        else { return 0 }
        return b.uint64Value
    }

    /// Tap the public testnet faucet to fund `address` (~10k NIM, for the in-app demo send).
    static func tapFaucet(_ address: String) async {
        var req = URLRequest(url: faucetURL)
        req.httpMethod = "POST"
        req.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
        req.httpBody = "address=\(address.replacingOccurrences(of: " ", with: ""))".data(using: .utf8)
        _ = try? await URLSession.shared.data(for: req)
    }
}
