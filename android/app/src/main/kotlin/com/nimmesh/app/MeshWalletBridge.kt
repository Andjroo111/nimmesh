package com.nimmesh.app

import com.nimmesh.app.wallet.Wallet
import org.json.JSONArray
import org.json.JSONObject
import uniffi.nimmesh_core.MeshNode
import uniffi.nimmesh_core.NetworkId

/**
 * The bridge methods that need BOTH the mesh node and the wallet (A3), split out to keep
 * [Bridge] under the repo's 800-line ceiling.
 *
 * Nothing here touches the network. `meshSendTransaction` in particular is the offline
 * path: it anchors to a gateway head heard over Bluetooth, signs locally, and floods the
 * signed blob. There is no RPC call anywhere on it.
 */
class MeshWalletBridge(private val node: MeshNode, private val wallet: Wallet) {

    fun handles(method: String): Boolean = method in METHODS

    fun dispatch(method: String, args: JSONObject?): Pair<Boolean, Any> {
        val address = wallet.address()
        if (address.isNullOrEmpty() && method != "meshSendTransaction") return false to "no wallet"

        return when (method) {

            // Flood a balance query. Fire and forget: the answer lands in the node's cache
            // and is read back through meshCachedBalance.
            "meshQueryBalance" -> {
                node.queryBalance(address!!)
                true to json("ok" to true)
            }

            // Last-known and UNVERIFIED, per the core's contract: a relay is untrusted, so
            // the UI shows it as such and uses headHeight for a freshness stamp.
            "meshCachedBalance" -> {
                val cached = node.cachedBalance(address!!)
                if (cached == null) true to json("has" to false)
                else true to json(
                    "has" to true,
                    "luna" to cached.balance.toLong(),
                    "headHeight" to cached.headHeight.toLong(),
                )
            }

            "meshQueryHistory" -> {
                node.queryTxHistory(address!!)
                true to json("ok" to true)
            }

            "meshCachedHistory" -> {
                val rows = node.cachedTxHistory(address!!)
                val txs = JSONArray()
                rows.forEach { r ->
                    txs.put(
                        json(
                            "hash" to r.hash,
                            "counterparty" to r.counterparty,
                            "valueLuna" to r.valueLuna.toLong(),
                            "timestamp" to r.timestampMs.toDouble(),
                            "incoming" to r.incoming,
                            "confirmed" to r.confirmed,
                        ),
                    )
                }
                true to json(
                    "txs" to txs,
                    "headHeight" to (rows.firstOrNull()?.headHeight?.toLong() ?: 0L),
                )
            }

            "meshSendTransaction" -> meshSend(args)

            else -> false to "MeshWalletBridge does not handle $method"
        }
    }

    /**
     * The offline send. The intent is anchored to the freshest gateway head beacon heard
     * over Bluetooth, never to a local clock and never to a zero head, because pre-dating a
     * transaction burns its validity window. It is signed with the same wallet key the
     * online send uses: the mesh changes the delivery, never who signs.
     */
    private fun meshSend(args: JSONObject?): Pair<Boolean, Any> {
        val recipient = args?.optString("recipient").orEmpty()
        if (recipient.isEmpty()) return false to "missing recipient"
        val amount = args?.optLong("amountLuna") ?: 0L
        if (amount <= 0L) return false to "missing amount"

        val signer = wallet.signer() ?: return false to "no wallet yet"
        val intent = node.anchoredIntent(recipient, amount.toULong())
            ?: return false to "no gateway head heard yet"

        return try {
            val signed = signer.signTransfer(intent)
            val meshTxId = node.submitSignedTransfer(signed)
            if (meshTxId.isEmpty()) {
                false to "could not encode the signed tx"
            } else {
                true to json(
                    "meshTxId" to meshTxId.joinToString("") { "%02x".format(it) },
                    "txHash" to signed.txHash,
                    "network" to if (intent.network == NetworkId.MAINNET) "mainnet" else "testnet",
                )
            }
        } catch (e: Exception) {
            false to (e.message ?: e.javaClass.simpleName)
        }
    }

    private fun json(vararg pairs: Pair<String, Any?>): JSONObject =
        JSONObject().apply { pairs.forEach { (k, v) -> put(k, v) } }

    companion object {
        val METHODS = setOf(
            "meshQueryBalance", "meshCachedBalance", "meshQueryHistory", "meshCachedHistory",
            "meshSendTransaction",
        )
    }
}
