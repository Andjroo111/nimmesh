package com.nimmesh.app.wallet

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * A3's crypto gate, and the one that decides whether an Android install can recover an
 * iPhone's wallet.
 *
 * Three layers, weakest to strongest:
 *
 *  1. The official BIP39 (Trezor), SLIP-0010 ed25519 and RFC 5869 HKDF vectors. These
 *     prove the port is correct against the standards.
 *  2. **iOS ground truth.** The private keys and backup codes below were produced by
 *     running the SHIPPING Swift code (`Mnemonic.swift`, and `Wallet.codes(fromEntropy:)`
 *     with CryptoKit's `HKDF<SHA256>`) on this machine. Standards conformance alone would
 *     not catch a place where both platforms are wrong in different ways.
 *  3. Rejection cases, because a wallet that accepts a bad phrase is worse than one that
 *     accepts none.
 *
 * A JVM test, not an instrumented one, so it runs in CI on every PR. That matters more
 * here than anywhere else in the app: a silent divergence would not surface as a crash, it
 * would surface as somebody's funds sitting at an address they cannot reach.
 */
class WalletVectorsTest {

    private val bip39: Bip39 = Bip39.fromWordlist(wordlistFile().readText())
        ?: error("the BIP39 wordlist did not parse as 2048 words")

    // ---- BIP39, official Trezor vectors ------------------------------------------------

    private data class Bip39Vector(val entropy: String, val mnemonic: String, val seed: String)

    private val trezorVectors = listOf(
        Bip39Vector(
            "0000000000000000000000000000000000000000000000000000000000000000",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon " +
                "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon " +
                "abandon abandon abandon art",
            "bda85446c68413707090a52022edd26a1c9462295029f2e60cd7c4f2bbd3097170af7a4d73245caf" +
                "a9c3cca8d561a7c3de6f5d4a10be8ed2a5e608d68f92fcc8",
        ),
        Bip39Vector(
            "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
            "legal winner thank year wave sausage worth useful legal winner thank year wave " +
                "sausage worth useful legal winner thank year wave sausage worth title",
            "bc09fca1804f7e69da93c2f2028eb238c227f2e9dda30cd63699232578480a4021b146ad717fbb7e" +
                "451ce9eb835f43620bf5c514db0f8add49f5d121449d3e87",
        ),
        Bip39Vector(
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo " +
                "zoo zoo zoo vote",
            "dd48c104698c30cfe2b6142103248622fb7bb0ff692eebb00089b32d22484e1613912f0a5b694407" +
                "be899ffd31ed3992c456cdf60f5d4564b8ba3f05a69890ad",
        ),
    )

    @Test
    fun bip39MatchesTheOfficialTrezorVectors() {
        trezorVectors.forEach { v ->
            assertEquals(
                "entropy to mnemonic",
                v.mnemonic, bip39.mnemonicFromEntropy(hex(v.entropy)),
            )
            assertEquals(
                "mnemonic to entropy",
                v.entropy, hex(bip39.entropyFromMnemonic(v.mnemonic)!!),
            )
            assertEquals(
                "mnemonic to seed (passphrase TREZOR)",
                v.seed, hex(bip39.seedFromMnemonic(v.mnemonic, "TREZOR")),
            )
        }
    }

    @Test
    fun bip39RejectsWhatItShould() {
        assertTrue(bip39.isValid(trezorVectors[0].mnemonic))
        // 24x "abandon": on the wordlist, right length, wrong checksum.
        assertFalse(
            "a tampered checksum was accepted",
            bip39.isValid(List(24) { "abandon" }.joinToString(" ")),
        )
        assertFalse(
            "an off-list word was accepted",
            bip39.isValid("notaword " + trezorVectors[0].mnemonic),
        )
        assertFalse("a short phrase was accepted", bip39.isValid("abandon abandon"))
        assertNull(bip39.entropyFromMnemonic(""))
    }

    // ---- SLIP-0010 ed25519, official spec vectors --------------------------------------

    @Test
    fun slip10MatchesTheOfficialEd25519Vectors() {
        val seed = hex("000102030405060708090a0b0c0d0e0f")
        fun h(i: UInt) = i or 0x8000_0000u

        assertEquals(
            "2b4be7f19ee27bbf30c667b642d5f4aa69fd169872f8fc3059c08ebae2eb19e7",
            hex(Slip10.derive(emptyList(), seed)),
        )
        assertEquals(
            "68e0fe46dfb67e368c75379acec591dad19df3cde26e63b93a8e704f1dade7a3",
            hex(Slip10.derive(listOf(h(0u)), seed)),
        )
        assertEquals(
            "8f94d394a8e8fd6b1bc2f3f49f5c47e385281d5c17e65324b0f62483e37e8793",
            hex(Slip10.derive(listOf(h(0u), h(1u), h(2u), h(2u), h(1_000_000_000u)), seed)),
        )
    }

    // ---- The one that matters: identical derivation to iOS -----------------------------

    /**
     * The Nimiq path key, `m/44'/242'/0'/0'`, produced by the SHIPPING `Mnemonic.swift`
     * for each Trezor phrase. If Android ever derives something else from the same words,
     * the same recovery phrase opens two different wallets and the user's funds are on the
     * one they are not looking at.
     */
    private val iosNimiqPathKeys = listOf(
        "e56957e4e5dfcc4e1eb41a0f1c2ace51fa04ea244f3f3e63f4921b87fab10714",
        "9d696aadab11d5361dfb44889f0dc520d515f6e6e45422881ad1a7ae86bbba72",
        "11c7744cc444d72d3930c8a091d09dfcfd4c807b619e954f67cc22b86d9e1761",
    )

    @Test
    fun theNimiqPathKeyIsByteIdenticalToWhatIosDerives() {
        trezorVectors.zip(iosNimiqPathKeys).forEach { (vector, expected) ->
            val key = NimiqHd.privateKey(vector.mnemonic, bip39)
            assertEquals("the derived key is not 32 bytes", 32, key?.size)
            assertEquals(
                "Android and iOS derive DIFFERENT keys from the same recovery phrase",
                expected, hex(key!!),
            )
        }
    }

    @Test
    fun derivationIsCaseInsensitiveAndWhitespaceTolerant() {
        val phrase = trezorVectors[0].mnemonic
        val plain = NimiqHd.privateKey(phrase, bip39)!!
        assertArrayEquals(plain, NimiqHd.privateKey(phrase.uppercase(), bip39))
        assertArrayEquals(plain, NimiqHd.privateKey("  ${phrase.replace(" ", "   ")}  ", bip39))
        assertNull(NimiqHd.privateKey("not a real phrase", bip39))
    }

    // ---- HKDF and the backup codes -----------------------------------------------------

    @Test
    fun hkdfMatchesRfc5869() {
        // RFC 5869 test case 1, SHA-256.
        val prk = Hkdf.extract(hex("000102030405060708090a0b0c"), hex("0b".repeat(22)))
        assertEquals(
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
            hex(prk),
        )
        assertEquals(
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf" +
                "34007208d5b887185865",
            hex(Hkdf.expand(prk, hex("f0f1f2f3f4f5f6f7f8f9"), 42)),
        )
    }

    /**
     * Produced by the shipping `Wallet.codes(fromEntropy:)` with CryptoKit's
     * `HKDF<SHA256>`. Both platforms must render the same two strings, or the codes written
     * down on one phone do not restore the wallet on the other.
     *
     * The all-zero vector is degenerate on purpose: with zero entropy the plaintext is all
     * zero too, so `code2 == code1`. It is kept because it is exactly the shape of input
     * that would hide an XOR bug.
     */
    private val iosBackupCodes = listOf(
        Triple(
            "0000000000000000000000000000000000000000000000000000000000000000",
            "OqP2MUNZS8C73;9EG!vATUoQhiCA;owngQikQdGXyWih",
            "OqP2MUNZS8C73;9EG!vATUoQhiCA;owngQikQdGXyWih",
        ),
        Triple(
            "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
            "0CSeCdw7K2Q!QZcVXh0krAe6Quzu!eKWlIUPDoP7DHFq",
            "0FvhdqNEVBtAPuhqIWJb03jFPZORgp3p6!pwcfyEcw4V",
        ),
        Triple(
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "5xuzAfqiLuE;2kVGif70L9psxfybSwjgDZwRfbeVDL3n",
            "5;RM!gVd0R7BJbq5dgEL0CWTOgNktPcf8mPugkhq80IY",
        ),
    )

    @Test
    fun backupCodesAreByteIdenticalToWhatIosRenders() {
        iosBackupCodes.forEach { (entropyHex, code1, code2) ->
            val pair = BackupCodes.fromEntropy(hex(entropyHex))
            assertEquals("code1 diverged from iOS", code1, pair.code1)
            assertEquals("code2 diverged from iOS", code2, pair.code2)
        }
    }

    @Test
    fun backupCodesRoundTripInEitherOrder() {
        val entropy = hex(trezorVectors[1].entropy)
        val pair = BackupCodes.fromEntropy(entropy)
        assertArrayEquals(entropy, BackupCodes.entropyFrom(pair.code1, pair.code2))
        assertArrayEquals(
            "the keyguard accepts the codes in either order, so this must too",
            entropy, BackupCodes.entropyFrom(pair.code2, pair.code1),
        )
    }

    @Test
    fun backupCodesRejectWhatTheyShould() {
        val pair = BackupCodes.fromEntropy(hex(trezorVectors[1].entropy))
        val other = BackupCodes.fromEntropy(hex(trezorVectors[2].entropy))

        assertNull("a code from a DIFFERENT wallet was accepted",
            BackupCodes.entropyFrom(pair.code1, other.code2))
        assertNull("the same code twice was accepted",
            BackupCodes.entropyFrom(pair.code1, pair.code1))
        assertNull("a truncated code was accepted",
            BackupCodes.entropyFrom(pair.code1.dropLast(4), pair.code2))
        assertNull("garbage was accepted", BackupCodes.entropyFrom("not", "codes"))
        assertNull("empty input was accepted", BackupCodes.entropyFrom("", ""))
    }

    // ---- helpers -----------------------------------------------------------------------

    private fun hex(s: String): ByteArray =
        ByteArray(s.length / 2) { s.substring(it * 2, it * 2 + 2).toInt(16).toByte() }

    private fun hex(b: ByteArray): String = b.joinToString("") { "%02x".format(it) }

    private fun wordlistFile(): File {
        // The wordlist is synced from the iOS resources at build time; read the source of
        // truth directly so this test cannot pass against a stale copy.
        val candidates = listOf(
            File("../../apple/NimmeshApp/Resources/bip39-english.txt"),
            File("../apple/NimmeshApp/Resources/bip39-english.txt"),
        )
        return candidates.firstOrNull { it.exists() }
            ?: error("bip39-english.txt not found from ${File(".").absolutePath}")
    }
}
