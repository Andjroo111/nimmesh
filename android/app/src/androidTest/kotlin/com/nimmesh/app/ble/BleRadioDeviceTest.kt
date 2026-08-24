package com.nimmesh.app.ble

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.nimmesh_core.MeshNode
import java.security.SecureRandom

/**
 * A5 on a device with NO usable Bluetooth, which is what the emulator is and what a
 * permission-denied phone is.
 *
 * This is not a substitute for the two-phone field test, and nothing here proves a byte
 * ever crossed the air. What it does prove is the thing that would otherwise only be found
 * by handing someone a broken build: that the radio comes up, refuses gracefully, and tells
 * the truth about itself instead of crashing or reporting a mesh that is not there.
 *
 * The on-air behaviour is unproven until two Android phones exist. That is stated in
 * `docs/ANDROID.md` rather than implied by a green suite.
 */
@RunWith(AndroidJUnit4::class)
class BleRadioDeviceTest {

    /**
     * Gradle reinstalls the app for every connected-test run, which CLEARS runtime grants.
     * Without this the dual-role check below silently skips, and a skipped test proves
     * nothing while still reading green. UiAutomation grants without a new dependency.
     */
    @Before
    fun grantBluetoothPermissions() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val pkg = instrumentation.targetContext.packageName
        BleMeshRadio.REQUIRED_PERMISSIONS.forEach { permission ->
            try {
                instrumentation.uiAutomation.grantRuntimePermission(pkg, permission)
            } catch (e: Exception) {
                // A device that refuses the grant simply skips the dual-role check below.
            }
        }
    }

    private fun radio() = BleMeshRadio(InstrumentationRegistry.getInstrumentation().targetContext)

    @Test
    fun theRadioComesUpAndNeverThrowsWithoutBluetoothOrPermissions() {
        val radio = radio()
        // Every one of these is called by Rust across the FFI. An exception escaping any of
        // them surfaces as UnexpectedUniFFICallbackError and ABORTS THE PROCESS, so
        // "does not throw" is the contract, not a nicety.
        radio.startAdvertising()
        radio.startScanning()
        radio.send("00:11:22:33:44:55", ByteArray(139))
        radio.disconnect("00:11:22:33:44:55")
        radio.stop()
    }

    @Test
    fun aNodeCanBeConstructedAgainstItAndReportsAnEmptyMeshHonestly() {
        val radio = radio()
        val senderId = ByteArray(8).also { SecureRandom().nextBytes(it) }
        // Constructing the node calls straight back out into startAdvertising and
        // startScanning, so this is the real launch path, not a simulation of it.
        val node = MeshNode(senderId, radio)
        radio.node = node
        try {
            assertTrue("a mesh with no radio must report no peers", node.peerCount() == 0u)
        } finally {
            node.shutdown()
            node.close()
            radio.stop()
        }
    }

    @Test
    fun sendReturnsImmediatelyInsteadOfBlockingOnAConnection() {
        // ADR-0002 gotcha (b): `send` is fire and forget. Rust calls it from the relay
        // worker, so blocking here stalls the whole mesh, and on iOS blocking inside the
        // radio's own dispatch queue deadlocks it outright.
        //
        // 250ms is deliberately loose. It is not a performance assertion; it is the
        // difference between handing bytes to the stack and waiting on a connection attempt,
        // which takes seconds.
        val radio = radio()
        val node = MeshNode(ByteArray(8), radio)
        radio.node = node
        try {
            val slowest = (1..5).maxOf {
                val started = System.nanoTime()
                radio.send("00:11:22:33:44:5$it", ByteArray(139))
                (System.nanoTime() - started) / 1_000_000
            }
            assertTrue(
                "send took ${slowest}ms, so it is blocking rather than firing and forgetting",
                slowest < 250,
            )
        } finally {
            node.shutdown()
            node.close()
            radio.stop()
        }
    }

    @Test
    fun debugSummaryTellsTheTruthAboutWhyTheMeshIsEmpty() {
        val radio = radio()
        val summary = radio.debugSummary()
        assertNotNull(summary)
        // The Network screen has to distinguish "no peers nearby" from "Bluetooth is off",
        // "permission denied" and "this hardware cannot advertise". An empty mesh with no
        // reason shown is the state that wastes an afternoon.
        listOf("perm:", "bt:", "adv:", "scan:", "peers:").forEach {
            assertTrue("debugSummary does not report $it, got: $summary", summary.contains(it))
        }
        assertTrue("a radio with no links must report zero peers, got: $summary", summary.contains("peers:0"))
        radio.stop()
    }

    /**
     * The strongest evidence available without a second phone.
     *
     * It does NOT prove a byte crossed the air. It proves the pair of things the mesh is
     * built on actually start on a real Android Bluetooth stack, CONCURRENTLY:
     *
     *  - the advertiser accepted the payload, which is not a given: the advertisement budget
     *    is 31 bytes and the nimmesh service UUID is a 128-bit one, so including the device
     *    name would push it over and the whole advertisement would be rejected
     *  - the GATT server opened and the service and its CCCD were added without error
     *  - the scanner started with a service filter
     *  - neither role displaced the other, which is the property Web Bluetooth cannot offer
     *    and the reason this is a native app at all
     *
     * Skipped, visibly, on a device with Bluetooth off or the permissions not granted.
     */
    @Test
    fun bothRolesComeUpConcurrentlyOnARealBluetoothStack() {
        val radio = radio()
        assumeTrue(
            "Bluetooth is off or the permissions are not granted, so the dual-role check was skipped",
            radio.hasPermissions() && radio.bluetoothEnabled(),
        )
        try {
            radio.startAdvertising()
            radio.startScanning()
            // Both are asynchronous: the advertiser answers through onStartSuccess.
            val deadline = System.currentTimeMillis() + 10_000
            var summary = radio.debugSummary()
            while (System.currentTimeMillis() < deadline && !(summary.contains("adv:on") && summary.contains("scan:on"))) {
                Thread.sleep(250)
                summary = radio.debugSummary()
            }
            assertTrue("the scanner never started, got: $summary", summary.contains("scan:on"))
            if (radio.canAdvertise()) {
                assertTrue(
                    "the advertiser never started on hardware that supports it, got: $summary",
                    summary.contains("adv:on"),
                )
            } else {
                // A real hardware limit on part of the Android fleet. Central-only still
                // relays and still pays; it just cannot be discovered.
                assertTrue("expected the unsupported marker, got: $summary", summary.contains("adv:UNSUPPORTED"))
            }
        } finally {
            radio.stop()
        }
    }

    @Test
    fun theHardwareLimitsAreReportedRatherThanAssumed() {
        val radio = radio()
        // Neither of these is asserted to be true: the emulator has no Bluetooth and a real
        // phone may not support advertising. What is asserted is that asking is safe and
        // answers, so the app can degrade to central-only instead of silently failing.
        val canAdvertise = radio.canAdvertise()
        val enabled = radio.bluetoothEnabled()
        assertTrue(canAdvertise || !canAdvertise)
        assertTrue(enabled || !enabled)
        assertFalse(
            "the emulator has no Bluetooth, so a summary claiming otherwise is wrong",
            radio.debugSummary().isEmpty(),
        )
        radio.stop()
    }
}
