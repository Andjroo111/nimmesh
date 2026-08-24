package com.nimmesh.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * Keeps the mesh alive when the app is not on screen.
 *
 * On iOS this is a plist entry: `bluetooth-central` and `bluetooth-peripheral` background
 * modes, and the OS keeps CoreBluetooth running with nothing shown to the user. Android has
 * no equivalent. The ONLY supported way to hold a BLE connection with the app backgrounded
 * is a foreground service, and a foreground service must show a notification. That
 * notification is not a design choice; it is the price of relaying at all.
 *
 * The honest framing for the user is in the notification text: this phone is carrying
 * payments for people nearby. That is what it is doing, and it is why the app is allowed to
 * keep the radio open.
 *
 * ⚠ **This does not defeat Doze, and nothing can.** Deep Doze suspends the app entirely, and
 * several vendors (Xiaomi, Oppo, Samsung, OnePlus) kill background work far more
 * aggressively than stock Android does regardless of foreground-service status. The
 * mitigation is the battery-optimisation exemption offered in [MainActivity] plus saying so
 * plainly, not a trick. This is the Android twin of the iOS background overflow-area dead
 * spot: a platform fact, mitigated rather than solved.
 */
class MeshService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        isRunning = true
        pendingStart = false
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // startForeground FIRST, unconditionally, even on the STOP path. The five-second
        // contract opened by startForegroundService is already running by the time this is
        // reached, and failing to honour it kills the whole app with
        // ForegroundServiceDidNotStartInTimeException.
        try {
            createChannel()
            val notification = buildNotification()
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                // API 34 and up demand a declared type, and it must match a permission the
                // manifest holds. connectedDevice is the one that covers a BLE link.
                startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
                )
            } else {
                startForeground(NOTIFICATION_ID, notification)
            }
        } catch (e: Exception) {
            // A foreground service can be refused outright: no POST_NOTIFICATIONS on some
            // OEM builds, or a background-start restriction. The app must keep working with
            // the mesh foreground-only rather than crashing on launch.
            Log.w(TAG, "could not enter the foreground; the mesh will run foreground-only", e)
            isRunning = false
            pendingStart = false
            stopSelf()
            return START_NOT_STICKY
        }

        if (intent?.action == ACTION_STOP) {
            // The contract above is now honoured, so it is safe to go away.
            stopForeground(STOP_FOREGROUND_REMOVE)
            isRunning = false
            stopSelf()
            return START_NOT_STICKY
        }
        return START_STICKY
    }

    override fun onDestroy() {
        isRunning = false
        pendingStart = false
        // The radio is owned by MeshHost and deliberately NOT stopped here. The service ends
        // when the app does; tearing the radio down on every service restart would drop
        // every live link for no reason.
        super.onDestroy()
    }

    private fun createChannel() {
        val manager = getSystemService(NotificationManager::class.java) ?: return
        if (manager.getNotificationChannel(CHANNEL_ID) != null) return
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                getString(R.string.mesh_channel_name),
                // LOW: no sound, no heads-up. It is a status line, not an alert, and an
                // always-present notification that pings would be worse than useless.
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = getString(R.string.mesh_channel_description)
                setShowBadge(false)
            },
        )
    }

    private fun buildNotification(): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).apply {
                flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
            },
            PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.mesh_notification_title))
            .setContentText(getString(R.string.mesh_notification_text))
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setContentIntent(open)
            .setOngoing(true)
            .setShowWhen(false)
            .build()
    }

    companion object {
        private const val TAG = "nimmesh.service"
        private const val CHANNEL_ID = "nimmesh.mesh"
        private const val NOTIFICATION_ID = 1

        /**
         * Whether the relay is actually running in the background, surfaced through
         * `meshStatus`. Reported rather than assumed: a foreground service can be refused
         * outright on some OEM builds, and claiming it is relaying when it is not is exactly
         * the class of lie this app tries not to tell.
         */
        @Volatile
        var isRunning: Boolean = false
            private set

        /** A start has been asked for but `onStartCommand` has not run yet. */
        @Volatile
        private var pendingStart: Boolean = false

        private const val ACTION_STOP = "com.nimmesh.app.action.STOP_MESH"

        /**
         * Start relaying in the background. Safe to call repeatedly; safe to call when the
         * service is already running.
         *
         * Never call this from the background: since Android 12 a background start throws
         * `ForegroundServiceStartNotAllowedException`. Every caller here is an Activity.
         */
        fun start(context: Context) {
            // ⚠ Not just an optimisation. `startForegroundService` opens a CONTRACT: the
            // service must call `startForeground` within about five seconds or the system
            // kills the app with ForegroundServiceDidNotStartInTimeException. A redundant
            // start queues a second contract that a subsequent stop can strand, and the
            // crash lands later, somewhere unrelated.
            if (isRunning || pendingStart) return
            pendingStart = true
            try {
                context.startForegroundService(Intent(context, MeshService::class.java))
            } catch (e: Exception) {
                // Refusable outright: a background start since Android 12, or an OEM policy.
                // The mesh then runs foreground-only, which is worse but not broken.
                Log.w(TAG, "foreground service start refused; the mesh runs foreground-only", e)
                pendingStart = false
            }
        }

        /**
         * Stop relaying.
         *
         * ⚠ Deliberately NOT `stopService`. Calling that while a `startForegroundService`
         * contract is still pending destroys the service before it can call
         * `startForeground`, and the system responds by KILLING THE APP with
         * ForegroundServiceDidNotStartInTimeException. It is a real hazard, not a test
         * artifact: `MainActivity.onResume` stops the relay when the Bluetooth permissions
         * have been revoked, which can land moments after a start.
         *
         * Routing the stop THROUGH the service means the contract is always honoured first,
         * and only then does it take itself down.
         */
        fun stop(context: Context) {
            if (!isRunning && !pendingStart) return
            try {
                context.startForegroundService(
                    Intent(context, MeshService::class.java).setAction(ACTION_STOP),
                )
            } catch (e: Exception) {
                Log.w(TAG, "stopping the mesh service", e)
            }
        }
    }
}
