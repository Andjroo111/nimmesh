package com.nimmesh.app

import android.annotation.SuppressLint
import android.app.Activity
import android.os.Bundle
import android.view.ViewGroup
import android.webkit.JsResult
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.webkit.WebViewAssetLoader

/**
 * The native shell. It owns exactly two things: the WebView that renders `webui/`, and
 * the bridge that connects it to the Rust core. No UI is built in Kotlin, the same rule
 * the iOS host follows.
 */
class MainActivity : Activity() {

    private lateinit var webView: WebView
    private lateinit var bridge: Bridge

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Serve the bundled assets over a real https origin instead of file://. This is
        // not cosmetic: on iOS the app runs on file://, where WKWebView blocks fetch() to
        // the network outright, which is the only reason several read-only chain calls
        // exist as native bridge methods there at all. Here fetch() works, so those stay
        // bridge methods only for parity, not necessity.
        val assetLoader = WebViewAssetLoader.Builder()
            .setDomain(ASSET_DOMAIN)
            .addPathHandler("/assets/", WebViewAssetLoader.AssetsPathHandler(this))
            .build()

        bridge = Bridge(this)
        webView = WebView(this).apply {
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            )
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            // The page is ours and served from assets; nothing loads across origins.
            settings.allowFileAccess = false
            settings.allowContentAccess = false
            settings.mediaPlaybackRequiresUserGesture = false
            // The wallet layer sets its own viewport. Letting WebView apply its wide
            // viewport heuristics on top of that is how a mobile layout ends up zoomed.
            settings.useWideViewPort = false
            settings.loadWithOverviewMode = false
            setBackgroundColor(getColor(R.color.page_background))

            addJavascriptInterface(bridge, Bridge.CHANNEL)

            webViewClient = object : WebViewClient() {
                override fun shouldInterceptRequest(
                    view: WebView,
                    request: WebResourceRequest,
                ): WebResourceResponse? = assetLoader.shouldInterceptRequest(request.url)

                override fun onPageStarted(view: WebView, url: String?, favicon: android.graphics.Bitmap?) {
                    super.onPageStarted(view, url, favicon)
                    // The iOS host injects the shim with WKUserScript at documentStart.
                    // WebView has no documentStart hook, so it is injected here, which
                    // runs before the page's own scripts execute.
                    view.evaluateJavascript(BridgeJs.SHIM, null)
                }
            }

            // Without a WebChromeClient, window.confirm() silently returns false and
            // window.alert() does nothing. On iOS the equivalent gap (no WKUIDelegate)
            // made every confirm-gated action a no-op on device: delete wallet, log out,
            // the mainnet switch. Same bug, same fix, written down so it is not
            // rediscovered a third time.
            webChromeClient = object : WebChromeClient() {
                override fun onJsAlert(
                    view: WebView?, url: String?, message: String?, result: JsResult,
                ): Boolean {
                    android.app.AlertDialog.Builder(this@MainActivity)
                        .setMessage(message)
                        .setPositiveButton(android.R.string.ok) { _, _ -> result.confirm() }
                        .setOnCancelListener { result.cancel() }
                        .show()
                    return true
                }

                override fun onJsConfirm(
                    view: WebView?, url: String?, message: String?, result: JsResult,
                ): Boolean {
                    android.app.AlertDialog.Builder(this@MainActivity)
                        .setMessage(message)
                        .setPositiveButton(android.R.string.ok) { _, _ -> result.confirm() }
                        .setNegativeButton(android.R.string.cancel) { _, _ -> result.cancel() }
                        .setOnCancelListener { result.cancel() }
                        .show()
                    return true
                }
            }
        }
        bridge.webView = webView
        setContentView(webView)
        webView.loadUrl(INDEX_URL)
    }

    override fun onBackPressed() {
        // The wallet is a single page with its own in-page views and sheets. Letting the
        // system back button unwind WebView history would walk out of a sheet in a way
        // the page does not know about.
        if (webView.canGoBack()) webView.goBack() else super.onBackPressed()
    }

    companion object {
        private const val ASSET_DOMAIN = "appassets.androidplatform.net"
        const val INDEX_URL = "https://$ASSET_DOMAIN/assets/webui/index.html"
    }
}
