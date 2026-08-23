package com.nimmesh.core

import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.nimmesh_core.BleRadio
import uniffi.nimmesh_core.MeshNode
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.CopyOnWriteArrayList

/**
 * A1: the gate that proves the Rust core actually runs on an Android device, and that
 * the ADR-0002 seam works in BOTH directions.
 *
 * This is deliberately not a "read a version string" test. It drives the same
 * `BleRadio` foreign trait the real Kotlin radio will implement in A5, so a failure
 * here means the whole Android plan is wrong, not that a string changed:
 *
 *  - Rust calls OUT to Kotlin: `MeshNode(..)` brings the radio up, so `startAdvertising`
 *    and `startScanning` must land on this Kotlin object.
 *  - Kotlin calls IN to Rust: `onPeerConnected` then `submitLocalTx` must make the Rust
 *    worker flood the tx back out through `send`.
 *  - The bytes survive the round trip: the packet the radio is handed must CONTAIN the
 *    exact transaction bytes that went in (the mesh wraps them in a header and never
 *    inspects the payload).
 *
 * It also proves the .so loads under JNA on a real ABI and that the UniFFI contract
 * version matches, which is the failure that aborts the iOS app at launch when the
 * bindings and the runtime drift apart.
 */
@RunWith(AndroidJUnit4::class)
class CoreFfiSmokeTest {

    /** The pure-Kotlin stand-in for A5's CoreBluetooth-equivalent radio. */
    private class RecordingRadio : BleRadio {
        val advertised = CountDownLatch(1)
        val scanned = CountDownLatch(1)
        val sent = CountDownLatch(1)
        val sentBytes = CopyOnWriteArrayList<ByteArray>()
        val sentPeers = CopyOnWriteArrayList<String>()
        @Volatile var stopped = false

        override fun startAdvertising() { advertised.countDown() }
        override fun startScanning() { scanned.countDown() }

        /**
         * Fire and forget (ADR-0002 gotcha b): record and return immediately. Blocking
         * here, or calling back into the node, is exactly what the real radio must not do.
         */
        override fun send(peerId: String, bytes: ByteArray) {
            sentPeers.add(peerId)
            sentBytes.add(bytes)
            sent.countDown()
        }

        override fun disconnect(peerId: String) = Unit
        override fun stop() { stopped = true }
    }

    @Test
    fun rustBringsTheRadioUpAndFloodsALocalTxBackThroughIt() {
        val radio = RecordingRadio()
        // The 8-byte protocol sender id. Distinct from a BLE peer id, which is a
        // connection identity the protocol never sees.
        val senderId = byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8)
        val node = MeshNode(senderId, radio)

        try {
            // Rust -> Kotlin. `MeshNode::new` brings the radio up.
            assertTrue(
                "Rust never called startAdvertising on the Kotlin radio",
                radio.advertised.await(5, TimeUnit.SECONDS)
            )
            assertTrue(
                "Rust never called startScanning on the Kotlin radio",
                radio.scanned.await(5, TimeUnit.SECONDS)
            )

            // Kotlin -> Rust. A peer is up, so a locally submitted tx has somewhere to go.
            node.onPeerConnected("peer-1")

            // A stand-in for a signed transaction. The mesh treats it as opaque bytes,
            // so its contents only have to be recognisable, not valid.
            val txWire = ByteArray(139) { i -> (i and 0xFF).toByte() }
            val txId = node.submitLocalTx(txWire)
            assertEquals("txId should be the protocol's 32 bytes", 32, txId.size)

            // Rust -> Kotlin again, this time from the worker thread after relay jitter.
            assertTrue(
                "the Rust worker never flooded the tx out through the radio",
                radio.sent.await(10, TimeUnit.SECONDS)
            )
            assertEquals("peer-1", radio.sentPeers.first())

            val packet = radio.sentBytes.first()
            assertTrue(
                "the flooded packet (${packet.size} B) is smaller than the tx it carries",
                packet.size >= txWire.size
            )
            assertTrue(
                "the flooded packet does not contain the transaction bytes verbatim",
                packet.indexOfSubsequence(txWire) >= 0
            )
        } finally {
            node.shutdown()
            node.close()
        }
    }

    private fun ByteArray.indexOfSubsequence(needle: ByteArray): Int {
        if (needle.isEmpty() || needle.size > size) return -1
        outer@ for (i in 0..(size - needle.size)) {
            for (j in needle.indices) if (this[i + j] != needle[j]) continue@outer
            return i
        }
        return -1
    }
}
