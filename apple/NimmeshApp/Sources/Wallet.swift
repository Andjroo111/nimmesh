import CryptoKit
import Foundation
import NimmeshCore
import Security

/// C1: a Keychain-backed Ed25519 key that implements the Rust `EnclaveKey` foreign trait.
///
/// The 32-byte seed lives in the iOS **Keychain** (hardware-encrypted at rest,
/// `ThisDeviceOnly`); only the **public key** (32 B, to derive the address) and a **detached
/// signature** (64 B) ever cross the FFI boundary into the Rust core — the seed never does.
/// CryptoKit's `Curve25519.Signing` is RFC-8032 Ed25519, byte-compatible with the core's
/// `ed25519-dalek` verifier (proven by `Wallet.selfTest()`).
final class KeychainEnclaveKey: EnclaveKey {
    private let privateKey: Curve25519.Signing.PrivateKey

    init(privateKey: Curve25519.Signing.PrivateKey) { self.privateKey = privateKey }

    func publicKey() -> Data { privateKey.publicKey.rawRepresentation }

    func signContent(content: Data) -> Data {
        // CryptoKit Ed25519 signatures are deterministic (RFC 8032), 64 bytes.
        (try? privateKey.signature(for: content)) ?? Data()
    }
}

/// The app's wallet: loads (or creates on first launch) the Keychain key and exposes a Rust
/// `AppSigner` over it. Testnet only. Receive/Send read the address + sign through this.
enum Wallet {
    private static let service = "com.nimmesh.wallet"
    private static let account = "testnet-ed25519-seed"

    /// The shared signer for this device's wallet (created once, then loaded from the Keychain).
    static let signer: AppSigner = AppSigner(enclaveKey: KeychainEnclaveKey(privateKey: loadOrCreateKey()))

    /// The wallet's user-friendly `NQ…` testnet address, or `nil` if it can't be derived.
    static func address() -> String? { try? signer.address() }

    /// Prove the native signer interoperates with the Rust verifier: sign a fixed testnet
    /// transfer with the Keychain key and confirm the core accepts the signature. Returns
    /// `true` iff CryptoKit ↔ ed25519-dalek round-trips byte-for-byte.
    static func selfTest() -> Bool {
        let intent = TransferIntent(
            recipient: "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6",
            value: 100_000,
            validityStartHeight: 1,
            network: .testnet
        )
        guard let signed = try? signer.signTransfer(intent: intent) else { return false }
        return verifySignedTxHex(rawHex: signed.rawHex)
    }

    // --- Keychain-backed key persistence -------------------------------------------------

    private static func loadOrCreateKey() -> Curve25519.Signing.PrivateKey {
        if let seed = readSeed(), let key = try? Curve25519.Signing.PrivateKey(rawRepresentation: seed) {
            return key
        }
        let key = Curve25519.Signing.PrivateKey()
        storeSeed(key.rawRepresentation)
        return key
    }

    private static func readSeed() -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess else { return nil }
        return item as? Data
    }

    private static func storeSeed(_ seed: Data) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecValueData as String: seed,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        SecItemDelete(query as CFDictionary) // replace any prior entry
        SecItemAdd(query as CFDictionary, nil)
    }
}
