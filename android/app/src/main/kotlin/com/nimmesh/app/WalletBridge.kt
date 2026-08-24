package com.nimmesh.app

import com.nimmesh.app.wallet.Wallet
import org.json.JSONObject

/**
 * The wallet half of the bridge (A3), split out so [Bridge] stays under the repo's
 * 800-line ceiling. This is the same "family dispatch seam" the iOS bridge uses to add
 * methods without growing `WebHostView.swift` past the guard.
 *
 * Every method here is a faithful port of the matching case in `WebHostView.swift`. Two
 * differ on purpose, and both are noted where they are answered.
 */
class WalletBridge(private val wallet: Wallet, private val prefs: Prefs) {

    /** True if this bridge answers [method]. */
    fun handles(method: String): Boolean = method in METHODS

    fun dispatch(method: String, args: JSONObject?): Pair<Boolean, Any> = when (method) {

        "walletAddress" -> true to json("address" to (wallet.address() ?: ""))

        "walletExists" -> true to json("exists" to wallet.hasWallet())

        "createWallet" -> {
            val phrase = wallet.createNew()
            if (phrase == null) false to "could not create wallet"
            else true to json("mnemonic" to phrase, "address" to (wallet.address() ?: ""))
        }

        "importWallet" -> {
            if (!wallet.importMnemonic(args?.optString("mnemonic").orEmpty())) {
                false to "invalid recovery phrase"
            } else {
                true to json("address" to (wallet.address() ?: ""))
            }
        }

        "recoveryPhrase" -> {
            val phrase = wallet.recoveryPhrase()
            if (phrase == null) false to "no wallet" else true to json("mnemonic" to phrase)
        }

        "walletStatus" -> true to json(
            "exists" to wallet.hasWallet(),
            // ⚠ Always false on Android, and not a stub. On iOS the Keychain SURVIVES an
            // uninstall, so a fresh install can find a previous install's wallet and has
            // to ask the user whether to keep it. Android deletes both the app's data and
            // its Keystore key on uninstall, so a wallet cannot outlive the app and this
            // state genuinely cannot occur. See WalletStore's header.
            "recovered" to false,
        )

        // Nothing to resolve, for the reason above. It still answers rather than
        // rejecting, because the page calls it as part of a normal onboarding flow.
        "resolveRecovered" -> true to json("exists" to wallet.hasWallet())

        "deleteWallet" -> {
            wallet.delete()
            prefs.setBool(Prefs.RECOVERED_WALLET, false)
            prefs.setBool(Prefs.BACKED_UP, false)
            // The cached balance and history belonged to the wallet that was just removed.
            prefs.clearWalletCaches()
            true to json("deleted" to true)
        }

        "backupCodes" -> {
            val codes = wallet.backupCodes()
            if (codes == null) false to "no wallet"
            else true to json("code1" to codes.code1, "code2" to codes.code2)
        }

        "importBackupCodes" -> {
            val ok = wallet.importBackupCodes(
                args?.optString("code1").orEmpty(),
                args?.optString("code2").orEmpty(),
            )
            if (!ok) false to "invalid backup codes"
            else true to json("address" to (wallet.address() ?: ""))
        }

        else -> false to "WalletBridge does not handle $method"
    }

    private fun json(vararg pairs: kotlin.Pair<String, Any?>): JSONObject =
        JSONObject().apply { pairs.forEach { (k, v) -> put(k, v) } }

    companion object {
        val METHODS = setOf(
            "walletAddress", "walletExists", "createWallet", "importWallet", "recoveryPhrase",
            "walletStatus", "resolveRecovered", "deleteWallet", "backupCodes",
            "importBackupCodes",
        )
    }
}
