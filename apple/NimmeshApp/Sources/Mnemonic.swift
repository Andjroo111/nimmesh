import CommonCrypto
import CryptoKit
import Foundation
import Security

/// BIP39 + SLIP-0010 key derivation for the wallet's recovery phrase (C1e).
///
/// This is the ONLY place the recovery phrase / seed is handled, and it lives entirely
/// native-side (Swift) — the seed never crosses the Rust FFI (only the public key + a
/// signature do). It produces a 32-byte Ed25519 private key from a Nimiq-standard 24-word
/// phrase (`m/44'/242'/0'/0'`), which CryptoKit then signs with and the Rust core turns into
/// the `NQ…` address. Verified against the official BIP39 (Trezor) and SLIP-0010 ed25519 test
/// vectors by `apple/scripts/verify-mnemonic-main.swift`.
///
/// Deliberately free of `NimmeshCore` (and any app singletons) so it compiles + runs as a
/// standalone CLI for vector verification.

// MARK: - BIP39

/// A BIP39 mnemonic codec bound to a wordlist (the official 2048-word English list).
struct Bip39 {
    let words: [String]
    private let index: [String: Int]

    init(words: [String]) {
        self.words = words
        var idx = [String: Int](minimumCapacity: words.count)
        for (i, w) in words.enumerated() { idx[w] = i }
        self.index = idx
    }

    /// Load the wordlist from a newline-separated file (CLI / bundle resource).
    init?(wordlistAt path: String) {
        guard let text = try? String(contentsOfFile: path, encoding: .utf8) else { return nil }
        let ws = text.split(whereSeparator: \.isNewline).map(String.init)
        guard ws.count == 2048 else { return nil }
        self.init(words: ws)
    }

    /// Generate a fresh 24-word phrase from 256 bits of CSPRNG entropy.
    func generate() -> String {
        var entropy = Data(count: 32)
        let rc = entropy.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!) }
        precondition(rc == errSecSuccess, "CSPRNG failed")
        return mnemonic(fromEntropy: entropy)!
    }

    /// Encode entropy (16–32 bytes, multiple of 4) as a mnemonic with the BIP39 checksum.
    func mnemonic(fromEntropy entropy: Data) -> String? {
        guard entropy.count >= 16, entropy.count <= 32, entropy.count % 4 == 0 else { return nil }
        var bits = bitArray(entropy)
        let checksumBits = entropy.count * 8 / 32
        let hash = Array(SHA256.hash(data: entropy))
        for i in 0..<checksumBits { bits.append((hash[i / 8] >> (7 - i % 8)) & 1 == 1) }
        var out = [String]()
        for chunk in stride(from: 0, to: bits.count, by: 11) {
            var n = 0
            for j in 0..<11 { n = (n << 1) | (bits[chunk + j] ? 1 : 0) }
            out.append(words[n])
        }
        return out.joined(separator: " ")
    }

    /// Decode a mnemonic back to its entropy, validating the checksum. `nil` if any word is
    /// off-list, the length is wrong, or the checksum fails.
    func entropy(fromMnemonic mnemonic: String) -> Data? {
        let ws = normalizedWords(mnemonic)
        guard [12, 15, 18, 21, 24].contains(ws.count) else { return nil }
        var bits = [Bool]()
        for w in ws {
            guard let n = index[w] else { return nil }
            for j in (0..<11).reversed() { bits.append((n >> j) & 1 == 1) }
        }
        let entBits = ws.count * 11 * 32 / 33
        let csBits = entBits / 32
        guard entBits % 8 == 0 else { return nil }
        var entropy = Data(count: entBits / 8)
        for i in 0..<entBits where bits[i] { entropy[i / 8] |= 1 << (7 - i % 8) }
        let hash = Array(SHA256.hash(data: entropy))
        for i in 0..<csBits {
            let expected = (hash[i / 8] >> (7 - i % 8)) & 1 == 1
            if bits[entBits + i] != expected { return nil }
        }
        return entropy
    }

    /// Whether a phrase is a valid BIP39 mnemonic (words on-list + checksum holds).
    func isValid(_ mnemonic: String) -> Bool { entropy(fromMnemonic: mnemonic) != nil }

    /// BIP39 seed: PBKDF2-HMAC-SHA512(mnemonic, "mnemonic"+passphrase, 2048) → 64 bytes.
    /// Inputs are NFKD-normalised per spec (a no-op for the ASCII English list, but correct).
    func seed(fromMnemonic mnemonic: String, passphrase: String = "") -> Data {
        let pw = normalizedWords(mnemonic).joined(separator: " ")
            .decomposedStringWithCompatibilityMapping.data(using: .utf8)!
        let salt = ("mnemonic" + passphrase)
            .decomposedStringWithCompatibilityMapping.data(using: .utf8)!
        return pbkdf2SHA512(password: pw, salt: salt, iterations: 2048, keyLength: 64)
    }

    private func normalizedWords(_ mnemonic: String) -> [String] {
        mnemonic.lowercased().split(whereSeparator: \.isWhitespace).map(String.init)
    }

    private func bitArray(_ data: Data) -> [Bool] {
        var bits = [Bool]()
        bits.reserveCapacity(data.count * 8)
        for b in data { for i in (0..<8).reversed() { bits.append((b >> i) & 1 == 1) } }
        return bits
    }
}

