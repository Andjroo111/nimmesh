package com.nimmesh.app.wallet

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Where the recovery phrase lives at rest.
 *
 * The phrase is encrypted with AES-256-GCM under a key held in AndroidKeyStore, which is
 * hardware-backed where the device offers it and non-exportable regardless. Only the
 * ciphertext reaches disk.
 *
 * This is the counterpart of the iOS `kSecClassGenericPassword` item with
 * `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`: device-only, never synced, never backed
 * up (the manifest sets `allowBackup=false`).
 *
 * ⚠ One platform difference that is real and cannot be papered over. On iOS the Keychain
 * SURVIVES an uninstall, so a fresh install can find a previous wallet and has to ask the
 * user whether to keep it. On Android, uninstalling deletes both the app's data and its
 * Keystore key, so a wallet can never outlive the app. `walletStatus` therefore always
 * reports `recovered: false` here, and it is not a stub: that state genuinely cannot occur.
 */
class WalletStore(context: Context) {

    private val prefs = context.applicationContext
        .getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)

    fun hasMnemonic(): Boolean = prefs.contains(KEY_CIPHERTEXT)

    fun readMnemonic(): String? {
        val stored = prefs.getString(KEY_CIPHERTEXT, null) ?: return null
        return try {
            val blob = Base64.decode(stored, Base64.NO_WRAP)
            if (blob.size <= IV_SIZE) return null
            val cipher = Cipher.getInstance(TRANSFORMATION).apply {
                init(
                    Cipher.DECRYPT_MODE,
                    secretKey(),
                    GCMParameterSpec(TAG_BITS, blob, 0, IV_SIZE),
                )
            }
            String(cipher.doFinal(blob, IV_SIZE, blob.size - IV_SIZE), Charsets.UTF_8)
        } catch (e: Exception) {
            // A key invalidated by a device change, or a corrupted blob. Returning null
            // means "no wallet", which the UI already handles; throwing here would crash
            // the app at launch with no path back.
            null
        }
    }

    fun storeMnemonic(phrase: String): Boolean = try {
        val cipher = Cipher.getInstance(TRANSFORMATION).apply { init(Cipher.ENCRYPT_MODE, secretKey()) }
        val ciphertext = cipher.doFinal(phrase.toByteArray(Charsets.UTF_8))
        val blob = cipher.iv + ciphertext
        prefs.edit().putString(KEY_CIPHERTEXT, Base64.encodeToString(blob, Base64.NO_WRAP)).commit()
    } catch (e: Exception) {
        false
    }

    /**
     * Remove the wallet from this device. Without its words it is unrecoverable, so every
     * caller confirms first. The Keystore key goes too: leaving it behind would keep a key
     * around that can only decrypt data that no longer exists.
     */
    fun delete(): Boolean {
        prefs.edit().remove(KEY_CIPHERTEXT).commit()
        return try {
            KeyStore.getInstance(KEYSTORE).apply { load(null) }.deleteEntry(KEY_ALIAS)
            true
        } catch (e: Exception) {
            true // the phrase is already gone, which is what the caller asked for
        }
    }

    private fun secretKey(): SecretKey {
        val keyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (keyStore.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }

        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE).apply {
            init(
                KeyGenParameterSpec.Builder(
                    KEY_ALIAS,
                    KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                )
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setKeySize(KEY_BITS)
                    // A fresh IV per encryption. Reusing one under GCM with the same key
                    // is a total break of the mode, not a weakening of it.
                    .setRandomizedEncryptionRequired(true)
                    // No biometric gate on the key itself: the phrase must be readable to
                    // derive an address at launch. The "unlock your backup" prompt guards
                    // SHOWING the words, and is a separate check (A4).
                    .setUserAuthenticationRequired(false)
                    .build(),
            )
        }.generateKey()
    }

    companion object {
        private const val KEYSTORE = "AndroidKeyStore"
        private const val KEY_ALIAS = "com.nimmesh.wallet.mnemonic"
        private const val PREFS_NAME = "nimmesh.wallet"
        private const val KEY_CIPHERTEXT = "mnemonic.aes-gcm"
        private const val TRANSFORMATION = "AES/GCM/NoPadding"
        private const val KEY_BITS = 256
        private const val TAG_BITS = 128
        private const val IV_SIZE = 12
    }
}
