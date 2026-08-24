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

    /**
     * The head height a fresh transaction anchors its validity window to.
     *
     * Falls back to the block explorer when the node is unreachable, so a balance and a
     * history still render during an outage. See [NimiqWatch] for why the fallback is an
     * explorer rather than a second RPC node.
     */
    fun headHeight(): UInt = withReadFallback({
        val n = call("getBlockNumber", JSONArray()) as? Number
            ?: throw RpcException("getBlockNumber: unexpected result shape")
        n.toLong().toUInt()
    }, NimiqWatch::headHeight)

    /**
     * Broadcast a raw signed transaction (hex). Returns the tx hash on accept.
     *
     * ⚠ **No fallback, deliberately.** Broadcasting needs a real node, and the block
     * explorer cannot do it. When the node is down an online send is genuinely impossible,
     * so this fails loudly with a message that says what still works, rather than appearing
     * to succeed. The offline mesh send is unaffected: it anchors to a gateway beacon heard
     * over Bluetooth and never touches HTTP.
     */
    fun sendRawTransaction(rawHex: String): String = try {
        call("sendRawTransaction", JSONArray().put(rawHex)) as? String
            ?: throw RpcException("sendRawTransaction: unexpected result shape")
    } catch (e: Exception) {
        if (e is RpcException && e.message?.contains("unexpected result shape") == true) throw e
        throw RpcException(
            "the Nimiq node is unreachable, so an online send is not possible right now. " +
                "An offline mesh send still works if a peer is nearby. (${e.message})",
        )
    }

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
        return withReadFallback({
            val account = call("getAccountByAddress", JSONArray().put(compact(address))) as? JSONObject
                ?: throw RpcException("getAccountByAddress: unexpected result shape")
            val b = account.opt("balance") as? Number
                ?: throw RpcException("getAccountByAddress: no balance in result")
            b.toLong().toULong()
        }) { NimiqWatch.balance(address) }
    }

    /**
     * Recent transactions, newest first. Throws on failure for the same reason as
     * [balance]: offline is not "no transactions", and caching an empty list would make it
     * permanent.
     */
    fun transactions(address: String, max: Int = 20): List<JSONObject> {
        return withReadFallback({
            val params = JSONArray().put(compact(address)).put(max).put(JSONObject.NULL)
            val arr = call("getTransactionsByAddress", params) as? JSONArray
                ?: throw RpcException("getTransactionsByAddress: unexpected result shape")
            with(Http) { arr.toObjectList() }
        }) { NimiqWatch.transactions(address, max) }
    }

    /**
     * Which source last answered a read. Surfaced through the bridge so the page can say
     * where a number came from instead of presenting an explorer figure as if a node had
     * confirmed it.
     */
    @Volatile
    var lastReadSource: String = "node"
        private set

    /**
     * Try the node, fall back to the explorer.
     *
     * ⚠ Only [lastSuccessAtMs] tracks the NODE, and only the node can broadcast, so
     * `reachability` still reports offline when the explorer alone is answering. That is the
     * honest reading: "online" is supposed to mean a send will go out, and with no node it
     * will not.
     */
    private fun <T> withReadFallback(primary: () -> T, fallback: () -> T): T = try {
        val value = primary()
        lastReadSource = "node"
        value
    } catch (nodeFailure: Exception) {
        try {
            val value = fallback()
            lastReadSource = "explorer"
            value
        } catch (fallbackFailure: Exception) {
            // Report the NODE's failure: it is the primary, and its message is the one that
            // explains why a send is unavailable too.
            throw RpcException(
                "${nodeFailure.message} (explorer fallback also failed: ${fallbackFailure.message})",
            )
        }
    }

    /** Nimiq addresses are displayed in spaced groups; the RPC wants them unspaced. */
    private fun compact(address: String): String = address.replace(" ", "")
}
