package com.nimmesh.app.net

import org.json.JSONArray
import org.json.JSONObject

/**
 * Turns raw RPC transaction objects into the rows the page renders.
 *
 * Pure, and separate from [NimiqRpc] on purpose: direction, counterparty and confirmation
 * are decided HERE, and getting any of them wrong shows a user a payment going the wrong
 * way. Keeping it free of IO means the decisions are covered by a JVM test that runs in CI
 * rather than only by a device test nobody runs on a PR.
 *
 * Nothing secret passes through: this is all public chain data.
 */
object TxRows {

    /**
     * @param selfAddress this wallet's address, in any spacing or case.
     * @param rows raw `getTransactionsByAddress` objects, newest first.
     */
    fun normalize(selfAddress: String, rows: List<JSONObject>): JSONArray {
        val self = canonical(selfAddress)
        val out = JSONArray()
        rows.forEach { t ->
            // A transaction TO us is incoming. Compared canonically because the RPC and the
            // display form differ in spacing and case, and a mismatch would silently label
            // every received payment as sent.
            val incoming = canonical(t.optString("to")) == self
            out.put(
                JSONObject()
                    .put("hash", t.optString("hash"))
                    .put("counterparty", if (incoming) t.optString("from") else t.optString("to"))
                    .put("valueLuna", t.optLong("value"))
                    .put("timestamp", t.optDouble("timestamp", 0.0))
                    .put("incoming", incoming)
                    // blockNumber is absent or null until inclusion, so a pending tx must
                    // read as unconfirmed rather than defaulting to confirmed.
                    .put("confirmed", t.optLong("blockNumber", 0L) > 0L),
            )
        }
        return out
    }

    private fun canonical(address: String): String = address.replace(" ", "").uppercase()
}
