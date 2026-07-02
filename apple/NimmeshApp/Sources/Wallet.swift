import CryptoKit
import Foundation
import NimmeshCore
import Security

/// C1: a Keychain-backed Ed25519 key that implements the Rust `EnclaveKey` foreign trait.
///
/// The signing key is derived from the wallet's BIP39 recovery phrase (see `Wallet`); only the
/// **public key** (32 B, to derive the address) and a **detached signature** (64 B) ever cross
/// the FFI boundary into the Rust core — the phrase/seed never does. CryptoKit's
/// `Curve25519.Signing` is RFC-8032 Ed25519, byte-compatible with the core's `ed25519-dalek`
/// verifier (proven by `Wallet.selfTest()`).
final class KeychainEnclaveKey: EnclaveKey {
    private let privateKey: Curve25519.Signing.PrivateKey

    init(privateKey: Curve25519.Signing.PrivateKey) { self.privateKey = privateKey }

    func publicKey() -> Data { privateKey.publicKey.rawRepresentation }

    func signContent(content: Data) -> Data {
        // CryptoKit Ed25519 signatures are valid RFC-8032 (accepted by dalek verify_strict).
        (try? privateKey.signature(for: content)) ?? Data()
    }
}

/// The app's wallet (C1e): a Nimiq-standard 24-word recovery phrase, stored in the iOS
/// Keychain. The signing key is derived from the phrase (`m/44'/242'/0'/0'`, BIP39 + SLIP-0010,
/// see `Mnemonic.swift`). The phrase/seed never crosses the Rust FFI — only the public key +
/// signature do. There is no silent auto-create: the UI runs onboarding (create or import)
/// first, so the user always owns a backed-up phrase.
enum Wallet {
    private static let service = "com.nimmesh.wallet"
    private static let account = "testnet-bip39-mnemonic"

    /// Cache the derived signer for the loaded phrase (PBKDF2 is cheap but not free); rebuilt
    /// when the stored phrase changes (after create/import).
    private static var cache: (mnemonic: String, signer: AppSigner)?

    // MARK: Wallet lifecycle

    /// Whether a wallet exists yet (drives onboarding vs the home).
    static func hasWallet() -> Bool { readMnemonic() != nil }

    /// Create a brand-new wallet: generate a 24-word phrase, persist it, and return the words
    /// so the UI can show them for backup. `nil` if the wordlist/Keychain is unavailable.
    @discardableResult
    static func createNew() -> String? {
        guard let bip39 = bip39() else { return nil }
        let phrase = bip39.generate()
        guard storeMnemonic(phrase) else { return nil }
        cache = nil
        return phrase
    }

    /// Import an existing wallet from a recovery phrase. Returns `false` if the phrase is not a
    /// valid BIP39 mnemonic (so the caller can show "check your words").
    static func importMnemonic(_ phrase: String) -> Bool {
        let normalized = phrase.lowercased().split(whereSeparator: \.isWhitespace).joined(separator: " ")
        guard let bip39 = bip39(), bip39.isValid(normalized) else { return false }
        guard storeMnemonic(normalized) else { return false }
        cache = nil
        return true
    }

    /// The recovery phrase, for the backup screen (`nil` if no wallet).
    static func recoveryPhrase() -> String? { readMnemonic() }

    /// Remove the wallet from this device. Without its 24 words the wallet is unrecoverable,
    /// so the UI always confirms (and reminds about the backup) before calling this. Used by
    /// the account menu's log-out and the fresh-install "start fresh" choice.
    @discardableResult
    static func delete() -> Bool {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        let status = SecItemDelete(query as CFDictionary)
        cache = nil
        return status == errSecSuccess || status == errSecItemNotFound
    }

    // MARK: Signing

    /// The signer for the current wallet, or `nil` if none exists yet (onboarding not done).
    static var signer: AppSigner? {
        guard let phrase = readMnemonic() else { return nil }
        if let c = cache, c.mnemonic == phrase { return c.signer }
        guard let bip39 = bip39(),
              let keyData = NimiqHD.privateKey(mnemonic: phrase, bip39: bip39), keyData.count == 32,
              let ck = try? Curve25519.Signing.PrivateKey(rawRepresentation: keyData)
        else { return nil }
        let signer = AppSigner(enclaveKey: KeychainEnclaveKey(privateKey: ck))
        cache = (phrase, signer)
        return signer
    }

    /// The wallet's user-friendly `NQ…` address, or `nil` if no wallet / can't derive.
    static func address() -> String? { try? signer?.address() }

    /// Prove the native signer interoperates with the Rust verifier (CryptoKit ↔ ed25519-dalek):
    /// sign a fixed transfer and confirm the core accepts it. `false` if there's no wallet yet.
    static func selfTest() -> Bool {
        guard let signer = signer else { return false }
        let intent = TransferIntent(
            recipient: "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6",
            value: 100_000,
            validityStartHeight: 1,
            network: .mainnet   // local sign+verify only — nothing is broadcast here
        )
        guard let signed = try? signer.signTransfer(intent: intent) else { return false }
        return verifySignedTxHex(rawHex: signed.rawHex)
    }

    // MARK: Wordlist + Keychain

    private static func bip39() -> Bip39? {
        guard let path = Bundle.main.path(forResource: "bip39-english", ofType: "txt") else { return nil }
        return Bip39(wordlistAt: path)
    }

    private static func readMnemonic() -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data, let phrase = String(data: data, encoding: .utf8)
        else { return nil }
        return phrase
    }

    @discardableResult
    private static func storeMnemonic(_ phrase: String) -> Bool {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecValueData as String: Data(phrase.utf8),
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        SecItemDelete(query as CFDictionary) // replace any prior entry
        return SecItemAdd(query as CFDictionary, nil) == errSecSuccess
    }
}
