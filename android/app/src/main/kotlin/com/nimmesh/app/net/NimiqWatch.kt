package com.nimmesh.app.net

import org.json.JSONObject

/**
 * The nimiq.watch block-explorer REST API, used as a READ fallback when the JSON-RPC node
 * is unreachable (#43).
 *
 * ### Why this and not a second RPC node
 *
 * There is no second public Nimiq JSON-RPC endpoint. Every candidate checked on 2026-08-24
 * (`rpc.nimiq.watch`, `mainnet.nimiq.watch`, `rpc.nimiq.com`, `nimiq.mopsus.com`,
 * `rpc.zeromox.com`, `albatross.nimiq.network`) fails to resolve at all, so an ordered list
 * of RPC nodes would have nothing to fall back TO. The explorer's API is the one independent
 * mainnet source that is actually up, and it was up throughout the outage that prompted this.
 *
 * ### What it can and cannot do
 *
 * Reads only: head height, balance, transaction history. **It cannot broadcast**, because
 * broadcasting needs a real node. So an online send has no fallback and must fail loudly
 * rather than appear to work. The offline mesh send is unaffected: it anchors to a gateway
 * beacon heard over Bluetooth and never touches HTTP.
 *
 * ### Trust
 *
 * This is a second party to trust, and it is worth naming. A dishonest source could
 * understate a balance, hide a transaction, or report a stale head. The head matters most:
 * it anchors `validityStartHeight`, and a pre-dated transaction burns its validity window.
 * The mitigation today is that this is READ-ONLY and never used to sign a broadcast, since
 * `sendTransaction` requires the RPC node and fails without it.
 */
object NimiqWatch {

    private const val BASE = "https://api.nimiq.watch/api/v1"

    /** The head height, from the newest indexed block. */
    fun headHeight(): UInt {
        val blocks = Http.get("$BASE/latest/1")
        val arr = org.json.JSONArray(blocks)
        val head = arr.optJSONObject(0)?.optLong("height")
            ?: throw NimiqRpc.RpcException("nimiq.watch: no height in /latest/1")
        return head.toUInt()
    }

    fun balance(address: String): ULong {
        val account = Http.getJson("$BASE/account/${compact(address)}")
        if (account.optBoolean("error")) {
            throw NimiqRpc.RpcException("nimiq.watch: ${account.optString("statusMessage")}")
        }
        val balance = account.opt("balance") as? Number
            ?: throw NimiqRpc.RpcException("nimiq.watch: no balance for $address")
        return balance.toLong().toULong()
    }

    /**
     * Recent transactions, remapped into the JSON-RPC field names so [TxRows] stays the one
     * place direction, counterparty and confirmation are decided. Two platforms deciding
     * "incoming" separately is two places for it to be wrong.
     */
    fun transactions(address: String, max: Int = 20): List<JSONObject> {
        val raw = org.json.JSONArray(Http.get("$BASE/account-transactions/${compact(address)}/$max"))
        return (0 until raw.length()).mapNotNull { raw.optJSONObject(it) }.map { t ->
            JSONObject()
                .put("hash", t.optString("hash"))
                .put("from", t.optString("sender_address"))
                .put("to", t.optString("receiver_address"))
                .put("value", t.optLong("value"))
                // ⚠ The explorer reports SECONDS; the JSON-RPC node and the page's
                // `new Date(t.timestamp)` both use MILLISECONDS. Miss this and every
                // transaction renders as 1970.
                .put("timestamp", t.optLong("timestamp") * 1000L)
                .put("blockNumber", t.optLong("block_height"))
        }.sortedByDescending { it.optLong("blockNumber") } // newest first, as the RPC returns
    }

    private fun compact(address: String): String = address.replace(" ", "")
}
