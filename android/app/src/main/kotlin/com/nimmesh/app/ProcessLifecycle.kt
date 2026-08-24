package com.nimmesh.app

import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.LifecycleRegistry

/**
 * A minimal always-resumed [LifecycleOwner] for CameraX to bind to.
 *
 * CameraX takes a LifecycleOwner so it can release the camera when the owner stops.
 * [QrScannerActivity] is a plain `android.app.Activity` and owns the camera for exactly as
 * long as it is on screen, so it unbinds explicitly in `onDestroy` instead. This exists so
 * that contract is satisfied without pulling in androidx.activity purely to inherit a
 * lifecycle.
 *
 * `getLifecycle()` as a method rather than an overridden `val`: CameraX 1.6 resolves
 * lifecycle-runtime 2.3.1 transitively, where `LifecycleOwner` is still a Java interface
 * with a getter. Writing it the modern way compiles against 2.8+ and not against what is
 * actually on the classpath.
 */
object ProcessLifecycle : LifecycleOwner {

    private val registry = LifecycleRegistry(this).apply {
        handleLifecycleEvent(Lifecycle.Event.ON_CREATE)
        handleLifecycleEvent(Lifecycle.Event.ON_START)
        handleLifecycleEvent(Lifecycle.Event.ON_RESUME)
    }

    override fun getLifecycle(): Lifecycle = registry
}
