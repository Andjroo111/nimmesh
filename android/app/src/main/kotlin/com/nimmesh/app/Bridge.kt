package com.nimmesh.app

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.JavascriptInterface
import android.webkit.WebView
import com.nimmesh.app.wallet.Wallet
import org.json.JSONObject
import java.util.concurrent.Executors

/**
 * The JS to Kotlin to Rust bridge, the Android twin of `WebHostView.Bridge`.
 *
 * The page posts `{id, method, args}` as a JSON string and gets its Promise resolved by
 * id. Every answer goes back through `window.__nimmeshResolve`, exactly as on iOS.
 *
 * ### Two things this must never do
 *
 * `@JavascriptInterface` methods arrive on WebView's internal JavaBridge thread, not the
 * UI thread, and blocking that thread stalls the page. So work is handed to an executor
 * and the resolve is posted back to the UI thread, which is where `evaluateJavascript`
 * has to run.
 *
 * Any method the page can call but this platform has not built yet REJECTS with a named
 * reason rather than resolving something empty. The web layer wraps its bridge calls in
 * `try/catch`, so a rejection reads as "this feature is absent here", while a fake empty
 * success reads as "you have no wallet" or "you have no peers", which is a lie the UI
 * would render as fact.
 */
class Bridge(context: Context) {

    private val appContext = context.applicationContext
    private val prefs = Prefs(appContext)
    private val wallet = Wallet(appContext)

    // Family seams. Splitting the dispatch by family is how the iOS bridge adds methods
    // without growing WebHostView.swift past the repo's 800-line guard; the same reason
    // applies here.
    private val walletBridge = WalletBridge(wallet, prefs)
    private val networkBridge = NetworkBridge(wallet, prefs)
    val nativeUi = NativeUiBridge()
    private val executor = Executors.newSingleThreadExecutor { r ->
        Thread(r, "nimmesh-bridge").apply { isDaemon = true }
    }

    // NOT webView.post(). View.post() before a view is attached to a window only QUEUES the
    // runnable, and it runs on attach or never. Every answer would sit unposted and every
    // page Promise would hang forever, silently. A main-looper Handler does not care whether
    // the view is attached.
    private val main = Handler(Looper.getMainLooper())

    var webView: WebView? = null

    /** Entry point for the page. Must return fast; everything real happens on the executor. */
    @JavascriptInterface
    fun postMessage(message: String) {
        val envelope = try {
            JSONObject(message)
        } catch (e: Exception) {
            Log.w(TAG, "unparseable bridge message", e)
            return
        }
        val id = envelope.optInt("id", -1)
        val method = envelope.optString("method")
        if (id < 0 || method.isEmpty()) return
        val args = envelope.optJSONObject("args")

        // The native-UI methods (camera, share sheet, unlock prompt) answer only when the
        // user finishes with them, so they resolve the page's Promise themselves rather
        // than returning a value through the synchronous path below.
        if (nativeUi.handles(method)) {
            if (nativeUi.dispatch(method, args) { ok, payload -> resolve(id, ok, payload) }) return
        }

        executor.execute {
            val (ok, payload) = try {
                dispatch(method, args)
            } catch (e: Throwable) {
                // A throw here would otherwise leave the page's Promise pending forever,
                // and a hung Promise is invisible: the UI just never updates.
                Log.e(TAG, "bridge method '$method' threw", e)
                false to (e.message ?: e.javaClass.simpleName)
            }
            resolve(id, ok, payload)
        }
    }

