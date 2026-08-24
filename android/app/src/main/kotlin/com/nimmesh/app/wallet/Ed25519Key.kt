package com.nimmesh.app.wallet

import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer
import uniffi.nimmesh_core.EnclaveKey

/**
 * The Rust `EnclaveKey` foreign trait, implemented in Kotlin.
 *
 * Only the 32-byte public key and a detached 64-byte signature cross the FFI. The seed
 * never does, which is the entire reason this trait exists rather than the core holding
 * the key itself.
 *
 * ### Why BouncyCastle and not the platform
 *
 * `java.security` gained Ed25519 in API 33, and this app's minSdk is 31, so Android 12
 * and 12L have no platform Ed25519 to sign with. AndroidKeyStore is no help either: it
 * supports EC with NIST curves and RSA, and cannot hold an Ed25519 signing key at all.
 *
 * That is not the downgrade it first looks like. iOS is not using the Secure Enclave for
 * this key either: `Wallet.swift` stores raw `Curve25519.Signing` bytes as a Keychain
 * generic password and signs in-process with CryptoKit. The security property both
 * platforms actually provide is the same one, that the key never crosses into Rust, plus
 * encryption at rest ([WalletStore] wraps the phrase with a hardware-backed Keystore key).
 *
 * BouncyCastle is used through its LOW-LEVEL API and is never registered as a JCE
 * provider, because registering one collides with the cut-down BouncyCastle Android
 * already ships.
 */
class Ed25519Key(privateKeySeed: ByteArray) : EnclaveKey {

    init {
        require(privateKeySeed.size == SEED_SIZE) {
            "an Ed25519 seed is $SEED_SIZE bytes, got ${privateKeySeed.size}"
        }
    }

    private val privateKey = Ed25519PrivateKeyParameters(privateKeySeed, 0)

    /** Safe to leak: the core only uses it to derive the sender address. */
    override fun publicKey(): ByteArray = privateKey.generatePublicKey().encoded

    /**
     * Sign the 67-byte `serializeContent` and return the detached 64-byte signature.
     * RFC 8032 Ed25519, which is byte-compatible with the core's `ed25519-dalek`
     * verifier. `Wallet.selfTest()` proves that end to end rather than asserting it.
     */
    override fun signContent(content: ByteArray): ByteArray =
        Ed25519Signer().run {
            init(true, privateKey)
            update(content, 0, content.size)
            generateSignature()
        }

    companion object {
        const val SEED_SIZE = 32
    }
}
