package com.nimmesh.app

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Intent
import android.util.Log
import android.os.Build
import android.os.Bundle
import android.view.ViewGroup
import android.webkit.JsResult
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.webkit.WebViewAssetLoader
import com.nimmesh.app.wallet.Wallet

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
        bridge.nativeUi.attach(this)

        // Bring the mesh up. The radio no-ops until the Bluetooth permissions are granted,
        // which is asked for below, so this is safe on a first launch.
        MeshHost.start(this)
        requestBluetoothPermissionsIfNeeded()

        // Prove the Kotlin signer interoperates with the Rust verifier (BouncyCastle
        // Ed25519 against ed25519-dalek) on THIS device, once per launch, the same check
        // the iOS host logs at startup. It signs nothing that leaves the device. With no
        // wallet yet it reports false, which is the honest answer rather than a skip.
        Thread {
            val wallet = Wallet(this)
            val address = wallet.address() ?: "none"
            android.util.Log.i(TAG, "wallet self-test: address=$address signedOk=${wallet.selfTest()}")
        }.start()

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

    override fun onResume() {
        super.onResume()
        // Permissions can be revoked from Settings while the app is backgrounded, so this is
        // rechecked rather than assumed from launch. If they are gone the relay must stop:
        // a foreground-service notification claiming to carry payments, over a radio that
        // cannot start, is a lie the user is looking at.
        val radio = MeshHost.radio
        if (radio != null && !radio.hasPermissions()) {
            MeshService.stop(this)
            return
        }
        // ⚠ Gated on there being a WALLET, not just a working radio.
        //
        // Without that check this fires on the very first resume, and the system settings
        // screen opens ON TOP OF ONBOARDING: a brand-new user's first sight of the app is a
        // battery menu they have no reason to understand. Caught by driving the real app,
        // not by a test, because the app was technically working the whole time.
        //
        // Someone with a wallet has something worth relaying and has already seen what the
        // app is for.
        if (radio != null && radio.hasPermissions() && radio.bluetoothEnabled() &&
            com.nimmesh.app.wallet.Wallet(this).hasWallet()
        ) {
            offerBatteryExemptionOnce()
        }
    }

    /**
     * Asked at launch rather than at first use, because the mesh is the point of the app:
     * a wallet that quietly is not relaying is worse than one prompt. Declined is a
     * perfectly workable state, the app just runs online-only and says so.
     */
    private fun requestBluetoothPermissionsIfNeeded() {
        val missing = com.nimmesh.app.ble.BleMeshRadio.REQUIRED_PERMISSIONS
            .filter { checkSelfPermission(it) != android.content.pm.PackageManager.PERMISSION_GRANTED }
        if (missing.isEmpty()) onMeshReady()
        else requestPermissions(missing.toTypedArray(), REQUEST_BLUETOOTH)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQUEST_BLUETOOTH) return
        // The radio was built before the user was asked, so its first startAdvertising and
        // startScanning were no-ops. Nothing else would ever call them again.
        if (grantResults.isNotEmpty() && grantResults.all { it == android.content.pm.PackageManager.PERMISSION_GRANTED }) {
            onMeshReady()
        }
    }

    /**
     * The radio has what it needs, so bring it up and start relaying in the background.
     *
     * The notification permission is asked for SEPARATELY and afterwards, because the
     * service runs either way: without it the relay still works and only the status
     * notification is hidden. Bundling it with the Bluetooth prompt would make a refusal
     * look like it disabled the mesh.
     */
    private fun onMeshReady() {
        MeshHost.onPermissionsGranted()
        MeshService.start(this)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) !=
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), REQUEST_NOTIFICATIONS)
        }
    }

    /**
     * Offer the battery-optimisation exemption, once, and only when it would change
     * something.
     *
     * `ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS` opens the system list rather than
     * `ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`, which needs a permission Google
     * restricts and which several OEM builds simply refuse. One extra tap for the user, no
     * restricted permission, and it works everywhere.
     *
     * ⚠ Even exempt, several vendors kill background work anyway. This improves the odds; it
     * does not guarantee anything, and the docs say so rather than implying otherwise.
     */
    private fun offerBatteryExemptionOnce() {
        val prefs = Prefs(this)
        if (prefs.getBool(Prefs.BATTERY_PROMPT_SHOWN)) return
        val power = getSystemService(android.os.PowerManager::class.java) ?: return
        if (power.isIgnoringBatteryOptimizations(packageName)) return
        prefs.setBool(Prefs.BATTERY_PROMPT_SHOWN, true)
        try {
            startActivity(Intent(android.provider.Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
        } catch (e: Exception) {
            Log.w(TAG, "no battery optimisation settings screen on this device", e)
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: android.content.Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == NativeUiBridge.REQUEST_SCAN_QR) bridge.nativeUi.onScanResult(resultCode, data)
    }

    // Deprecated in favour of the predictive-back callback, which is opt-in via
    // android:enableOnBackInvokedCallback and not enabled here, so this still fires.
    @Suppress("DEPRECATION", "OVERRIDE_DEPRECATION")
    override fun onBackPressed() {
        // The wallet is a single page with its own in-page views and sheets. Letting the
        // system back button unwind WebView history would walk out of a sheet in a way
        // the page does not know about.
        if (webView.canGoBack()) webView.goBack() else super.onBackPressed()
    }

    companion object {
        private const val TAG = "nimmesh.app"
        private const val REQUEST_BLUETOOTH = 2
        private const val REQUEST_NOTIFICATIONS = 3
        private const val ASSET_DOMAIN = "appassets.androidplatform.net"
        const val INDEX_URL = "https://$ASSET_DOMAIN/assets/webui/index.html"
    }
}
