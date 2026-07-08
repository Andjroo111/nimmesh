import CryptoKit
import Foundation
import NimmeshCore

/// Cashlinks — NIM that travels as a URL. A fresh single-use key is generated on this
/// device, the wallet funds its address with a NORMAL transfer (the proven online or
/// mesh send path — nothing new touches the money path), and the private key + amount
/// ride the URL fragment in the official Nimiq Hub format, so **any browser** can claim
/// at hub.nimiq.com with zero infrastructure of ours. Treat a link like cash: whoever
/// holds it (including us, until they claim) can sweep it.
///
/// Storage: records embed the private key (inside the URL), so the list lives in the
/// iOS **Keychain** — never UserDefaults, never the mesh, never a log.
struct CashlinkRecord: Codable {
    let url: String
    let address: String
    let amountLuna: UInt64
    let message: String
    let createdAt: Double // Unix seconds
    var txHash: String    // funding tx (online) or signed-tx hash (mesh)
    var viaMesh: Bool
}

enum CashlinkVault {
    private static let service = "com.nimmesh.cashlinks"
    private static let account = "records"

    static func load() -> [CashlinkRecord] {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data,
              let list = try? JSONDecoder().decode([CashlinkRecord].self, from: data)
        else { return [] }
        return list
    }

    static func save(_ list: [CashlinkRecord]) {
        guard let data = try? JSONEncoder().encode(list) else { return }
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(base as CFDictionary)
        var add = base
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        SecItemAdd(add as CFDictionary, nil)
    }

    static func add(_ record: CashlinkRecord) {
        var list = load()
        list.insert(record, at: 0)
        if list.count > 100 { list.removeLast(list.count - 100) }
        save(list)
    }

    /// The official Nimiq Hub cashlink URL for MAINNET (`hub.nimiq.com/cashlink/#…`).
    /// Byte layout per the hub's `Cashlink.render()` — see `encodeFragment`.
    static func hubURL(seed: Data, amountLuna: UInt64, message: String) -> String {
        "https://hub.nimiq.com/cashlink/#" + encodeFragment(
            seed: seed, amountLuna: amountLuna, message: message)
    }

    /// The hub fragment, byte-exact per nimiq/hub `Cashlink.render()` (corroborated by the
    /// MIT nimiq/cashlink-generator): `seed(32) ‖ value u64 BE (luna) ‖ [msgLen u8 ‖ msg UTF-8]`.
    /// No version byte, no checksum; theme omitted (0 = unspecified). Minimal link = 40 bytes.
    static func encodeFragment(seed: Data, amountLuna: UInt64, message: String) -> String {
        var buf = Data()
        buf.append(seed) // 32-byte Ed25519 private key seed — the account key itself
        var v = amountLuna.bigEndian
        withUnsafeBytes(of: &v) { buf.append(contentsOf: $0) }
        let msg = Data(message.utf8).prefix(255)
        if !msg.isEmpty {
            buf.append(UInt8(msg.count))
            buf.append(msg)
        }
        return base64UrlHub(buf)
    }

    /// RFC 4648 base64url, UNPADDED — what the current hub emits and parses (`+`→`-`,
    /// `/`→`_`, padding stripped; the legacy `.` padding is accepted but no longer used).
    static func base64UrlHub(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}

extension Bridge {
    /// Create + fund a cashlink. `mesh: true` funds over Bluetooth (offline path, same
    /// anchored-intent flow as the mesh Send); otherwise the online RPC path. The USER
    /// initiated this — same deliberate-send rule as every other transfer.
    func cashlinkCreate(args: Any?) async -> (Bool, Any) {
        let a = args as? [String: Any] ?? [:]
        let amount = (a["amountLuna"] as? NSNumber)?.uint64Value ?? 0
        let message = String(((a["message"] as? String) ?? "").prefix(64))
        let viaMesh = (a["mesh"] as? Bool) ?? false
        guard amount > 0 else { return (false, "missing amount") }
        guard let signer = Wallet.signer else { return (false, "no wallet yet") }

        // Fresh single-use key; its address comes from the same Rust derivation the
        // wallet itself uses (AppSigner over an enclave-wrapped CryptoKit key).
        let linkKey = Curve25519.Signing.PrivateKey()
        let linkSigner = AppSigner(enclaveKey: KeychainEnclaveKey(privateKey: linkKey))
        guard let linkAddress = try? linkSigner.address() else {
            return (false, "could not derive the link address")
        }

        let txHash: String
        do {
            if viaMesh {
                guard let intent = node.anchoredIntent(recipient: linkAddress, value: amount)
                else { return (false, "no gateway head heard yet") }
                let signed = try signer.signTransfer(intent: intent)
                let meshTxId = node.submitSignedTransfer(signedTransfer: signed)
                guard !meshTxId.isEmpty else { return (false, "could not encode the signed tx") }
                txHash = signed.txHash
            } else {
                let head = try await NimiqRpc.headHeight()
                let intent = TransferIntent(
                    recipient: linkAddress, value: amount,
                    validityStartHeight: head, network: NimiqRpc.network)
                let signed = try signer.signTransfer(intent: intent)
                txHash = try await NimiqRpc.sendRawTransaction(signed.rawHex)
            }
        } catch {
            return (false, "funding failed: \(error)")
        }

        let record = CashlinkRecord(
            url: CashlinkVault.hubURL(
                seed: linkKey.rawRepresentation, amountLuna: amount, message: message),
            address: linkAddress, amountLuna: amount, message: message,
            createdAt: Date().timeIntervalSince1970, txHash: txHash, viaMesh: viaMesh)
        CashlinkVault.add(record)
        return (true, Self.recordPayload(record))
    }

    /// The stored links, newest first (the URLs embed keys — they go to the local UI only).
    func cashlinkList() -> (Bool, Any) {
        (true, ["links": CashlinkVault.load().map(Self.recordPayload)])
    }

    /// Live status for one link address: its on-chain balance (0 after funding = claimed).
    func cashlinkStatus(args: Any?) async -> (Bool, Any) {
        let a = args as? [String: Any] ?? [:]
        guard let address = a["address"] as? String, !address.isEmpty else {
            return (false, "missing address")
        }
        do {
            let luna = try await NimiqRpc.balance(address)
            return (true, ["balanceLuna": Int(luna)])
        } catch {
            return (false, "\(error)")
        }
    }

    private static func recordPayload(_ r: CashlinkRecord) -> [String: Any] {
        [
            "url": r.url, "address": r.address, "amountLuna": Int(r.amountLuna),
            "message": r.message, "createdAt": r.createdAt, "txHash": r.txHash,
            "viaMesh": r.viaMesh,
        ]
    }
}
