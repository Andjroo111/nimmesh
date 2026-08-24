package com.nimmesh.app.ble

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothProfile
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.os.Build
import android.os.Handler
import android.os.ParcelUuid
import android.util.Log

/**
 * The central half: scan, connect out, subscribe, write.
 *
 * Peer ids are the device MAC address as Android reports it, which is this platform's
 * connection identity, exactly as the peripheral UUID string is on iOS. The protocol never
 * sees it; it routes bytes to a connected peer and is blind to what they mean.
 */
@SuppressLint("MissingPermission") // guarded by BleMeshRadio.ready() before anything starts
internal class CentralRole(
    private val context: Context,
    private val adapter: BluetoothAdapter?,
    private val handler: Handler,
    private val events: () -> RadioEvents,
) {

    private val gatts = HashMap<String, BluetoothGatt>()
    private val writeChars = HashMap<String, BluetoothGattCharacteristic>()
    private val connecting = HashSet<String>()

    var isScanning = false
        private set
    var discovered = 0
        private set
    var connected = 0
        private set

    fun start() {
        if (isScanning) return
        val scanner = adapter?.bluetoothLeScanner ?: return
        val filters = listOf(
            ScanFilter.Builder().setServiceUuid(ParcelUuid(MeshGatt.SERVICE_UUID)).build(),
        )
        val settings = ScanSettings.Builder()
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            // Report every sighting, not just the first. A peer that drops and returns must
            // be seen again, or the mesh never heals after a walk out of range.
            .setCallbackType(ScanSettings.CALLBACK_TYPE_ALL_MATCHES)
            .build()
        scanner.startScan(filters, settings, scanCallback)
        // ⚠ Weaker evidence than the advertiser's flag, and worth knowing when debugging an
        // empty mesh. `startAdvertising` reports success asynchronously through
        // `onStartSuccess`, so `adv:on` means the STACK accepted it. `startScan` has no
        // success callback at all, only `onScanFailed`, so `scan:on` means only that the
        // call was made and has not failed yet.
        isScanning = true
    }

    fun stop() {
        if (isScanning) {
            adapter?.bluetoothLeScanner?.stopScan(scanCallback)
            isScanning = false
        }
        gatts.values.forEach { it.close() }
        gatts.clear()
        writeChars.clear()
        connecting.clear()
    }

    fun disconnect(peerId: String) {
        gatts[peerId]?.disconnect()
    }

    /** @return true if the write was handed to the stack. Never blocks. */
    fun write(peerId: String, bytes: ByteArray): Boolean {
        val gatt = gatts[peerId] ?: return false
        val characteristic = writeChars[peerId] ?: return false
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            gatt.writeCharacteristic(
                characteristic,
                bytes,
                BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE,
            ) == BluetoothGatt.GATT_SUCCESS
        } else {
            // API 31 and 32. The value is carried on the characteristic object itself, which
            // means two writes in flight to the same characteristic race. The radio
            // serialises everything onto one worker, so they cannot be.
            @Suppress("DEPRECATION")
            run {
                characteristic.writeType = BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
                characteristic.value = bytes
                gatt.writeCharacteristic(characteristic)
            }
        }
    }

    private val scanCallback = object : ScanCallback() {
        override fun onScanResult(callbackType: Int, result: ScanResult?) {
            val device = result?.device ?: return
            handler.post { connect(device) }
        }

        override fun onScanFailed(errorCode: Int) {
            Log.w(TAG, "scan failed: $errorCode")
            isScanning = false
        }
    }

    private fun connect(device: BluetoothDevice) {
        val id = device.address
        discovered++
        // CALLBACK_TYPE_ALL_MATCHES means the same device is reported continuously. Without
        // this guard every advertisement would open another GATT connection, and Android
        // caps concurrent connections low enough that the mesh would wedge within seconds.
        if (gatts.containsKey(id) || !connecting.add(id)) return
        device.connectGatt(context, false, gattCallback, BluetoothDevice.TRANSPORT_LE)
    }

    private val gattCallback = object : BluetoothGattCallback() {

        override fun onConnectionStateChange(gatt: BluetoothGatt, status: Int, newState: Int) {
            val id = gatt.device.address
            handler.post {
                when (newState) {
                    BluetoothProfile.STATE_CONNECTED -> {
                        connected++
                        gatts[id] = gatt
                        // Ask before discovering: a later MTU change can invalidate what was
                        // already discovered on some stacks.
                        gatt.requestMtu(MeshGatt.DESIRED_MTU)
                    }

                    BluetoothProfile.STATE_DISCONNECTED -> {
                        connecting.remove(id)
                        gatts.remove(id)?.close()
                        writeChars.remove(id)
                        events().onLinkDown(id, PeerLinks.Role.CENTRAL)
                        // Keep the mesh healing. The scan is still running, so the peer will
                        // be rediscovered when it comes back.
                    }
                }
            }
        }

        override fun onMtuChanged(gatt: BluetoothGatt, mtu: Int, status: Int) {
            // Whether or not the MTU request succeeded, discovery has to happen. A small MTU
            // is a smaller packet budget, not a dead link.
            handler.post { gatt.discoverServices() }
        }

        override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
            handler.post {
                val id = gatt.device.address
                val characteristic = gatt.getService(MeshGatt.SERVICE_UUID)
                    ?.getCharacteristic(MeshGatt.CHAR_UUID)
                if (characteristic == null) {
                    Log.w(TAG, "$id advertises the service but has no mesh characteristic")
                    gatt.disconnect()
                    return@post
                }
                writeChars[id] = characteristic
                subscribe(gatt, characteristic)
                // PeerLinks deduplicates by role, so a repeated discovery cannot double-count.
                events().onLinkUp(id, PeerLinks.Role.CENTRAL)
            }
        }

        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray,
        ) {
            handler.post { events().onPacket(gatt.device.address, value) }
        }

        @Deprecated("Pre-API-33 delivery path", ReplaceWith(""))
        @Suppress("DEPRECATION")
        override fun onCharacteristicChanged(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic?,
        ) {
            // API 31 and 32 deliver the value on the characteristic rather than as an
            // argument. Both overloads must exist or inbound bytes are silently dropped on
            // exactly the versions minSdk 31 was chosen to include.
            val value = characteristic?.value ?: return
            handler.post { events().onPacket(gatt.device.address, value) }
        }
    }

    /**
     * Subscribing on Android takes TWO steps, and this is the classic silent failure.
     * `setCharacteristicNotification` only sets a local flag; the remote device is told
     * nothing until the CCCD is written. Skip the second step and everything looks healthy
     * while no notification is ever delivered.
     */
    private fun subscribe(gatt: BluetoothGatt, characteristic: BluetoothGattCharacteristic) {
        gatt.setCharacteristicNotification(characteristic, true)
        val cccd = characteristic.getDescriptor(MeshGatt.CCCD_UUID) ?: run {
            Log.w(TAG, "no CCCD on the mesh characteristic; the reverse path will be silent")
            return
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            gatt.writeDescriptor(cccd, BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE)
        } else {
            @Suppress("DEPRECATION")
            run {
                cccd.value = BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
                gatt.writeDescriptor(cccd)
            }
        }
    }

    private companion object {
        const val TAG = "nimmesh.radio"
    }
}
