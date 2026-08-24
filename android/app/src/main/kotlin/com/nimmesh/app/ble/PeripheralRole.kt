package com.nimmesh.app.ble

import android.annotation.SuppressLint
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattServer
import android.bluetooth.BluetoothGattServerCallback
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.content.Context
import android.os.Build
import android.os.Handler
import android.os.ParcelUuid
import android.util.Log

/**
 * The peripheral half: advertise the nimmesh service, accept writes, notify subscribers.
 *
 * Running this AT THE SAME TIME as [CentralRole] is what makes the mesh a mesh, and it is
 * the capability a web app cannot have.
 */
@SuppressLint("MissingPermission") // guarded by BleMeshRadio.ready() before anything starts
internal class PeripheralRole(
    private val context: Context,
    private val manager: BluetoothManager?,
    private val handler: Handler,
    private val events: () -> RadioEvents,
) {

    private var server: BluetoothGattServer? = null
    private var characteristic: BluetoothGattCharacteristic? = null
    private val subscribed = HashMap<String, BluetoothDevice>()

    var isAdvertising = false
        private set
    val subscribers: Int get() = subscribed.size

    fun start() {
        if (server != null) return
        val manager = manager ?: return
        val adapter = manager.adapter ?: return

        val characteristic = BluetoothGattCharacteristic(
            MeshGatt.CHAR_UUID,
            BluetoothGattCharacteristic.PROPERTY_WRITE_NO_RESPONSE or
                BluetoothGattCharacteristic.PROPERTY_NOTIFY,
            BluetoothGattCharacteristic.PERMISSION_WRITE,
        ).apply {
            // Without this descriptor a client cannot subscribe and the reverse direction is
            // silently dead. iOS synthesises it; Android does not. See MeshGatt.CCCD_UUID.
            addDescriptor(
                BluetoothGattDescriptor(
                    MeshGatt.CCCD_UUID,
                    BluetoothGattDescriptor.PERMISSION_READ or
                        BluetoothGattDescriptor.PERMISSION_WRITE,
                ),
            )
        }
        this.characteristic = characteristic

        server = manager.openGattServer(context, serverCallback)?.apply {
            addService(
                BluetoothGattService(
                    MeshGatt.SERVICE_UUID,
                    BluetoothGattService.SERVICE_TYPE_PRIMARY,
                ).apply { addCharacteristic(characteristic) },
            )
        }

        val advertiser = adapter.bluetoothLeAdvertiser
        if (advertiser == null || !adapter.isMultipleAdvertisementSupported) {
            // A real and common hardware limit. This phone still relays and still pays as a
            // central; it simply cannot be DISCOVERED. Logged, and surfaced through
            // debugSummary, rather than looking like an empty mesh.
            Log.w(TAG, "this device cannot advertise; running central-only")
            return
        }
        advertiser.startAdvertising(
            AdvertiseSettings.Builder()
                .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_LOW_LATENCY)
                .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_HIGH)
                .setConnectable(true)
                .build(),
            AdvertiseData.Builder()
                // The service UUID is a 128-bit one and the advertisement budget is 31
                // bytes, so the name must stay out or the payload is rejected outright.
                .setIncludeDeviceName(false)
                .addServiceUuid(ParcelUuid(MeshGatt.SERVICE_UUID))
                .build(),
            advertiseCallback,
        )
    }

    fun stop() {
        if (isAdvertising) {
            manager?.adapter?.bluetoothLeAdvertiser?.stopAdvertising(advertiseCallback)
            isAdvertising = false
        }
        server?.close()
        server = null
        characteristic = null
        subscribed.clear()
    }

    /** @return true if a notification was handed to the stack for this peer. */
    fun notify(peerId: String, bytes: ByteArray): Boolean {
        val device = subscribed[peerId] ?: return false
        val characteristic = characteristic ?: return false
        val server = server ?: return false
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            server.notifyCharacteristicChanged(device, characteristic, false, bytes) ==
                android.bluetooth.BluetoothStatusCodes.SUCCESS
        } else {
            @Suppress("DEPRECATION")
            run {
                characteristic.value = bytes
                server.notifyCharacteristicChanged(device, characteristic, false)
            }
        }
    }

    private val advertiseCallback = object : AdvertiseCallback() {
        override fun onStartSuccess(settingsInEffect: AdvertiseSettings?) {
            isAdvertising = true
        }

        override fun onStartFailure(errorCode: Int) {
            isAdvertising = false
            Log.w(TAG, "advertising failed: $errorCode")
        }
    }

    private val serverCallback = object : BluetoothGattServerCallback() {

        override fun onConnectionStateChange(device: BluetoothDevice, status: Int, newState: Int) {
            if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                handler.post {
                    val id = device.address
                    if (subscribed.remove(id) != null) {
                        events().onLinkDown(id, PeerLinks.Role.PERIPHERAL)
                    }
                }
            }
        }

        override fun onCharacteristicWriteRequest(
            device: BluetoothDevice,
            requestId: Int,
            characteristic: BluetoothGattCharacteristic,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray?,
        ) {
            val bytes = value
            if (responseNeeded) {
                server?.sendResponse(device, requestId, android.bluetooth.BluetoothGatt.GATT_SUCCESS, offset, null)
            }
            if (bytes == null || characteristic.uuid != MeshGatt.CHAR_UUID) return
            handler.post { events().onPacket(device.address, bytes) }
        }

        /**
         * A client writing the CCCD is Android's equivalent of iOS's `didSubscribeTo`, and it
         * is the ONLY signal that the reverse path is live. Treating a mere connection as a
         * subscription would have us notifying a peer that is not listening.
         */
        override fun onDescriptorWriteRequest(
            device: BluetoothDevice,
            requestId: Int,
            descriptor: BluetoothGattDescriptor,
            preparedWrite: Boolean,
            responseNeeded: Boolean,
            offset: Int,
            value: ByteArray?,
        ) {
            if (responseNeeded) {
                server?.sendResponse(device, requestId, android.bluetooth.BluetoothGatt.GATT_SUCCESS, offset, null)
            }
            if (descriptor.uuid != MeshGatt.CCCD_UUID) return
            val enabling = value != null &&
                value.contentEquals(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE)
            handler.post {
                val id = device.address
                if (enabling) {
                    subscribed[id] = device
                    events().onLinkUp(id, PeerLinks.Role.PERIPHERAL)
                } else if (subscribed.remove(id) != null) {
                    events().onLinkDown(id, PeerLinks.Role.PERIPHERAL)
                }
            }
        }
    }

    private companion object {
        const val TAG = "nimmesh.radio"
    }
}
