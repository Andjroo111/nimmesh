package com.nimmesh.app

import android.content.Context
import com.nimmesh.app.ble.BleMeshRadio
import uniffi.nimmesh_core.MeshNode
import java.security.SecureRandom

/**
 * Owns the single app-lifetime [MeshNode] and the radio it drives.
 *
 * App-lifetime on purpose, matching the iOS shim: the node holds the radio strongly, and
 * the accepted one-instance cycle buys callbacks that actually land. `shutdown()` breaks it
 * if a teardown is ever needed.
 */
object MeshHost {

    private var radioInstance: BleMeshRadio? = null
    private var nodeInstance: MeshNode? = null

    /**
     * The 8-byte protocol sender id: random per launch, matching iOS
     * (`SwapMesh.makeNormalNode`).
     *
     * Deliberately NOT derived from the wallet. It is the protocol's own identity, seen by
     * every relay that forwards a packet, and tying it to the wallet key would make a
     * user's payments linkable across the mesh by anyone listening. It is also not a BLE
     * peer id, which is a connection identity the protocol never sees.
     */
    private val senderId: ByteArray = ByteArray(8).also { SecureRandom().nextBytes(it) }

    /**
     * Build the node and bring the radio up. Constructing [MeshNode] calls straight back out
     * to `startAdvertising` and `startScanning`, so the radio has to exist first.
     */
    @Synchronized
    fun start(context: Context): MeshNode {
        nodeInstance?.let { return it }
        val radio = BleMeshRadio(context)
        val node = MeshNode(senderId, radio)
        // The radio outlives any single node and replays its live links onto whoever the
        // node is now. Without this a node installed after a peer linked would sit at zero
        // peers forever.
        radio.node = node
        radioInstance = radio
        nodeInstance = node
        return node
    }

    /** The live radio, or null before [start]. Used for the honest Network-screen readout. */
    val radio: BleMeshRadio? get() = radioInstance

    /**
     * Called once the Bluetooth permissions are granted. The radio is constructed at launch,
     * before the user has been asked, so its first `startAdvertising` and `startScanning`
     * are no-ops. Something has to ask again afterwards or the mesh never comes up at all.
     */
    fun onPermissionsGranted() {
        radioInstance?.let {
            it.startAdvertising()
            it.startScanning()
        }
    }
}
