package com.nimmesh.app

import android.content.Context
import android.content.SharedPreferences

/**
 * The Android twin of the iOS bridge's `UserDefaults` keys.
 *
 * Two of these exist because a web-layer `localStorage` copy was not durable enough on
 * device. The swap responder role in particular silently reset across relaunches, which
 * made both phones initiators so they could never match (field bug, 2026-07-19). Keys
 * that decide behaviour live natively; the page asks for them.
 *
 * Key names are kept byte-identical to the iOS ones so the two platforms stay legible
 * side by side in a bug report.
 */
class Prefs(context: Context) {

    private val sp: SharedPreferences =
        context.applicationContext.getSharedPreferences("nimmesh", Context.MODE_PRIVATE)

    fun getBool(key: String, default: Boolean = false): Boolean = sp.getBoolean(key, default)
    fun setBool(key: String, value: Boolean) = sp.edit().putBoolean(key, value).apply()

    fun getString(key: String, default: String = ""): String = sp.getString(key, default) ?: default
    fun setString(key: String, value: String) = sp.edit().putString(key, value).apply()

    /** Drop every cached balance/history entry. Used when a wallet is deleted (A3). */
    fun clearWalletCaches() {
        val editor = sp.edit()
        sp.all.keys.filter { it.startsWith(CACHE_PREFIX) }.forEach { editor.remove(it) }
        editor.apply()
    }

    companion object {
        const val BACKED_UP = "nimmesh.backedUp"
        const val LANG = "nimmesh.lang"
        const val RESPOND_ROLE = "nimmesh.swap.respond"
        const val RECOVERED_WALLET = "nimmesh.recoveredWallet"
        const val CACHE_PREFIX = "nimmesh.cache."
    }
}
