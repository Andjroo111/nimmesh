package com.nimmesh.app

import android.webkit.WebView
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * A2's gate: a real WebView, the real injected shim, and the real bridge, proving a page
 * Promise resolves out of the Rust core and back.
 *
 * It deliberately drives `window.nimmesh` rather than calling [Bridge] directly, because
 * the thing that can break is the seam: the shim's JSON encoding, the JavascriptInterface
 * hop, the executor, and the `__nimmeshResolve` callback all have to line up. Calling the
 * Kotlin method in isolation would pass while the page saw nothing.
 */
@RunWith(AndroidJUnit4::class)
class BridgeRoundTripTest {

    private fun withBridgedWebView(block: (WebView, (String) -> String) -> Unit) {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val context = instrumentation.targetContext
        lateinit var webView: WebView
        val bridge = Bridge(context)

        instrumentation.runOnMainSync {
            webView = WebView(context)
            webView.settings.javaScriptEnabled = true
            webView.addJavascriptInterface(bridge, Bridge.CHANNEL)
            bridge.webView = webView
            // about:blank is enough: this test is about the bridge, not the wallet page.
            webView.loadUrl("about:blank")
            webView.evaluateJavascript(BridgeJs.SHIM, null)
        }

        // Evaluate an expression that returns a Promise and hand back what it settled to,
        // shaped as "ok:<json>" or "err:<message>" so a rejection is an assertable value
        // rather than a hang.
        val eval = { expression: String ->
            val answers = ArrayBlockingQueue<String>(1)
            instrumentation.runOnMainSync {
                webView.evaluateJavascript(
                    """
                    (function () {
                      window.__testResult = null;
                      Promise.resolve($expression)
                        .then(function (v) { window.__testResult = 'ok:' + JSON.stringify(v); })
                        .catch(function (e) { window.__testResult = 'err:' + e.message; });
                    })();
                    """.trimIndent(),
                    null,
                )
            }
            var settled: String? = null
            val deadline = System.currentTimeMillis() + 10_000
            while (settled == null && System.currentTimeMillis() < deadline) {
                instrumentation.runOnMainSync {
                    webView.evaluateJavascript("window.__testResult") { raw ->
                        if (raw != null && raw != "null") answers.offer(raw)
                    }
                }
                settled = answers.poll(200, TimeUnit.MILLISECONDS)
            }
            requireNotNull(settled) { "the page's Promise never settled for: $expression" }
        }

        try {
            block(webView, eval)
        } finally {
            instrumentation.runOnMainSync { webView.destroy() }
        }
    }

    @Test
    fun theShimIsInstalledWithEveryMethodTheIosBridgeExposes() = withBridgedWebView { _, eval ->
        val missing = BridgeJs.METHODS.filter { method ->
            !eval("typeof window.nimmesh.$method === 'function'").contains("true")
        }
        assertTrue("window.nimmesh is missing: $missing", missing.isEmpty())
    }

    @Test
    fun versionResolvesWithTheRustCoreVersion() = withBridgedWebView { _, eval ->
        val result = eval("window.nimmesh.version()")
        assertTrue("expected a resolved version, got: $result", result.startsWith("\"ok:"))
        val expected = uniffi.nimmesh_core.coreVersion()
        assertTrue(
            "the page did not get the Rust core version ($expected), got: $result",
            result.contains(expected),
        )
    }

    @Test
    fun meshStatusReportsHonestlyThroughTheCore() = withBridgedWebView { _, eval ->
        val result = eval("window.nimmesh.meshStatus()")
        // No radio until A5, so the only truthful answer is offline with no peers. The
        // point is that these numbers came from the Rust node, not from a Kotlin literal.
        assertTrue("expected an offline mesh with 0 peers, got: $result", result.contains("\\\"offline\\\""))
        assertTrue("expected 0 peers, got: $result", result.contains("\\\"peers\\\":0"))
    }

    @Test
    fun preferencesSurviveTheRoundTrip() = withBridgedWebView { _, eval ->
        eval("window.nimmesh.setLang('de')")
        val result = eval("window.nimmesh.getLang()")
        assertTrue("the language did not come back, got: $result", result.contains("de"))
        eval("window.nimmesh.setLang('')")
    }

    @Test
    fun anUnbuiltMethodRejectsByNameInsteadOfFakingAnAnswer() = withBridgedWebView { _, eval ->
        // The failure this guards against is subtle: resolving an empty success for a
        // method that has not been built renders as data the user reads as fact. A
        // rejection cannot be mistaken for data.
        //
        // This has now moved twice, from walletExists (built in A3) to headHeight (built in
        // A4) to here. Keep moving it rather than deleting it: the day nothing is left to
        // name is the day the bridge is complete, and that should be a deliberate edit
        // rather than a test that quietly stops checking anything.
        val result = eval("window.nimmesh.usdcBalances()")
        assertTrue("expected a rejection, got: $result", result.startsWith("\"err:"))
        assertTrue("the rejection does not say why, got: $result", result.contains("deferred"))
    }

    @Test
    fun theBuiltMethodsAnswerRatherThanRejecting() = withBridgedWebView { _, eval ->
        // The other half of the same contract: everything A1 through A4 claims to have
        // built must actually answer. A method quietly falling through to the "not on
        // Android yet" table would otherwise look like a considered decision.
        listOf("version", "meshStatus", "reachability", "walletExists", "walletStatus", "getLang")
            .forEach { method ->
                val result = eval("window.nimmesh.$method()")
                assertTrue("$method should answer, not reject: $result", result.startsWith("\"ok:"))
            }
    }

    @Test
    fun walletExistsAnswersNowThatA3IsBuilt() = withBridgedWebView { _, eval ->
        val result = eval("window.nimmesh.walletExists()")
        assertTrue("walletExists should answer, not reject: $result", result.startsWith("\"ok:"))
        assertTrue("expected an exists flag, got: $result", result.contains("exists"))
    }

    @Test
    fun anUnknownMethodIsRefused() = withBridgedWebView { _, eval ->
        // window.nimmesh.call() is public, so the page can reach any string. The bridge
        // answers only names it published.
        val result = eval("window.nimmesh.call('drainWallet')")
        assertTrue("expected a refusal, got: $result", result.startsWith("\"err:"))
        assertTrue(result.contains("unknown method"))
    }

    @Test
    fun theWalletPageLoadsFromAssetsOverHttps() {
        // The asset origin matters: on file:// (what iOS runs on) fetch() to the network is
        // blocked outright. Assert the page the app actually ships is reachable and is the
        // wallet, not a 404 body served with a 200.
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val stream = instrumentation.targetContext.assets.open("webui/index.html")
        val html = stream.bufferedReader().use { it.readText() }
        assertTrue("webui/index.html is not in the APK assets", html.length > 10_000)
        assertTrue("the bundled page does not look like the wallet", html.contains("window.nimmesh"))
        assertEquals(
            "https://appassets.androidplatform.net/assets/webui/index.html",
            MainActivity.INDEX_URL,
        )
    }
}
