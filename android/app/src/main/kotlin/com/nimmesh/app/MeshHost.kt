package com.nimmesh.app

import android.util.Log
import uniffi.nimmesh_core.BleRadio
import uniffi.nimmesh_core.MeshNode

/**
 * Owns the single app-lifetime [MeshNode] and the radio it drives.
 *
 * The node is app-lifetime on purpose, matching the iOS shim: it holds the radio
 * strongly, and the accepted one-instance cycle buys callbacks that actually land.
 * `shutdown()` breaks it if a teardown is ever needed.
 */
object MeshHost {

    /**
     * A2 placeholder. It satisfies the [BleRadio] contract without touching a radio, so
     * the Rust core can be constructed, driven, and read while the real Kotlin radio is
     * still A5 work.
     *
     * It is named to be impossible to mistake for the real thing, and it logs, so a
     * build that reached a device with this still installed says so in logcat rather
     * than quietly reporting an empty mesh. `meshStatus` will honestly read 0 peers,
     * because there are none.
     */
    private class UnimplementedRadio : BleRadio {
        override fun startAdvertising() { Log.w(TAG, "startAdvertising: no radio yet (A5)") }
        override fun startScanning() { Log.w(TAG, "startScanning: no radio yet (A5)") }
        override fun send(peerId: String, bytes: ByteArray) {
            Log.w(TAG, "send(${bytes.size} B to $peerId) dropped: no radio yet (A5)")
        }
        override fun disconnect(peerId: String) = Unit
        override fun stop() = Unit
    }

    private const val TAG = "nimmesh.radio"

    /** True while the mesh is running on the placeholder radio rather than real Bluetooth. */
    @Volatile
    var radioIsPlaceholder: Boolean = true
        private set

    private val radio: BleRadio by lazy { UnimplementedRadio() }

    /**
     * The 8-byte protocol sender id. This is NOT the wallet address and NOT a BLE peer id;
     * it is the protocol's own identity, and A3 will derive it from the wallet key so it is
     * stable across launches. Until then it is a fixed development id, which is fine
     * precisely because nothing is on the air yet.
     */
    private val senderId: ByteArray = byteArrayOf(0x6E, 0x69, 0x6D, 0x6D, 0x65, 0x73, 0x68, 0x00)

    val node: MeshNode by lazy { MeshNode(senderId, radio) }
}
