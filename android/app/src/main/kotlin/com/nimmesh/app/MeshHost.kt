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
     * The 8-byte protocol sender id: random per launch, matching what the iOS shim does
     * (`SwapMesh.makeNormalNode`).
     *
     * Deliberately NOT derived from the wallet. It is the protocol's own identity, seen by
     * every relay that forwards a packet, and tying it to the wallet key would make a
     * user's payments linkable across the mesh by anyone listening. It is also not a BLE
     * peer id, which is a connection identity the protocol never sees.
     */
    private val senderId: ByteArray = ByteArray(8).also { java.security.SecureRandom().nextBytes(it) }

    val node: MeshNode by lazy { MeshNode(senderId, radio) }
}