// MARK: - SLIP-0010 (ed25519, hardened-only)

enum Slip10 {
    /// Master key from a BIP39 seed: HMAC-SHA512("ed25519 seed", seed) → (key, chainCode).
    static func master(seed: Data) -> (key: Data, chainCode: Data) {
        let i = hmacSHA512(key: Data("ed25519 seed".utf8), data: seed)
        return (i.prefix(32), i.suffix(32))
    }

    /// Hardened child derivation (ed25519 supports only hardened): the index already carries
    /// the hardened bit. data = 0x00 || key || ser32(index).
    static func ckdPriv(_ parent: (key: Data, chainCode: Data), index: UInt32) -> (key: Data, chainCode: Data) {
        var data = Data([0x00])
        data.append(parent.key)
        data.append(contentsOf: [UInt8(index >> 24), UInt8(index >> 16 & 0xff), UInt8(index >> 8 & 0xff), UInt8(index & 0xff)])
        let i = hmacSHA512(key: parent.chainCode, data: data)
        return (i.prefix(32), i.suffix(32))
    }

    /// Derive a full hardened path; each element must already be OR'd with 0x80000000.
    /// Returns the leaf node's 32-byte private key (the Ed25519 seed).
    static func derive(path: [UInt32], seed: Data) -> Data {
        var node = master(seed: seed)
        for index in path { node = ckdPriv(node, index: index) }
        return node.key
    }
}

// MARK: - Nimiq HD wallet

enum NimiqHD {
    /// Nimiq's standard first-account address path: m/44'/242'/0'/0' (all hardened).
    static let path: [UInt32] = [44, 242, 0, 0].map { $0 | 0x8000_0000 }

    /// Derive the 32-byte Ed25519 private key for this device's wallet from a recovery phrase.
    /// Returns `nil` if the phrase is not a valid BIP39 mnemonic.
    static func privateKey(mnemonic: String, bip39: Bip39, passphrase: String = "") -> Data? {
        guard bip39.isValid(mnemonic) else { return nil }
        let seed = bip39.seed(fromMnemonic: mnemonic, passphrase: passphrase)
        return Slip10.derive(path: path, seed: seed)
    }
}

// MARK: - Primitives

func hmacSHA512(key: Data, data: Data) -> Data {
    let mac = HMAC<SHA512>.authenticationCode(for: data, using: SymmetricKey(data: key))
    return Data(mac)
}

func pbkdf2SHA512(password: Data, salt: Data, iterations: Int, keyLength: Int) -> Data {
    var derived = Data(count: keyLength)
    let status = derived.withUnsafeMutableBytes { dPtr -> Int32 in
        salt.withUnsafeBytes { sPtr -> Int32 in
            password.withUnsafeBytes { pPtr -> Int32 in
                CCKeyDerivationPBKDF(
                    CCPBKDFAlgorithm(kCCPBKDF2),
                    pPtr.baseAddress!.assumingMemoryBound(to: CChar.self), password.count,
                    sPtr.baseAddress!.assumingMemoryBound(to: UInt8.self), salt.count,
                    CCPseudoRandomAlgorithm(kCCPRFHmacAlgSHA512),
                    UInt32(iterations),
                    dPtr.baseAddress!.assumingMemoryBound(to: UInt8.self), keyLength
                )
            }
        }
    }
    precondition(status == kCCSuccess, "PBKDF2 failed")
    return derived
}
