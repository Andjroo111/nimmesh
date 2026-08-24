package com.nimmesh.app.wallet

import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/**
 * SLIP-0010 hardened-only ed25519 derivation, ported from `Mnemonic.swift` and asserted
 * against the same official SLIP-0010 ed25519 vectors.
 *
 * ed25519 supports hardened derivation only, so there is no public-parent path here and
 * no need for one.
 */
object Slip10 {

    private const val CURVE = "ed25519 seed"

    data class Node(val key: ByteArray, val chainCode: ByteArray) {
        // Data classes over ByteArray get identity equality by default, which silently
        // makes two equal keys compare unequal. Nothing here relies on it, so the
        // generated versions are replaced rather than left as a trap for the next caller.
        override fun equals(other: Any?): Boolean =
            other is Node && key.contentEquals(other.key) && chainCode.contentEquals(other.chainCode)

        override fun hashCode(): Int = 31 * key.contentHashCode() + chainCode.contentHashCode()
    }

    /** Master node from a BIP39 seed: `HMAC-SHA512("ed25519 seed", seed)`. */
    fun master(seed: ByteArray): Node {
        val i = hmacSha512(CURVE.toByteArray(Charsets.UTF_8), seed)
        return Node(i.copyOfRange(0, 32), i.copyOfRange(32, 64))
    }

    /**
     * Hardened child derivation. `index` already carries the hardened bit;
     * `data = 0x00 || key || ser32(index)`.
     */
    fun ckdPriv(parent: Node, index: UInt): Node {
        val data = ByteArray(1 + 32 + 4)
        data[0] = 0x00
        parent.key.copyInto(data, 1)
        data[33] = (index shr 24).toByte()
        data[34] = (index shr 16).toByte()
        data[35] = (index shr 8).toByte()
        data[36] = index.toByte()
        val i = hmacSha512(parent.chainCode, data)
        return Node(i.copyOfRange(0, 32), i.copyOfRange(32, 64))
    }

    /**
     * Walk a full hardened path and return the leaf's 32-byte private key, which is the
     * Ed25519 seed. Every element must already be OR'd with `0x80000000`.
     */
    fun derive(path: List<UInt>, seed: ByteArray): ByteArray =
        path.fold(master(seed)) { node, index -> ckdPriv(node, index) }.key

    fun hmacSha512(key: ByteArray, data: ByteArray): ByteArray =
        Mac.getInstance("HmacSHA512").run {
            init(SecretKeySpec(key, "HmacSHA512"))
            doFinal(data)
        }
}

/** Nimiq's HD wallet path. */
object NimiqHd {

    /** The standard first-account address path, `m/44'/242'/0'/0'`, all hardened. */
    val PATH: List<UInt> = listOf(44u, 242u, 0u, 0u).map { it or 0x8000_0000u }

    /**
     * The 32-byte Ed25519 private key for this device's wallet. Null if the phrase is not
     * a valid BIP39 mnemonic.
     */
    fun privateKey(mnemonic: String, bip39: Bip39, passphrase: String = ""): ByteArray? {
        if (!bip39.isValid(mnemonic)) return null
        return Slip10.derive(PATH, bip39.seedFromMnemonic(mnemonic, passphrase))
    }
}