    private fun dispatch(method: String, args: JSONObject?): Pair<Boolean, Any> {
        if (method !in BridgeJs.METHODS) return false to "unknown method: $method"
        val node = MeshHost.start(appContext)

        if (walletBridge.handles(method)) return walletBridge.dispatch(method, args)
        if (networkBridge.handles(method)) return networkBridge.dispatch(method, args)
        if (method in MeshWalletBridge.METHODS) {
            return MeshWalletBridge(node, wallet).dispatch(method, args)
        }

        return when (method) {
            "version" -> true to json("core" to uniffi.nimmesh_core.coreVersion())

            "meshStatus" -> {
                val peers = node.peerCount().toInt()
                val radio = MeshHost.radio
                // `state` and `peers` are the two fields iOS returns and the page reads.
                // The rest are ADDITIVE, and exist so the page can eventually say WHY a mesh
                // is empty rather than only that it is. Extra fields are ignored by anything
                // that does not know them, so this stays parity-safe: the method list is
                // what BridgeMethodParityTest compares, not the payload.
                //
                // No copy is written for them here. The wording lives in webui/,
                // across five languages.
                true to json(
                    "state" to if (peers > 0) "meshed" else "offline",
                    "peers" to peers,
                    "permitted" to (radio?.hasPermissions() ?: false),
                    "bluetoothOn" to (radio?.bluetoothEnabled() ?: false),
                    // False on a real slice of Android hardware. Such a phone still relays
                    // and still pays as a central; it just cannot be DISCOVERED.
                    "canAdvertise" to (radio?.canAdvertise() ?: false),
                    "relayingInBackground" to MeshService.isRunning,
                )
            }

            "meshDebug" -> {
                // The live radio state, so the Network screen can show what Bluetooth is
                // actually doing rather than only what the mesh concluded.
                val radio = MeshHost.radio?.debugSummary() ?: "radio:none"
                true to json("debug" to "$radio node-peers:${node.peerCount()}")
            }

            // Emit a head beacon to connected peers. A no-op with no peers, which is the
            // only state this build can be in.
            "keepalive" -> { node.pollBeacon(); true to json("ok" to true) }

            "reachability" -> {
                // Deliberately NOT node.reachability(): once the phone is itself a gateway
                // that always reports Online whatever the actual connectivity. Online here
                // means a real RPC round trip SUCCEEDED in the last 30 seconds.
                val r = when {
                    com.nimmesh.app.net.NimiqRpc.isLive() -> "online"
                    node.peerCount() > 0u -> "meshed"
                    else -> "offline"
                }
                true to json("reachability" to r)
            }

            "meshSendInfo" -> {
                val head = node.cachedHeadHeight()
                true to json(
                    "headHeard" to (head != null),
                    "head" to (head?.toInt() ?: 0),
                    "peers" to node.peerCount().toInt(),
                    "network" to "mainnet",
                )
            }

            "meshPaymentStatus" -> {
                val txId = hexToBytes(args?.optString("meshTxId").orEmpty())
                if (txId.size != 32) return false to "bad meshTxId"
                val status = when (node.paymentStatus(txId)) {
                    uniffi.nimmesh_core.PaymentStatus.PENDING -> "pending"
                    uniffi.nimmesh_core.PaymentStatus.SETTLED -> "settled"
                    uniffi.nimmesh_core.PaymentStatus.FAILED -> "failed"
                }
                true to json("status" to status)
            }

            "mainnetSwapArmed" -> true to json(
                "armed" to uniffi.nimmesh_core.mainnetSwapArmed(),
                "reason" to uniffi.nimmesh_core.mainnetSwapReason(),
            )

            "backupUrgency" -> {
                val state = uniffi.nimmesh_core.BackupState(
                    backedUp = args?.optBoolean("backedUp") ?: false,
                    balanceLuna = (args?.optLong("balanceLuna") ?: 0L).toULong(),
                    daysSinceFirstFunds = (args?.optInt("daysSinceFirstFunds") ?: 0).toUInt(),
                )
                val level = when (uniffi.nimmesh_core.backupUrgency(state)) {
                    uniffi.nimmesh_core.BackupUrgency.NONE -> "none"
                    uniffi.nimmesh_core.BackupUrgency.GENTLE -> "gentle"
                    uniffi.nimmesh_core.BackupUrgency.IMPORTANT -> "important"
                    uniffi.nimmesh_core.BackupUrgency.CRITICAL -> "critical"
                }
                true to json("urgency" to level)
            }

            "sendChat" -> {
                val ok = node.sendChat(
                    nickname = args?.optString("nickname").orEmpty(),
                    text = args?.optString("text").orEmpty(),
                    timestampMs = System.currentTimeMillis().toULong(),
                )
                true to json("ok" to ok)
            }

            "chatMessages" -> {
                val rows = node.chatMessages().map { m ->
                    json(
                        "nickname" to m.nickname,
                        "text" to m.text,
                        "timestamp" to m.timestampMs.toDouble(),
                        "mine" to m.mine,
                    )
                }
                true to json("messages" to org.json.JSONArray(rows))
            }

            // Preferences. These live natively rather than in localStorage because a
            // web-layer copy silently reset across relaunches on device, which made both
            // phones swap initiators so they could never match (field bug 2026-07-19).
            "getLang" -> true to json("lang" to prefs.getString(Prefs.LANG))
            "setLang" -> {
                prefs.setString(Prefs.LANG, args?.optString("lang").orEmpty())
                true to json("ok" to true)
            }
            "getBackedUp" -> true to json("backedUp" to prefs.getBool(Prefs.BACKED_UP))
            "setBackedUp" -> {
                prefs.setBool(Prefs.BACKED_UP, args?.optBoolean("backedUp") ?: false)
                true to json("ok" to true)
            }
            "getRespondRole" -> true to json("on" to prefs.getBool(Prefs.RESPOND_ROLE))
            "setRespondRole" -> {
                prefs.setBool(Prefs.RESPOND_ROLE, args?.optBoolean("on") ?: false)
                true to json("ok" to true)
            }

            else -> false to notYetOnAndroid(method)
        }
    }

