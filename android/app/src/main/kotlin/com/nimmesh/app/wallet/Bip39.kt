package com.nimmesh.app.wallet

import org.bouncycastle.crypto.digests.SHA512Digest
import org.bouncycastle.crypto.generators.PKCS5S2ParametersGenerator
import org.bouncycastle.crypto.params.KeyParameter
import java.security.MessageDigest
import java.security.SecureRandom
import java.text.Normalizer

/**
 * BIP39, ported from `apple/NimmeshApp/Sources/Mnemonic.swift` and asserted against the
 * same official Trezor vectors.
 *
 * The recovery phrase and the seed live here and nowhere else, and neither ever crosses
 * the Rust FFI. Only a public key and a detached signature do (see [Ed25519Key]).
 */
class Bip39(val words: List<String>) {

    private val index: Map<String, Int> = words.withIndex().associate { (i, w) -> w to i }

    init {
        require(words.size == WORD_COUNT) { "a BIP39 wordlist has $WORD_COUNT words, got ${words.size}" }
    }

    /** A fresh 24-word phrase from 256 bits of CSPRNG entropy. */
    fun generate(): String {
        val entropy = ByteArray(32)
        SecureRandom().nextBytes(entropy)
        return mnemonicFromEntropy(entropy) ?: error("32 bytes of entropy is always encodable")
    }

    /** Encode entropy (16 to 32 bytes, a multiple of 4) as a phrase with its checksum. */
    fun mnemonicFromEntropy(entropy: ByteArray): String? {
        if (entropy.size !in 16..32 || entropy.size % 4 != 0) return null
        val bits = ArrayList<Boolean>(entropy.size * 8 + 8)
        entropy.forEach { b -> for (i in 7 downTo 0) bits.add((b.toInt() shr i) and 1 == 1) }
        val checksumBits = entropy.size * 8 / 32
        val hash = sha256(entropy)
        for (i in 0 until checksumBits) {
            bits.add((hash[i / 8].toInt() shr (7 - i % 8)) and 1 == 1)
        }
        return (bits.indices step 11).joinToString(" ") { chunk ->
            var n = 0
            for (j in 0 until 11) n = (n shl 1) or if (bits[chunk + j]) 1 else 0
            words[n]
        }
    }

    /**
     * Decode a phrase back to its entropy, validating the checksum. Null if any word is
     * off-list, the length is wrong, or the checksum fails.
     */
    fun entropyFromMnemonic(mnemonic: String): ByteArray? {
        val ws = normalizedWords(mnemonic)
        if (ws.size !in VALID_LENGTHS) return null
        val bits = ArrayList<Boolean>(ws.size * 11)
        for (w in ws) {
            val n = index[w] ?: return null
            for (j in 10 downTo 0) bits.add((n shr j) and 1 == 1)
        }
        val entBits = ws.size * 11 * 32 / 33
        val csBits = entBits / 32
        if (entBits % 8 != 0) return null
        val entropy = ByteArray(entBits / 8)
        for (i in 0 until entBits) {
            if (bits[i]) entropy[i / 8] = (entropy[i / 8].toInt() or (1 shl (7 - i % 8))).toByte()
        }
        val hash = sha256(entropy)
        for (i in 0 until csBits) {
            val expected = (hash[i / 8].toInt() shr (7 - i % 8)) and 1 == 1
            if (bits[entBits + i] != expected) return null
        }
        return entropy
    }

    fun isValid(mnemonic: String): Boolean = entropyFromMnemonic(mnemonic) != null

    /** Whitespace-collapsed, lowercased. The form that gets stored, so it round-trips. */
    fun normalize(mnemonic: String): String = normalizedWords(mnemonic).joinToString(" ")

    /**
     * The BIP39 seed: `PBKDF2-HMAC-SHA512(mnemonic, "mnemonic" + passphrase, 2048)`, 64 bytes.
     * Both inputs are NFKD-normalised per spec, which is a no-op for the ASCII English list
     * but correct for any other.
     *
     * BouncyCastle's generator rather than `SecretKeyFactory("PBKDF2WithHmacSHA512")`,
     * because the JCE API takes a `char[]` and Android's implementation keeps only the low
     * 8 bits of each character. That is invisible for ASCII and silently wrong for a
     * non-ASCII passphrase, which would derive a different wallet from the same words.
     */
    fun seedFromMnemonic(mnemonic: String, passphrase: String = ""): ByteArray {
        val password = nfkd(normalize(mnemonic)).toByteArray(Charsets.UTF_8)
        val salt = nfkd("mnemonic$passphrase").toByteArray(Charsets.UTF_8)
        val generator = PKCS5S2ParametersGenerator(SHA512Digest())
        generator.init(password, salt, PBKDF2_ITERATIONS)
        return (generator.generateDerivedParameters(SEED_BITS) as KeyParameter).key
    }

    private fun normalizedWords(mnemonic: String): List<String> =
        mnemonic.lowercase().split(Regex("\\s+")).filter { it.isNotEmpty() }

    private fun nfkd(s: String): String = Normalizer.normalize(s, Normalizer.Form.NFKD)

    private fun sha256(data: ByteArray): ByteArray =
        MessageDigest.getInstance("SHA-256").digest(data)

    companion object {
        private const val WORD_COUNT = 2048
        private const val PBKDF2_ITERATIONS = 2048
        private const val SEED_BITS = 512
        private val VALID_LENGTHS = setOf(12, 15, 18, 21, 24)

        /** Parse a newline-separated wordlist. Null unless it is exactly 2048 words. */
        fun fromWordlist(text: String): Bip39? {
            val ws = text.split(Regex("\\R")).map { it.trim() }.filter { it.isNotEmpty() }
            return if (ws.size == WORD_COUNT) Bip39(ws) else null
        }
    }
}
