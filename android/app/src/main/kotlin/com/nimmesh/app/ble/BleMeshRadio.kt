package com.nimmesh.app.ble

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import uniffi.nimmesh_core.BleRadio
import uniffi.nimmesh_core.MeshNode

/**
 * The Android `BleRadio`, the ADR-0002 shim: Kotlin owns the radio, Rust owns everything
 * above the byte stream.
 *
 * Every node runs BOTH roles at once, a GATT server advertising the nimmesh service and a
 * scanner connecting out to other people's servers. That dual role IS the mesh, and it is
 * the property Web Bluetooth cannot provide, which is why this is a native app at all.
 *
 * ### The four ADR-0002 gotchas, and where each is handled
 *
 *  - **`send` is fire and forget.** It returns immediately; the real GATT outcome arrives
 *    later through `MeshNode.onSendResult`. Nothing here blocks.
 *  - **The hot path never re-enters the radio synchronously.** Inbound bytes go straight to
 *    `onPacketReceivedFrom`, which only enqueues on the Rust side.
 *  - **Every callback is wrapped defensively.** An exception crossing back into Rust during
 *    a flood burst surfaces as `UnexpectedUniFFICallbackError` and aborts the process.
 *  - **The refcount cycle is broken by an explicit lifecycle**, not by GC. Kotlin has no
 *    deterministic collection, so a leaked handle would pin the Bluetooth stack. [stop]
 *    is the edge.
 *
 * Everything is serialised onto one worker thread, which is what makes [PeerLinks] and the
 * connection maps safe without locks.
 */
@SuppressLint("MissingPermission") // every entry point routes through `hasPermissions()`
class BleMeshRadio(context: Context) : BleRadio {

    private val appContext = context.applicationContext
    private val thread = HandlerThread("nimmesh-ble").apply { start() }
    private val handler = Handler(thread.looper)

    private val manager: BluetoothManager? =
        appContext.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
    private val adapter: BluetoothAdapter? = manager?.adapter

    private val links = PeerLinks()
    private val central = CentralRole(appContext, adapter, handler, ::events)
    private val peripheral = PeripheralRole(appContext, manager, handler, ::events)

    /**
     * The Rust node, held STRONGLY.
     *
     * A weak reference was tried on iOS and released out from under the radio on device, so
     * `onPeerConnected` silently no-opped: the radio counted a peer the node never heard
     * about. The node is app-lifetime here, so this is an accepted one-instance cycle in
     * exchange for callbacks that actually land, and [stop] breaks it.
     *
     * Replacing it REPLAYS the live links onto the new node. Without that, swapping the node
     * while a peer is connected leaves the new one permanently at zero peers.
     */
    @Volatile
    var node: MeshNode? = null
        set(value) {
            field = value
            val n = value ?: return
            handler.post {
                links.liveIds.forEach { guarded("replay onPeerConnected") { n.onPeerConnected(it) } }
            }
        }

    // ---- BleRadio, called from Rust ----------------------------------------------------

    override fun startAdvertising() {
        handler.post {
            if (!ready("startAdvertising")) return@post
            peripheral.start()
        }
    }

    override fun startScanning() {
        handler.post {
            if (!ready("startScanning")) return@post
            central.start()
        }
    }

    /**
     * Fire and forget. The central-to-peripheral write is preferred; a notify to a
     * subscribed client is the fallback. Either way the outcome is reported asynchronously
     * and this returns at once.
     */
    override fun send(peerId: String, bytes: ByteArray) {
        handler.post {
            val ok = guarded("send") {
                central.write(peerId, bytes) || peripheral.notify(peerId, bytes)
            } ?: false
            node?.let { n -> guarded("onSendResult") { n.onSendResult(peerId, ok) } }
        }
    }

    override fun disconnect(peerId: String) {
        handler.post { guarded("disconnect") { central.disconnect(peerId) } }
    }

    /** Tear down and release the radio. This is the edge that breaks the node/radio cycle. */
    override fun stop() {
        handler.post {
            guarded("stop") {
                central.stop()
                peripheral.stop()
                links.clear()
            }
            node = null
        }
    }

