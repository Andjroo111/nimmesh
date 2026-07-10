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

    // MARK: Backup codes

    /// The wallet's TWO BACKUP CODES — the real Nimiq wallet's XOR one-time-pad scheme
    /// (keyguard `BackupCodes.js`): `plaintext = versionAndFlags(0) + entropy(32)`;
    /// `code1 = KDF(secret)` (deterministic, so the same wallet always yields the same
    /// codes); `code2 = plaintext XOR code1`. EITHER code alone reveals nothing; both
    /// together recover the wallet. Rendered as 44-char base64 with `/`→`!`, `+`→`;`
    /// (the keyguard's narrow-character substitution). nimmesh codes use HKDF-SHA256 as
    /// the KDF, so they are nimmesh-specific (not importable into the Nimiq wallet).
    static func backupCodes() -> (code1: String, code2: String)? {
        guard let phrase = readMnemonic(), let bip39 = bip39(),
              let entropy = bip39.entropy(fromMnemonic: phrase), entropy.count == 32
        else { return nil }
        return codes(fromEntropy: entropy)
    }

    private static func codes(fromEntropy entropy: Data) -> (code1: String, code2: String) {
        var plain = Data([0x00])   // versionAndFlags: version 0, no flags
        plain.append(entropy)
        let key = HKDF<SHA256>.deriveKey(
            inputKeyMaterial: SymmetricKey(data: entropy),
            info: Data("nimmesh BackupCodes - 0".utf8),
            outputByteCount: plain.count
        )
        let code1Data = key.withUnsafeBytes { Data($0) }
        let code2Data = Data(zip(plain, code1Data).map { $0 ^ $1 })
        return (renderCode(code1Data), renderCode(code2Data))
    }

    /// Recover a wallet from its two backup codes (order-agnostic, like the keyguard:
    /// re-derive the codes from the recovered entropy and accept either assignment).
    /// Returns `false` on malformed codes, version mismatch, or checksum failure.
    static func importBackupCodes(_ a: String, _ b: String) -> Bool {
        guard let d1 = parseCode(a), let d2 = parseCode(b),
              d1.count == 33, d2.count == 33
        else { return false }
        let plain = Data(zip(d1, d2).map { $0 ^ $1 })
        guard plain[0] == 0x00 else { return false }   // version 0, no flags
        let entropy = Data(plain.dropFirst())
        let derived = codes(fromEntropy: entropy)
        let inputs = Set([a.trimmingCharacters(in: .whitespacesAndNewlines),
                          b.trimmingCharacters(in: .whitespacesAndNewlines)])
        guard inputs == Set([derived.code1, derived.code2]) else { return false }
        guard let bip39 = bip39(), let phrase = bip39.mnemonic(fromEntropy: entropy) else { return false }
        guard storeMnemonic(phrase) else { return false }
        cache = nil
        return true
    }

    private static func renderCode(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "=", with: "")
            .replacingOccurrences(of: "/", with: "!")
            .replacingOccurrences(of: "+", with: ";")
    }

    private static func parseCode(_ code: String) -> Data? {
        var s = code.trimmingCharacters(in: .whitespacesAndNewlines)
            .replacingOccurrences(of: "!", with: "/")
            .replacingOccurrences(of: ";", with: "+")
        while s.count % 4 != 0 { s += "=" }
        return Data(base64Encoded: s)
    }

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

    /// G10b: the wallet's raw enclave key — the SAME Ed25519 key `signer` signs with, handed
    /// to the Rust core only as the `EnclaveKey` foreign trait (public key + detached
    /// signatures cross the FFI; the phrase/seed never does). The live swap's NIM HTLC is
    /// funded and refunded by THIS key. `nil` before onboarding.
    static var enclaveKey: KeychainEnclaveKey? {
        guard let phrase = readMnemonic(), let bip39 = bip39(),
              let keyData = NimiqHD.privateKey(mnemonic: phrase, bip39: bip39), keyData.count == 32,
              let ck = try? Curve25519.Signing.PrivateKey(rawRepresentation: keyData)
        else { return nil }
        return KeychainEnclaveKey(privateKey: ck)
    }

    /// G10b: the wallet's two DERIVED Amoy (EVM) secrets for the live testnet swap — the
    /// claim-GAS payer and the USDC RECEIVE account — HKDF-SHA256 off the wallet entropy
    /// with distinct labels, so both accounts are recoverable from the recovery phrase alone
    /// (a reinstall strands nothing). The gas secret enters the Rust core once (it must sign
    /// Amoy gas transactions) and never crosses back; the claim secret never leaves Swift —
    /// only its derived ADDRESS does (receiving needs no key custody).
    static func swapEvmSecrets() -> (gas: Data, claim: Data)? {
        guard let phrase = readMnemonic(), let bip39 = bip39(),
              let entropy = bip39.entropy(fromMnemonic: phrase), entropy.count == 32
        else { return nil }
        func derive(_ label: String) -> Data {
            HKDF<SHA256>.deriveKey(
                inputKeyMaterial: SymmetricKey(data: entropy),
                info: Data(label.utf8),
                outputByteCount: 32
            ).withUnsafeBytes { Data($0) }
        }
        return (derive("nimmesh-swap-evm-gas-v1"), derive("nimmesh-swap-evm-claim-v1"))
    }

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
