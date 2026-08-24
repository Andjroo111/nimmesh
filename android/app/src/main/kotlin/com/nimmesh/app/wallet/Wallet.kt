package com.nimmesh.app.wallet

import android.content.Context
import android.util.Log
import uniffi.nimmesh_core.AppSigner
import uniffi.nimmesh_core.NetworkId
import uniffi.nimmesh_core.TransferIntent
import uniffi.nimmesh_core.verifySignedTxHex

/**
 * The app's wallet: a Nimiq-standard 24-word recovery phrase, encrypted at rest, with the
 * signing key derived from it at `m/44'/242'/0'/0'`.
 *
 * Ported from `apple/NimmeshApp/Sources/Wallet.swift`. The phrase and seed never cross the
 * Rust FFI; only a public key and detached signatures do.
 *
 * There is no silent auto-create. Onboarding runs first, so the user always owns a phrase
 * they had the chance to write down.
 */
class Wallet(context: Context) {

    private val appContext = context.applicationContext
    private val store = WalletStore(appContext)

    /** Derived from the phrase, so it is rebuilt whenever the stored phrase changes. */
    private var cached: Pair<String, AppSigner>? = null

    // ---- lifecycle -------------------------------------------------------------------

    fun hasWallet(): Boolean = store.hasMnemonic()

    /**
     * A brand-new wallet. Returns the words so the UI can show them for backup, or null if
     * the wordlist or the keystore is unavailable.
     */
    fun createNew(): String? {
        val bip39 = bip39() ?: return null
        val phrase = bip39.generate()
        if (!store.storeMnemonic(phrase)) return null
        cached = null
        return phrase
    }

    /** Import from a phrase. False if it is not a valid BIP39 mnemonic. */
    fun importMnemonic(phrase: String): Boolean {
        val bip39 = bip39() ?: return false
        val normalized = bip39.normalize(phrase)
        if (!bip39.isValid(normalized)) return false
        if (!store.storeMnemonic(normalized)) return false
        cached = null
        return true
    }

    /** The phrase, for the in-app backup screen. Null if there is no wallet. */
    fun recoveryPhrase(): String? = store.readMnemonic()

    fun delete(): Boolean {
        cached = null
        return store.delete()
    }

    // ---- backup codes ----------------------------------------------------------------

    fun backupCodes(): BackupCodes.Pair? {
        val phrase = store.readMnemonic() ?: return null
        val entropy = bip39()?.entropyFromMnemonic(phrase) ?: return null
        if (entropy.size != 32) return null
        return BackupCodes.fromEntropy(entropy)
    }

    fun importBackupCodes(a: String, b: String): Boolean {
        val entropy = BackupCodes.entropyFrom(a, b) ?: return false
        val bip39 = bip39() ?: return false
        val phrase = bip39.mnemonicFromEntropy(entropy) ?: return false
        if (!store.storeMnemonic(phrase)) return false
        cached = null
        return true
    }

    // ---- signing ---------------------------------------------------------------------

    /** The signer for the current wallet, or null before onboarding. */
    fun signer(): AppSigner? {
        val phrase = store.readMnemonic() ?: return null
        cached?.let { (mnemonic, signer) -> if (mnemonic == phrase) return signer }
        val bip39 = bip39() ?: return null
        val seed = NimiqHd.privateKey(phrase, bip39) ?: return null
        if (seed.size != Ed25519Key.SEED_SIZE) return null
        val signer = AppSigner(Ed25519Key(seed))
        cached = phrase to signer
        return signer
    }

    /** The wallet's `NQ...` address, or null if there is no wallet. */
    fun address(): String? = try {
        signer()?.address()
    } catch (e: Exception) {
        null
    }

    /**
     * The wallet's raw enclave key, the same one [signer] signs with. Handed to the core
     * only as the `EnclaveKey` foreign trait, so a public key and signatures cross the FFI
     * and the seed does not.
     */
    fun enclaveKey(): Ed25519Key? {
        val phrase = store.readMnemonic() ?: return null
        val bip39 = bip39() ?: return null
        val seed = NimiqHd.privateKey(phrase, bip39) ?: return null
        return if (seed.size == Ed25519Key.SEED_SIZE) Ed25519Key(seed) else null
    }

    /**
     * Prove the Kotlin signer interoperates with the Rust verifier, BouncyCastle Ed25519
     * against `ed25519-dalek`: sign a fixed transfer and confirm the core accepts it.
     * Nothing is broadcast; this is a local sign and verify. False if there is no wallet.
     */
    fun selfTest(): Boolean = try {
        val signer = signer()
        if (signer == null) {
            false
        } else {
            val signed = signer.signTransfer(
                TransferIntent(
                    recipient = SELF_TEST_RECIPIENT,
                    value = 100_000uL,
                    validityStartHeight = 1u,
                    network = NetworkId.MAINNET,
                ),
            )
            verifySignedTxHex(signed.rawHex)
        }
    } catch (e: Exception) {
        Log.e(TAG, "wallet self-test threw", e)
        false
    }

    // ---- wordlist --------------------------------------------------------------------

    private fun bip39(): Bip39? = wordlist ?: synchronized(this) {
        wordlist ?: run {
            val parsed = try {
                Bip39.fromWordlist(
                    appContext.assets.open(WORDLIST_ASSET).bufferedReader().use { it.readText() },
                )
            } catch (e: Exception) {
                Log.e(TAG, "could not read $WORDLIST_ASSET from assets", e)
                null
            }
            wordlist = parsed
            parsed
        }
    }

    companion object {
        private const val TAG = "nimmesh.wallet"
        private const val WORDLIST_ASSET = "bip39-english.txt"
        private const val SELF_TEST_RECIPIENT = "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6"

        // The wordlist is 2048 immutable strings and parsing it is the slow part of every
        // key derivation, so it is read once per process rather than per call.
        @Volatile
        private var wordlist: Bip39? = null
    }
}
