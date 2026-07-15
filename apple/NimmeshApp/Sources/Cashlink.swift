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
    /// Byte layout per the hub's `Cashlink.render()` — see `CashlinkCodec.encodeFragment`.
    static func hubURL(seed: Data, amountLuna: UInt64, message: String) -> String {
        "https://hub.nimiq.com/cashlink/#" + CashlinkCodec.encodeFragment(
            seed: seed, amountLuna: amountLuna, message: message)
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

    /// One dispatch seam for the whole cashlink family — WebHostView's switch stays a
    /// single case line however many methods this grows (the 800-line guard).
    func cashlinkHandle(method: String, args: Any?) async -> (Bool, Any) {
        switch method {
        case "cashlinkCreate": return await cashlinkCreate(args: args)
        case "cashlinkStatus": return await cashlinkStatus(args: args)
        case "cashlinkPeek": return cashlinkPeek(args: args)
        case "cashlinkClaim": return await cashlinkClaim(args: args)
        default: return (false, "unknown cashlink method: \(method)")
        }
    }

    /// Read a link someone handed us: derive its address (the SAME Rust derivation the
    /// wallet uses) and report amount + message. No signing, nothing stored.
    func cashlinkPeek(args: Any?) -> (Bool, Any) {
        let a = args as? [String: Any] ?? [:]
        let raw = (a["fragment"] as? String) ?? (a["url"] as? String) ?? ""
        guard let frag = CashlinkCodec.fragment(fromUrl: raw),
              let parsed = CashlinkCodec.decodeFragment(frag),
              let key = try? Curve25519.Signing.PrivateKey(rawRepresentation: parsed.seed),
              let address = try? AppSigner(enclaveKey: KeychainEnclaveKey(privateKey: key)).address()
        else { return (false, "not a cashlink") }
        return (true, [
            "address": address, "amountLuna": Int(parsed.amountLuna), "message": parsed.message,
        ])
    }

    /// Claim a received link: the LINK key signs a sweep to MY wallet, delivered over the
    /// normal send path — online RPC when reachable, else anchored to the freshest
    /// mesh-heard head and flooded over BLE (the exact machinery of the mesh Send). The
    /// USER tapped Claim and the funds land in their own wallet; the wallet key never
    /// signs anything here.
    func cashlinkClaim(args: Any?) async -> (Bool, Any) {
        let a = args as? [String: Any] ?? [:]
        guard let frag = CashlinkCodec.fragment(fromUrl: (a["url"] as? String) ?? ""),
              let parsed = CashlinkCodec.decodeFragment(frag),
              let linkKey = try? Curve25519.Signing.PrivateKey(rawRepresentation: parsed.seed)
        else { return (false, "not a cashlink") }
        guard let myAddress = Wallet.address() else { return (false, "no wallet yet") }
        let linkSigner = AppSigner(enclaveKey: KeychainEnclaveKey(privateKey: linkKey))
        guard let linkAddress = try? linkSigner.address() else {
            return (false, "could not derive the link address")
        }
        do {
            // Online: sweep the link's REAL on-chain balance — a fragment can overstate
            // what's left (re-shared or partly swept links stay honest this way).
            let head = try await NimiqRpc.headHeight()
            let balance = try await NimiqRpc.balance(linkAddress)
            guard balance > 0 else { return (false, "already claimed") }
            let intent = TransferIntent(
                recipient: myAddress, value: balance,
                validityStartHeight: head, network: NimiqRpc.network)
            let signed = try linkSigner.signTransfer(intent: intent)
            let hash = try await NimiqRpc.sendRawTransaction(signed.rawHex)
            return (true, ["txHash": hash, "via": "rpc", "amountLuna": Int(balance)])
        } catch {
            // Offline: no chain read possible — sweep the FRAGMENT amount over the mesh.
            // If the link was already swept the tx simply never settles; status stays honest.
            guard let intent = node.anchoredIntent(recipient: myAddress, value: parsed.amountLuna) else {
                return (false, "offline and no gateway head heard yet")
            }
            do {
                let signed = try linkSigner.signTransfer(intent: intent)
                let meshTxId = node.submitSignedTransfer(signedTransfer: signed)
                guard !meshTxId.isEmpty else { return (false, "could not encode the signed tx") }
                return (true, [
                    "txHash": signed.txHash, "via": "mesh", "amountLuna": Int(parsed.amountLuna),
                ])
            } catch {
                return (false, "\(error)")
            }
        }
    }
}
