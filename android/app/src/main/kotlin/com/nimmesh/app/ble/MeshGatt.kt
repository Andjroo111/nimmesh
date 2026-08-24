package com.nimmesh.app.ble

import java.util.UUID

/** The nimmesh GATT profile. Byte-identical to the iOS radio's, or the two never meet. */
object MeshGatt {

    val SERVICE_UUID: UUID = UUID.fromString("4E494D4D-4553-4800-0000-6E696D6D6573")
    val CHAR_UUID: UUID = UUID.fromString("4E494D4D-4553-4800-0001-6E696D6D6573")

    /**
     * The Client Characteristic Configuration Descriptor, 0x2902.
     *
     * ⚠ This has NO CoreBluetooth counterpart and is the single most common reason an
     * Android GATT server never delivers a notification. iOS synthesises the CCCD for you
     * and `didSubscribeTo` just fires. On Android the server must ADD this descriptor, and
     * the client must WRITE `ENABLE_NOTIFICATION_VALUE` into it; `setCharacteristicNotification`
     * alone only sets a local flag and tells the remote device nothing.
     *
     * Missing it fails silently in the worst possible shape: connections succeed, writes
     * succeed one way, and the reverse direction is simply never delivered.
     */
    val CCCD_UUID: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

    /**
     * The MTU to ask for. A nimmesh packet is up to 256 bytes and the default ATT MTU is 23,
     * which leaves 20 usable, so without this every packet would be truncated or refused.
     * 517 is the maximum ATT MTU; the stack negotiates down and whatever it lands on is
     * reported through `onMtuChanged`.
     */
    const val DESIRED_MTU = 517

    /** ATT overhead: a write-without-response carries MTU minus 3 bytes. */
    const val ATT_HEADER = 3
}
