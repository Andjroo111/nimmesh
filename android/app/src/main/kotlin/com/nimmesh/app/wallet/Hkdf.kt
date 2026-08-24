package com.nimmesh.app.wallet

import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/**
 * HKDF-SHA256 (RFC 5869), because the JDK does not ship one and the iOS wallet uses
 * CryptoKit's `HKDF<SHA256>` for the backup codes and the derived swap accounts.
 *
 * Both platforms must produce byte-identical output from the same wallet entropy, or the
 * two backup codes shown on an Android phone would not recover the wallet on an iPhone.
 * Asserted against the RFC 5869 test vectors.
 *
 * CryptoKit's `deriveKey(inputKeyMaterial:info:outputByteCount:)` uses an EMPTY salt, so
 * the extract step hashes with a zero-filled key of the hash length. That is the RFC's own
 * default, but it has to be matched deliberately rather than assumed.
 */
object Hkdf {

    private const val ALGORITHM = "HmacSHA256"
    private const val HASH_LEN = 32

    fun deriveKey(inputKeyMaterial: ByteArray, info: ByteArray, outputByteCount: Int): ByteArray =
        expand(extract(ByteArray(HASH_LEN), inputKeyMaterial), info, outputByteCount)

    fun extract(salt: ByteArray, inputKeyMaterial: ByteArray): ByteArray =
        hmac(if (salt.isEmpty()) ByteArray(HASH_LEN) else salt, inputKeyMaterial)

    fun expand(pseudoRandomKey: ByteArray, info: ByteArray, outputByteCount: Int): ByteArray {
        require(outputByteCount in 1..(255 * HASH_LEN)) {
            "HKDF-SHA256 can produce 1 to ${255 * HASH_LEN} bytes, asked for $outputByteCount"
        }
        val out = ByteArray(outputByteCount)
        var previous = ByteArray(0)
        var written = 0
        var counter = 1
        while (written < outputByteCount) {
            val block = hmac(pseudoRandomKey, previous + info + byteArrayOf(counter.toByte()))
            val take = minOf(block.size, outputByteCount - written)
            block.copyInto(out, written, 0, take)
            written += take
            previous = block
            counter++
        }
        return out
    }

    private fun hmac(key: ByteArray, data: ByteArray): ByteArray =
        Mac.getInstance(ALGORITHM).run {
            init(SecretKeySpec(key, ALGORITHM))
            doFinal(data)
        }
}
