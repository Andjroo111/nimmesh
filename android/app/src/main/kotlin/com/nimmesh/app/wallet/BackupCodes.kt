package com.nimmesh.app.wallet

import java.util.Base64

/**
 * The wallet's two backup codes: the real Nimiq wallet's XOR one-time-pad scheme
 * (keyguard `BackupCodes.js`), ported from `Wallet.swift`.
 *
 * `plaintext = versionAndFlags(0) || entropy(32)`, `code1 = KDF(entropy)` (deterministic,
 * so the same wallet always shows the same codes), `code2 = plaintext XOR code1`. Either
 * code alone reveals nothing; both together recover the wallet.
 *
 * Rendered as base64 with the keyguard's narrow-character substitution, `/` to `!` and
 * `+` to `;`, and no padding.
 *
 * nimmesh uses HKDF-SHA256 as the KDF, so these codes are nimmesh-specific and are not
 * importable into the Nimiq wallet. That is inherited from iOS deliberately: the two
 * platforms must agree with each other, and they do, byte for byte.
 */
object BackupCodes {

    private const val INFO = "nimmesh BackupCodes - 0"
    private const val PLAINTEXT_SIZE = 33
    private const val VERSION_AND_FLAGS: Byte = 0x00

    data class Pair(val code1: String, val code2: String)

    fun fromEntropy(entropy: ByteArray): Pair {
        require(entropy.size == 32) { "wallet entropy is 32 bytes, got ${entropy.size}" }
        val plain = ByteArray(PLAINTEXT_SIZE)
        plain[0] = VERSION_AND_FLAGS
        entropy.copyInto(plain, 1)
        val code1 = Hkdf.deriveKey(entropy, INFO.toByteArray(Charsets.UTF_8), PLAINTEXT_SIZE)
        val code2 = ByteArray(PLAINTEXT_SIZE) { i -> (plain[i].toInt() xor code1[i].toInt()).toByte() }
        return Pair(render(code1), render(code2))
    }

    /**
     * Recover the wallet entropy from two codes, order-agnostic like the keyguard: rebuild
     * both codes from the recovered entropy and accept either assignment. Null on malformed
     * input, a version mismatch, or a checksum failure.
     */
    fun entropyFrom(a: String, b: String): ByteArray? {
        val d1 = parse(a) ?: return null
        val d2 = parse(b) ?: return null
        if (d1.size != PLAINTEXT_SIZE || d2.size != PLAINTEXT_SIZE) return null
        val plain = ByteArray(PLAINTEXT_SIZE) { i -> (d1[i].toInt() xor d2[i].toInt()).toByte() }
        if (plain[0] != VERSION_AND_FLAGS) return null
        val entropy = plain.copyOfRange(1, PLAINTEXT_SIZE)
        val derived = fromEntropy(entropy)
        val given = setOf(a.trim(), b.trim())
        if (given != setOf(derived.code1, derived.code2)) return null
        return entropy
    }

    // java.util.Base64 rather than android.util.Base64 (API 26+, and minSdk here is 31).
    // It is identical for this use and it keeps this file free of Android, so the
    // cross-platform vectors run as plain JVM tests in CI instead of needing a device.
    private fun render(data: ByteArray): String =
        Base64.getEncoder().withoutPadding().encodeToString(data)
            .replace('/', '!')
            .replace('+', ';')

    private fun parse(code: String): ByteArray? = try {
        Base64.getDecoder().decode(code.trim().replace('!', '/').replace(';', '+'))
    } catch (e: IllegalArgumentException) {
        null
    }
}
