package com.nimmesh.app.net

import org.json.JSONArray
import org.json.JSONObject
import uniffi.nimmesh_core.NetworkId
import java.io.IOException

/**
 * A minimal Nimiq JSON-RPC client, ported from `TestnetRpc.swift`.
 *
 * All cryptography stays in the Rust core. This only does network IO: fetch the head for
 * `validityStartHeight`, broadcast a signed blob, read a balance and a history.
 *
 * **MAINNET ONLY**, like iOS. The network toggle was removed deliberately: the owner is the
 * mainnet gate (`docs/MAINNET-GATING.md`). The app still never sends on its own; a send is
 * always a deliberate user action.
 */
object NimiqRpc {

    val network: NetworkId = NetworkId.MAINNET

    private const val RPC_URL = "https://rpc.nimiqwatch.com"

    class RpcException(message: String) : IOException(message)

    /**
     * When the last round trip succeeded. This is the app's live-internet signal, and it
     * cannot come from the mesh node: once the phone is itself a gateway, the node's own
     * reachability always reports Online whatever the actual connectivity.
     */
    @Volatile
    var lastSuccessAtMs: Long = 0
        private set

    fun isLive(withinMs: Long = 30_000): Boolean =
        lastSuccessAtMs != 0L && System.currentTimeMillis() - lastSuccessAtMs < withinMs

    /**
     * One JSON-RPC 2.0 call. Albatross wraps the payload under `result.data`; a bare
     * `result` is tolerated. The node returns `error` as a string OR an object, so both
     * shapes throw rather than one of them slipping through as a successful null.
     */
    private fun call(method: String, params: JSONArray): Any? {
        val envelope = JSONObject()
            .put("jsonrpc", "2.0")
            .put("method", method)
            .put("params", params)
            .put("id", 1)
        val json = Http.postJson(RPC_URL, envelope)

        val error = json.opt("error")
        if (error != null && error != JSONObject.NULL) throw RpcException("$method: $error")

        lastSuccessAtMs = System.currentTimeMillis()

        val result = json.opt("result")
        if (result is JSONObject) {
            val data = result.opt("data")
            return if (data == JSONObject.NULL) null else data
        }
        return if (result == JSONObject.NULL) null else result
    }

    /** The head height a fresh transaction anchors its validity window to. */
    fun headHeight(): UInt {
        val n = call("getBlockNumber", JSONArray()) as? Number
            ?: throw RpcException("getBlockNumber: unexpected result shape")
        return n.toLong().toUInt()
    }

    /** Broadcast a raw signed transaction (hex). Returns the tx hash on accept. */
    fun sendRawTransaction(rawHex: String): String =
        call("sendRawTransaction", JSONArray().put(rawHex)) as? String
            ?: throw RpcException("sendRawTransaction: unexpected result shape")

    /**
     * An account's balance in luna.
     *
     * **Throws on any transport or RPC failure.** Offline has to be distinguishable from
     * "0 NIM", or the UI renders an empty wallet every time the network is down. That was a
     * real field bug on iOS during a Bluetooth-only test: the balance read returned 0 and
     * the wallet looked drained. A genuinely unfunded account still reads 0 from a
     * SUCCESSFUL call, which is a different thing entirely.
     */
    fun balance(address: String): ULong {
        val account = call("getAccountByAddress", JSONArray().put(compact(address))) as? JSONObject
            ?: throw RpcException("getAccountByAddress: unexpected result shape")
        val b = account.opt("balance") as? Number
            ?: throw RpcException("getAccountByAddress: no balance in result")
        return b.toLong().toULong()
    }

    /**
     * Recent transactions, newest first. Throws on failure for the same reason as
     * [balance]: offline is not "no transactions", and caching an empty list would make it
     * permanent.
     */
    fun transactions(address: String, max: Int = 20): List<JSONObject> {
        val params = JSONArray().put(compact(address)).put(max).put(JSONObject.NULL)
        val arr = call("getTransactionsByAddress", params) as? JSONArray
            ?: throw RpcException("getTransactionsByAddress: unexpected result shape")
        return with(Http) { arr.toObjectList() }
    }

    /** Nimiq addresses are displayed in spaced groups; the RPC wants them unspaced. */
    private fun compact(address: String): String = address.replace(" ", "")
}
