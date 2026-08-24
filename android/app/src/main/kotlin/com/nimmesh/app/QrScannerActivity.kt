package com.nimmesh.app

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.TextView
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.qrcode.QRCodeReader
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The QR scanner behind the bridge's `scanQr`, the counterpart of iOS's
 * `QrScannerViewController`.
 *
 * ZXing decodes, CameraX supplies the frames. Deliberately NOT ML Kit: its barcode scanner
 * pulls in Google Play Services, and this app ships as a direct APK precisely so it does
 * not depend on anyone's store being installed.
 *
 * Like the iOS one, it always presents: a denied camera permission finishes with a result
 * the page treats as a cancel, rather than silently doing nothing. **The web layer owns
 * parsing**; this only captures a string.
 */
class QrScannerActivity : Activity() {

    private val analysisExecutor = Executors.newSingleThreadExecutor()
    private val delivered = AtomicBoolean(false)
    private lateinit var previewView: PreviewView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        previewView = PreviewView(this)
        val root = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(
                previewView,
                FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT,
                ),
            )
            addView(
                TextView(this@QrScannerActivity).apply {
                    text = getString(R.string.scan_hint)
                    setTextColor(Color.WHITE)
                    textSize = 16f
                    setPadding(48, 48, 48, 96)
                },
                FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                    Gravity.BOTTOM,
                ),
            )
        }
        setContentView(root)

        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            startCamera()
        } else {
            requestPermissions(arrayOf(Manifest.permission.CAMERA), REQUEST_CAMERA)
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQUEST_CAMERA) return
        if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) startCamera() else finishCancelled()
    }

    private fun startCamera() {
        val future = ProcessCameraProvider.getInstance(this)
        future.addListener({
            try {
                val provider = future.get()
                val preview = Preview.Builder().build()
                    .also { it.surfaceProvider = previewView.surfaceProvider }
                val analysis = ImageAnalysis.Builder()
                    .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                    .build()
                    .also { it.setAnalyzer(analysisExecutor, ::analyze) }

                provider.unbindAll()
                // CameraX binds to a LifecycleOwner. This is a plain Activity, so it binds
                // to the process lifecycle and unbinds explicitly in onDestroy. Adding
                // androidx.lifecycle just to own a camera is not worth the dependency.
                provider.bindToLifecycle(
                    ProcessLifecycle,
                    CameraSelector.DEFAULT_BACK_CAMERA,
                    preview,
                    analysis,
                )
            } catch (e: Exception) {
                Log.e(TAG, "could not start the camera", e)
                finishCancelled()
            }
        }, mainExecutor)
    }

    private fun analyze(image: ImageProxy) {
        try {
            if (delivered.get()) return
            val plane = image.planes.firstOrNull() ?: return
            val bytes = ByteArray(plane.buffer.remaining()).also { plane.buffer.get(it) }
            val source = PlanarYUVLuminanceSource(
                bytes, plane.rowStride, image.height, 0, 0, image.width, image.height, false,
            )
            val text = try {
                QRCodeReader().decode(
                    BinaryBitmap(HybridBinarizer(source)),
                    mapOf(DecodeHintType.TRY_HARDER to true),
                ).text
            } catch (e: Exception) {
                null // no QR in this frame, which is the common case
            }
            if (!text.isNullOrEmpty() && delivered.compareAndSet(false, true)) {
                runOnUiThread {
                    setResult(RESULT_OK, Intent().putExtra(EXTRA_TEXT, text))
                    finish()
                }
            }
        } finally {
            image.close()
        }
    }

    private fun finishCancelled() {
        if (delivered.compareAndSet(false, true)) {
            setResult(RESULT_CANCELED)
            finish()
        }
    }

    override fun onDestroy() {
        analysisExecutor.shutdown()
        try {
            ProcessCameraProvider.getInstance(this).get().unbindAll()
        } catch (e: Exception) {
            Log.w(TAG, "camera unbind on destroy", e)
        }
        super.onDestroy()
    }

    companion object {
        private const val TAG = "nimmesh.qr"
        private const val REQUEST_CAMERA = 1
        const val EXTRA_TEXT = "text"
    }
}