    /**
     * The named rejection for a method the page knows about and this platform has not
     * built. Naming the slice makes a logcat line or a screenshot self-explanatory, and
     * keeps "absent" clearly distinct from "failed".
     */
    private fun notYetOnAndroid(method: String): String {
        val slice = when (method) {
            in USDC_METHODS -> "deferred: USDC rides the swap accounts, out of the Android v1 scope"
            in SWAP_METHODS -> "deferred: swap is out of the Android v1 scope"
            in BITCHAT_METHODS -> "deferred: Bitchat interop is out of the Android v1 scope"
            in CASHLINK_METHODS -> "deferred: cashlinks are out of the Android v1 scope"
            else -> "not implemented"
        }
        return "$method is not on Android yet: $slice"
    }

    private fun resolve(id: Int, ok: Boolean, payload: Any) {
        val encoded = when (payload) {
            is JSONObject, is org.json.JSONArray -> payload.toString()
            is String -> JSONObject.quote(payload)
            else -> JSONObject.quote(payload.toString())
        }
        val view = webView ?: return
        main.post {
            view.evaluateJavascript("window.__nimmeshResolve($id, $ok, $encoded);", null)
        }
    }

    private fun json(vararg pairs: Pair<String, Any?>): JSONObject =
        JSONObject().apply { pairs.forEach { (k, v) -> put(k, v) } }

    private fun hexToBytes(hex: String): ByteArray {
        if (hex.length % 2 != 0) return ByteArray(0)
        val out = ByteArray(hex.length / 2)
        for (i in out.indices) {
            val b = hex.substring(i * 2, i * 2 + 2).toIntOrNull(16) ?: return ByteArray(0)
            out[i] = b.toByte()
        }
        return out
    }

    companion object {
        private const val TAG = "nimmesh.bridge"

        /** The name the shim posts to. Matches `Bridge.channel` on iOS. */
        const val CHANNEL = "__nimmeshNative"

        private val USDC_METHODS = setOf("usdcBalances", "usdcHistory", "sendUsdc")
        private val SWAP_METHODS = setOf(
            "swapMeshStart", "swapMeshStatus", "swapMeshStop", "swapMeshRefund",
            "swapEvmAddresses",
        )
        private val BITCHAT_METHODS = setOf("bitchatStatus", "bitchatSetEnabled")
        private val CASHLINK_METHODS = setOf(
            "cashlinkCreate", "cashlinkList", "cashlinkStatus", "cashlinkPeek", "cashlinkClaim",
        )
    }
}
