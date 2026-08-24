package com.nimmesh.app

import com.nimmesh.app.net.CoinGecko
import com.nimmesh.app.net.NimiqRpc
import com.nimmesh.app.net.TxRows
import com.nimmesh.app.wallet.Wallet
import org.json.JSONArray
import org.json.JSONObject
import uniffi.nimmesh_core.TransferIntent

/**
 * The bridge methods that talk to the network (A4), split out to keep [Bridge] under the
 * repo's 800-line ceiling.
 *
 * Every method here runs on the bridge's executor, never the main thread.
 *
 * ### Offline continuity is native, and it is not a nicety
 *
 * `walletBalance` and `walletHistory` cache their last GOOD answer and serve it when the
 * network is unreachable, flagged `cached: true`. The alternative is what iOS shipped
 * first: a failed read that returned 0 and an empty list, which rendered a drained wallet
 * during a Bluetooth-only test and then cached that emptiness. The RPC client throws on
 * failure precisely so this layer can tell "offline" apart from "you have nothing".
 */
class NetworkBridge(private val wallet: Wallet, private val prefs: Prefs) {

    fun handles(method: String): Boolean = method in METHODS

    fun dispatch(method: String, args: JSONObject?): Pair<Boolean, Any> = try {
        when (method) {
            "headHeight" -> true to json(
                "height" to NimiqRpc.headHeight().toLong(),
                "source" to NimiqRpc.lastReadSource,
            )
            "walletBalance" -> walletBalance()
            "walletHistory" -> walletHistory()
            "sendTransaction" -> sendTransaction(args)
            "prices" -> true to CoinGecko.prices(args?.optString("currency").orEmpty().ifEmpty { "usd" })
            "market" -> true to CoinGecko.market(
                args?.optString("coin").orEmpty(),
                args?.optString("currency").orEmpty().ifEmpty { "usd" },
            )
            else -> false to "NetworkBridge does not handle $method"
        }
    } catch (e: Exception) {
        false to (e.message ?: e.javaClass.simpleName)
    }

    private fun walletBalance(): Pair<Boolean, Any> {
        val address = wallet.address() ?: return false to "no wallet"
        val key = Prefs.CACHE_PREFIX + "balance." + address.replace(" ", "")
        return try {
            val luna = NimiqRpc.balance(address).toLong()
            prefs.setString(key, luna.toString())
            // `source` is additive and parity-safe. It lets the page say where a number came
            // from rather than presenting an explorer figure as if a node had confirmed it.
            true to json("luna" to luna, "source" to NimiqRpc.lastReadSource)
        } catch (e: Exception) {
            val cached = prefs.getString(key).toLongOrNull()
                ?: return false to (e.message ?: "balance unavailable")
            true to json("luna" to cached, "cached" to true)
        }
    }

    private fun walletHistory(): Pair<Boolean, Any> {
        val address = wallet.address() ?: return false to "no wallet"
        val key = Prefs.CACHE_PREFIX + "txs." + address.replace(" ", "")
        return try {
            val txs = TxRows.normalize(address, NimiqRpc.transactions(address))
            prefs.setString(key, txs.toString())
            true to json("txs" to txs, "source" to NimiqRpc.lastReadSource)
        } catch (e: Exception) {
            val stored = prefs.getString(key)
            if (stored.isEmpty()) return false to (e.message ?: "history unavailable")
            true to json("txs" to JSONArray(stored), "cached" to true)
        }
    }

    /**
     * The online send. Anchored to the head fetched right now, signed by the wallet key,
     * broadcast. The mesh send in [MeshWalletBridge] is the same signature over a different
     * delivery; neither ever sends on its own.
     */
    private fun sendTransaction(args: JSONObject?): Pair<Boolean, Any> {
        val recipient = args?.optString("recipient").orEmpty()
        if (recipient.isEmpty()) return false to "missing recipient"
        val amount = args?.optLong("amountLuna") ?: 0L
        if (amount <= 0L) return false to "missing amount"
        val signer = wallet.signer() ?: return false to "no wallet, create or import one first"

        val head = NimiqRpc.headHeight()
        val signed = signer.signTransfer(
            TransferIntent(
                recipient = recipient,
                value = amount.toULong(),
                validityStartHeight = head,
                network = NimiqRpc.network,
            ),
        )
        return true to json("txHash" to NimiqRpc.sendRawTransaction(signed.rawHex))
    }

    private fun json(vararg pairs: Pair<String, Any?>): JSONObject =
        JSONObject().apply { pairs.forEach { (k, v) -> put(k, v) } }

    companion object {
        val METHODS = setOf(
            "headHeight", "walletBalance", "walletHistory", "sendTransaction", "prices", "market",
        )
    }
}