    // ---- what the two roles call back into ---------------------------------------------

    private fun events(): RadioEvents = eventSink

    private val eventSink = object : RadioEvents {
        override fun onLinkUp(peerId: String, role: PeerLinks.Role) {
            if (links.up(peerId, role)) {
                node?.let { n -> guarded("onPeerConnected") { n.onPeerConnected(peerId) } }
            }
        }

        override fun onLinkDown(peerId: String, role: PeerLinks.Role) {
            if (links.down(peerId, role)) {
                node?.let { n -> guarded("onPeerDisconnected") { n.onPeerDisconnected(peerId) } }
            }
        }

        override fun onPacket(peerId: String, bytes: ByteArray) {
            // Straight through. The Rust side only enqueues, so this cannot re-enter us.
            node?.let { n -> guarded("onPacketReceivedFrom") { n.onPacketReceivedFrom(peerId, bytes) } }
        }
    }

    // ---- state a UI can honestly report -------------------------------------------------

    /**
     * Whether this device can advertise at all. A meaningful tail of Android hardware
     * cannot, and such a phone still relays and still pays as a central; it simply cannot
     * be DISCOVERED. That has to be surfaced rather than looking like an empty mesh.
     */
    fun canAdvertise(): Boolean = adapter?.isMultipleAdvertisementSupported == true

    fun bluetoothEnabled(): Boolean = adapter?.isEnabled == true

    fun hasPermissions(): Boolean = REQUIRED_PERMISSIONS.all {
        appContext.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED
    }

    /** The live radio state, surfaced to the Network screen through `meshDebug`. */
    fun debugSummary(): String {
        val perm = if (hasPermissions()) "ok" else "DENIED"
        val bt = if (bluetoothEnabled()) "on" else "off"
        val adv = if (!canAdvertise()) "UNSUPPORTED" else if (peripheral.isAdvertising) "on" else "off"
        return "perm:$perm bt:$bt adv:$adv scan:${if (central.isScanning) "on" else "off"} | " +
            "disc:${central.discovered} conn:${central.connected} subs:${peripheral.subscribers} | " +
            "peers:${links.peerCount}"
    }

    private fun ready(what: String): Boolean {
        if (adapter == null) {
            Log.w(TAG, "$what: this device has no Bluetooth adapter")
            return false
        }
        if (!hasPermissions()) {
            // Not an error. The radio is constructed at launch and the permissions are asked
            // for at the moment they are needed, so this is the normal state until then.
            Log.i(TAG, "$what: waiting for the Bluetooth permissions")
            return false
        }
        if (!bluetoothEnabled()) {
            Log.i(TAG, "$what: Bluetooth is off")
            return false
        }
        return true
    }

    /**
     * ADR-0002 gotcha (c). An exception escaping a callback into Rust surfaces as
     * `UnexpectedUniFFICallbackError` and aborts the PROCESS, so a transient GATT error
     * during a flood burst must never be allowed to become a crash.
     */
    private fun <T> guarded(what: String, block: () -> T): T? = try {
        block()
    } catch (e: Throwable) {
        Log.e(TAG, "$what threw, swallowed so it cannot abort the process", e)
        null
    }

    companion object {
        private const val TAG = "nimmesh.radio"

        /**
         * API 31 and up. `BLUETOOTH_SCAN` is declared `neverForLocation` in the manifest,
         * which is the whole reason minSdk is 31: below it, scanning is impossible without
         * `ACCESS_FINE_LOCATION`, and a wallet asking for your location reads badly and is
         * badly worth it.
         */
        val REQUIRED_PERMISSIONS = arrayOf(
            Manifest.permission.BLUETOOTH_SCAN,
            Manifest.permission.BLUETOOTH_ADVERTISE,
            Manifest.permission.BLUETOOTH_CONNECT,
        )
    }
}

/** What the two role objects report back to the radio. */
interface RadioEvents {
    fun onLinkUp(peerId: String, role: PeerLinks.Role)
    fun onLinkDown(peerId: String, role: PeerLinks.Role)
    fun onPacket(peerId: String, bytes: ByteArray)
}
