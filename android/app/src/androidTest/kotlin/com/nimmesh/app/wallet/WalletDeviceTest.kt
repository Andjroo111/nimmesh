package com.nimmesh.app.wallet

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/**
 * A3 on a real device: the parts that cannot be tested on the JVM because they need
 * AndroidKeyStore, the packaged wordlist asset, and the Rust core.
 *
 * The cross-platform key derivation is proved in `WalletVectorsTest`, which runs in CI.
 * What is proved here is that the wallet survives being stored and read back, and that a
 * signature made by BouncyCastle in Kotlin is accepted by `ed25519-dalek` in Rust.
 */
@RunWith(AndroidJUnit4::class)
class WalletDeviceTest {

    private lateinit var wallet: Wallet

    private val knownPhrase =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon " +
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon " +
            "abandon abandon abandon art"

    @Before
    fun setUp() {
        wallet = Wallet(InstrumentationRegistry.getInstrumentation().targetContext)
        wallet.delete()
    }

    @After
    fun tearDown() {
        wallet.delete()
    }

    @Test
    fun thereIsNoWalletUntilOnboardingCreatesOne() {
        // No silent auto-create. A wallet that appears on its own is a wallet whose owner
        // was never given the chance to write down its words.
        assertFalse(wallet.hasWallet())
        assertNull(wallet.address())
        assertNull(wallet.recoveryPhrase())
        assertNull(wallet.backupCodes())
        assertFalse("selfTest must not claim success with no wallet", wallet.selfTest())
    }

    @Test
    fun aCreatedWalletPersistsThroughTheKeystoreAndDerivesAnAddress() {
        val phrase = wallet.createNew()
        assertNotNull("createNew returned null", phrase)
        assertEquals("a new wallet is 24 words", 24, phrase!!.trim().split(" ").size)
        assertTrue(wallet.hasWallet())

        val address = wallet.address()
        assertNotNull(address)
        assertTrue("not a Nimiq address: $address", address!!.startsWith("NQ"))

        // A SEPARATE instance, so this reads back through AndroidKeyStore rather than
        // through the in-process cache.
        val reopened = Wallet(InstrumentationRegistry.getInstrumentation().targetContext)
        assertEquals("the phrase did not survive being stored", phrase, reopened.recoveryPhrase())
        assertEquals("the address changed across a reopen", address, reopened.address())
    }

    @Test
    fun theCiphertextOnDiskIsNotTheRecoveryPhrase() {
        val phrase = wallet.createNew()!!
        val prefs = InstrumentationRegistry.getInstrumentation().targetContext
            .getSharedPreferences("nimmesh.wallet", android.content.Context.MODE_PRIVATE)
        val stored = prefs.all.values.joinToString(" ") { it.toString() }
        assertTrue("nothing was stored at all", stored.isNotEmpty())
        assertFalse("the recovery phrase is on disk in the clear", stored.contains(phrase))
        phrase.split(" ").take(4).forEach { word ->
            assertFalse("the word '$word' is on disk in the clear", stored.contains(" $word "))
        }
    }

    @Test
    fun twoNewWalletsAreDifferent() {
        val first = wallet.createNew()!!
        val second = wallet.createNew()!!
        assertNotEquals("createNew is not drawing fresh entropy", first, second)
    }

    @Test
    fun importingAKnownPhraseDerivesTheSameAddressEveryTime() {
        assertTrue(wallet.importMnemonic(knownPhrase))
        val address = wallet.address()
        assertNotNull(address)

        // Round-trip it through a delete and a re-import: the address must be a pure
        // function of the words, never of anything this device happens to hold.
        wallet.delete()
        assertFalse(wallet.hasWallet())
        assertTrue(wallet.importMnemonic(knownPhrase.uppercase()))
        assertEquals(address, wallet.address())
    }

    @Test
    fun importingRubbishIsRefusedAndLeavesTheWalletAlone() {
        assertTrue(wallet.importMnemonic(knownPhrase))
        val before = wallet.address()

        assertFalse(wallet.importMnemonic("not a real recovery phrase at all"))
        assertFalse(wallet.importMnemonic(List(24) { "abandon" }.joinToString(" ")))
        assertFalse(wallet.importMnemonic(""))

        assertEquals("a refused import damaged the existing wallet", before, wallet.address())
    }

    @Test
    fun kotlinSignaturesAreAcceptedByTheRustVerifier() {
        // The real interop question: BouncyCastle Ed25519 against ed25519-dalek. Nothing
        // is broadcast; the core signs a fixed transfer and verifies its own bytes back.
        assertTrue(wallet.importMnemonic(knownPhrase))
        assertTrue("the Rust core rejected a signature made in Kotlin", wallet.selfTest())
    }

    @Test
    fun theEnclaveKeyExposesOnlyAPublicKeyAndASignature() {
        assertTrue(wallet.importMnemonic(knownPhrase))
        val key = wallet.enclaveKey()
        assertNotNull(key)
        assertEquals("an Ed25519 public key is 32 bytes", 32, key!!.publicKey().size)
        assertEquals(
            "a detached Ed25519 signature is 64 bytes",
            64, key.signContent(ByteArray(67) { it.toByte() }).size,
        )
    }

    @Test
    fun backupCodesRecoverTheSameWallet() {
        assertTrue(wallet.importMnemonic(knownPhrase))
        val address = wallet.address()
        val codes = wallet.backupCodes()
        assertNotNull(codes)

        wallet.delete()
        assertFalse(wallet.hasWallet())

        assertTrue(wallet.importBackupCodes(codes!!.code1, codes.code2))
        assertEquals("the codes restored a DIFFERENT wallet", address, wallet.address())
        assertEquals("the codes restored a different phrase", knownPhrase, wallet.recoveryPhrase())
    }

    @Test
    fun deletingRemovesEverything() {
        wallet.createNew()
        assertTrue(wallet.hasWallet())
        assertTrue(wallet.delete())
        assertFalse(wallet.hasWallet())
        assertNull(wallet.address())
        assertNull(wallet.recoveryPhrase())

        // And through a fresh instance, so this is not just the cache being cleared.
        val reopened = Wallet(InstrumentationRegistry.getInstrumentation().targetContext)
        assertFalse(reopened.hasWallet())
    }
}
