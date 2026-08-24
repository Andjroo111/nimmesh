package com.nimmesh.app

import android.app.Activity
import android.content.Intent
import android.hardware.biometrics.BiometricPrompt
import android.os.CancellationSignal
import android.os.Handler
import android.os.Looper
import org.json.JSONObject
import java.lang.ref.WeakReference

/**
 * The bridge methods that need an Activity rather than a Context (A4): the QR scanner, the
 * share sheet, and the "unlock your backup" prompt.
 *
 * The Activity is held WEAKLY, the same discipline as the iOS bridge's
 * `topmostViewController()`: this object outlives any single screen, and a strong reference
 * here would pin a destroyed Activity and its whole view tree.
 *
 * Each of these answers asynchronously, so instead of returning a value they take a
 * `resolve` callback and the caller does not resolve the page's Promise itself.
 */
class NativeUiBridge {

    private var activityRef: WeakReference<Activity> = WeakReference(null)
    private var pendingScan: ((Boolean, Any) -> Unit)? = null

    fun attach(activity: Activity) {
        activityRef = WeakReference(activity)
    }

    fun handles(method: String): Boolean = method in METHODS

    /** @return false if this method is not handled, in which case nothing was resolved. */
    fun dispatch(method: String, args: JSONObject?, resolve: (Boolean, Any) -> Unit): Boolean {
        val activity = activityRef.get()
        if (activity == null || activity.isFinishing) {
            resolve(false, "no screen to present from")
            return true
        }
        when (method) {
            "scanQr" -> scanQr(activity, resolve)
            "share" -> share(activity, args, resolve)
            "authenticate" -> authenticate(activity, resolve)
            else -> return false
        }
        return true
    }

    private fun scanQr(activity: Activity, resolve: (Boolean, Any) -> Unit) {
        // A second scan while one is open would strand the first Promise forever.
        pendingScan?.invoke(false, "cancelled")
        pendingScan = resolve
        activity.runOnUiThread {
            activity.startActivityForResult(
                Intent(activity, QrScannerActivity::class.java),
                REQUEST_SCAN_QR,
            )
        }
    }

    /**
     * Routed from `MainActivity.onActivityResult`. A cancel REJECTS, matching iOS, because
     * the page treats a rejected scan as a quiet no-op and resolving an empty string would
     * look like a scan that read nothing.
     */
    fun onScanResult(resultCode: Int, data: Intent?) {
        val resolve = pendingScan ?: return
        pendingScan = null
        val text = data?.getStringExtra(QrScannerActivity.EXTRA_TEXT)
        if (resultCode == Activity.RESULT_OK && !text.isNullOrEmpty()) {
            resolve(true, JSONObject().put("text", text))
        } else {
            resolve(false, "cancelled")
        }
    }

    private fun share(activity: Activity, args: JSONObject?, resolve: (Boolean, Any) -> Unit) {
        val text = args?.optString("text").orEmpty()
        val url = args?.optString("url").orEmpty()
        val body = listOf(text, url).filter { it.isNotEmpty() }.joinToString(" ")
        activity.runOnUiThread {
            try {
                activity.startActivity(
                    Intent.createChooser(
                        Intent(Intent.ACTION_SEND).apply {
                            type = "text/plain"
                            putExtra(Intent.EXTRA_TEXT, body)
                        },
                        null,
                    ),
                )
                // Android's chooser reports nothing back without a broadcast receiver, so
                // this says the sheet OPENED, not that anything was sent. iOS can report
                // completion; claiming it here would be a guess.
                resolve(true, JSONObject().put("shared", true))
            } catch (e: Exception) {
                resolve(true, JSONObject().put("shared", false))
            }
        }
    }

    /**
     * The "unlock your backup" gate before recovery material is shown.
     *
     * The framework `BiometricPrompt` (API 28+, device-credential fallback from 30), not
     * androidx.biometric: that dependency wants a newer compileSdk than the platform ships
     * today, and minSdk here is 31 so the framework class is always present.
     *
     * A device with no biometric AND no screen lock has nothing to unlock WITH, so it
     * passes through, exactly like the iOS `canEvaluatePolicy` path. Such a device is
     * unprotected either way, and refusing would lock the owner out of their own words.
     */
    private fun authenticate(activity: Activity, resolve: (Boolean, Any) -> Unit) {
        activity.runOnUiThread {
            try {
                val prompt = BiometricPrompt.Builder(activity)
                    .setTitle(activity.getString(R.string.unlock_title))
                    .setDescription(activity.getString(R.string.unlock_description))
                    .setAllowedAuthenticators(
                        BiometricManagerCompat.BIOMETRIC_WEAK or BiometricManagerCompat.DEVICE_CREDENTIAL,
                    )
                    .build()
                prompt.authenticate(
                    CancellationSignal(),
                    { r -> Handler(Looper.getMainLooper()).post(r) },
                    object : BiometricPrompt.AuthenticationCallback() {
                        override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult?) {
                            resolve(true, JSONObject().put("ok", true))
                        }

                        override fun onAuthenticationError(code: Int, message: CharSequence?) {
                            // NO_BIOMETRICS / HW unavailable with no device credential set:
                            // there is nothing to unlock with, so pass through.
                            val nothingToUnlockWith = code == BiometricPrompt.BIOMETRIC_ERROR_NO_BIOMETRICS ||
                                code == BiometricPrompt.BIOMETRIC_ERROR_HW_NOT_PRESENT ||
                                code == BiometricPrompt.BIOMETRIC_ERROR_HW_UNAVAILABLE
                            if (nothingToUnlockWith) {
                                resolve(true, JSONObject().put("ok", true).put("method", "none"))
                            } else {
                                resolve(true, JSONObject().put("ok", false))
                            }
                        }

                        override fun onAuthenticationFailed() {
                            // A rejected fingerprint, not a finished prompt. The system lets
                            // the user try again, so this must NOT resolve.
                        }
                    },
                )
            } catch (e: Exception) {
                resolve(true, JSONObject().put("ok", true).put("method", "none"))
            }
        }
    }

    /** The authenticator constants, named rather than repeated as magic numbers. */
    private object BiometricManagerCompat {
        const val BIOMETRIC_WEAK = 0x00FF
        const val DEVICE_CREDENTIAL = 1 shl 15
    }

    companion object {
        const val REQUEST_SCAN_QR = 1001
        val METHODS = setOf("scanQr", "share", "authenticate")
    }
}
