package com.nimmesh.app

import android.app.NotificationManager
import android.content.Context
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.nimmesh.app.ble.BleMeshRadio
import org.junit.After
import org.junit.Before
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * A6: the foreground service that keeps the mesh alive off screen.
 *
 * On iOS this is two plist background modes and nothing visible. Android has no equivalent,
 * so a foreground service with a permanent notification is the only supported route, and
 * these assert the parts that would otherwise only be discovered by handing someone a build
 * that quietly stops relaying when the screen locks.
 */
@RunWith(AndroidJUnit4::class)
class MeshServiceDeviceTest {

    private val context: Context
        get() = InstrumentationRegistry.getInstrumentation().targetContext

    /**
     * ⚠ A `connectedDevice` foreground service needs BOTH the manifest
     * `FOREGROUND_SERVICE_CONNECTED_DEVICE` permission AND at least one RUNTIME-GRANTED
     * permission from a fixed list, which for this app means one of the Bluetooth trio.
     * Without that the system throws `SecurityException` at `startForeground` and the
     * service never comes up.
     *
     * That is why `MainActivity` starts the service only after the Bluetooth grant lands,
     * and never at launch. Here it has to be arranged explicitly, because Gradle reinstalls
     * the app for every connected-test run and reinstalling CLEARS runtime grants.
     */
    @Before
    fun grantBluetoothPermissions() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        BleMeshRadio.REQUIRED_PERMISSIONS.forEach {
            try {
                instrumentation.uiAutomation.grantRuntimePermission(context.packageName, it)
            } catch (e: Exception) {
                // Reported as a failure below rather than silently skipped.
            }
        }
        try {
            instrumentation.uiAutomation.grantRuntimePermission(
                context.packageName, android.Manifest.permission.POST_NOTIFICATIONS,
            )
        } catch (e: Exception) {
            // Older devices have no such runtime permission.
        }
    }

    @After
    fun tearDown() {
        MeshService.stop(context)
        // `isRunning` is process-wide and only clears in onDestroy, which is asynchronous.
        // Returning before it does leaves the next test's start() short-circuited by the
        // redundant-start guard, and that test then fails for a reason that has nothing to
        // do with what it is checking.
        awaitRunning(false)
    }

    /** @return true if the service reached [expected] before the timeout. */
    private fun awaitRunning(expected: Boolean, timeoutMs: Long = 5_000): Boolean {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (MeshService.isRunning != expected && System.currentTimeMillis() < deadline) {
            Thread.sleep(100)
        }
        return MeshService.isRunning == expected
    }

    @Test
    fun theServiceStartsAndReportsThatItIsRunning() {
        MeshService.start(context)
        assertTrue(
            "the mesh service never came up. A connectedDevice FGS needs a runtime-granted " +
                "Bluetooth permission as well as the manifest one",
            awaitRunning(true),
        )
    }

    @Test
    fun itPostsAnOngoingNotificationRatherThanRunningInvisibly() {
        MeshService.start(context)
        assertTrue("the service did not start", awaitRunning(true))

        val manager = context.getSystemService(NotificationManager::class.java)
        val active = manager.activeNotifications.firstOrNull { it.packageName == context.packageName }
        assertNotNull("a foreground service with no visible notification is not allowed to exist", active)
        assertTrue(
            "the relay notification must be ongoing, or the user can swipe the mesh away by accident",
            active!!.isOngoing,
        )
    }

    @Test
    fun theChannelIsQuietBecauseItIsAStatusLineNotAnAlert() {
        MeshService.start(context)
        assertTrue("the service did not start", awaitRunning(true))

        val manager = context.getSystemService(NotificationManager::class.java)
        val channel = manager.notificationChannels.firstOrNull { it.id == "nimmesh.mesh" }
        assertNotNull("the mesh channel was never created", channel)
        // An always-present notification that pings would be worse than useless.
        assertEquals(
            "the relay channel must not make noise",
            NotificationManager.IMPORTANCE_LOW, channel!!.importance,
        )
        assertFalse("a permanent notification must not carry a badge", channel.canShowBadge())
    }

    @Test
    fun startingTwiceIsSafeAndStoppingIsIdempotent() {
        // A second startForegroundService opens a SECOND five-second contract that a
        // subsequent stop can strand, and the system then kills the app with
        // ForegroundServiceDidNotStartInTimeException somewhere unrelated. The guard in
        // MeshService.start is what makes this safe, not luck.
        MeshService.start(context)
        MeshService.start(context)
        assertTrue("the service did not start", awaitRunning(true))

        MeshService.stop(context)
        MeshService.stop(context)
        assertFalse("the service did not stop", MeshService.isRunning && !awaitRunning(false))
    }

    @Test
    fun stoppingImmediatelyAfterStartingDoesNotKillTheApp() {
        // THE hazard, and the reason stop() routes through the service instead of calling
        // stopService. startForegroundService opens a five-second contract; destroying the
        // service before it can call startForeground makes the system kill the whole app
        // with ForegroundServiceDidNotStartInTimeException.
        //
        // This is not hypothetical: MainActivity.onResume stops the relay when the Bluetooth
        // permissions have been revoked, which can land moments after a launch-time start.
        //
        // No sleep between the two calls on purpose. The sleep is what would hide it.
        repeat(3) {
            MeshService.start(context)
            MeshService.stop(context)
        }

        // Reaching this line at all is most of the assertion: if the app had been killed,
        // the instrumentation would have died with it and this test would never report.
        assertTrue("the service never settled", awaitRunning(false, 8_000))
        assertTrue("the process should still be alive and usable", context.packageName.isNotEmpty())

        // And it must still be startable afterwards, rather than wedged.
        MeshService.start(context)
        assertTrue("the service could not start again after the race", awaitRunning(true))
    }

    @Test
    fun meshStatusReportsWhyTheMeshIsEmptyNotJustThatItIs() {
        // The page can render "no peers nearby" or "turn Bluetooth on" or "this phone cannot
        // be discovered" only if the bridge tells it which. An empty mesh with no reason is
        // the state that wastes an afternoon.
        val bridge = Bridge(context)
        val answered = java.util.concurrent.ArrayBlockingQueue<String>(1)
        bridge.webView = null // resolve() no-ops without a view; the dispatch still runs
        val payload = bridge.javaClass.getDeclaredMethod(
            "dispatch", String::class.java, org.json.JSONObject::class.java,
        ).apply { isAccessible = true }.invoke(bridge, "meshStatus", null) as Pair<*, *>

        assertEquals(true, payload.first)
        val json = payload.second as org.json.JSONObject
        listOf("state", "peers", "permitted", "bluetoothOn", "canAdvertise", "relayingInBackground")
            .forEach { assertTrue("meshStatus is missing $it, got: $json", json.has(it)) }
        answered.clear()
    }
}
