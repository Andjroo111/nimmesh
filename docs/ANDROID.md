# Android

The Android app: what is built, how to build it, and what is deliberately not there yet.

Epic and costing: [#38](https://github.com/Andjroo111/nimiq.nimmesh/issues/38).
Radio architecture: [ADR-0002](adr/0002-ble-layer.md).

## Why Android at all

iOS distribution has a hard ceiling that no amount of engineering moves. TestFlight build 1
is `betaReviewState: REJECTED`, an honest resubmit lands in 3.1.5(b) crypto-wallet territory
on an individual enrollment, and Ad Hoc OTA is capped at 100 registered UDIDs, each needing a
device profile fetched in Safari specifically.

None of those constraints exist on Android. An APK hosted next to the existing `/ota/` page
installs on any phone, from any browser, with no account, no cap, no review, and no per-device
registration. A mesh gets stronger with density, so the install ceiling is a product problem,
not a distribution one.

## What is built

| Slice | State |
| --- | --- |
| A0 toolchain on the Mini | done |
| A1 Kotlin bindings and a JNI gate | done |
| A2 WebView host and the bridge | done |
| A3 wallet, mnemonic, signer | not started |
| A4 network and native UI methods | not started |
| A5 the BLE radio | not started |
| A6 permissions, foreground service, Doze | not started |
| A7 packaging and distribution | not started |
| A8 two device field test | not started |

The app currently launches, renders the real wallet UI from `webui/`, and answers 18 of the
56 bridge methods out of the live Rust core. There is **no radio**, so the mesh honestly
reads `offline · 0 nearby`, and there is **no wallet**, so every wallet method rejects.

## A0: the toolchain

Installed on the Mac Mini (arm64), all pinned:

| Tool | Version | Location |
| --- | --- | --- |
| JDK | Temurin 17.0.20.1 | `~/tools/jdk-17` |
| cmdline-tools | **21.0** | `~/Library/Android/sdk/cmdline-tools/latest` |
| SDK platform | android-36 | |
| build-tools | 36.0.0 | |
| NDK | r27.3.13750724 (LTS) | |
| Gradle | 9.7.1 | `~/tools/gradle-9.7.1` |
| cargo-ndk | 4.1.2 | `~/.cargo/bin` |
| Emulator AVD | `nimmesh-test`, API 36 arm64 | |

All four Android Rust targets were already installed.

⚠ **cmdline-tools is pinned at 21.0 on purpose.** From 22.0 Google replaced the pure-Java
`sdkmanager` with a native `android` binary published for macOS as **x86_64 only**. On this
arm64 Mini it fails with `Bad CPU type in executable` and would need Rosetta, which is not
worth installing on a headless box. 21.0 is the last revision that runs on the JDK alone.

Every path lives in `android/scripts/android-env.sh`, and an already-set variable always
wins, so the same script serves the Mini and GitHub Actions. Source it; never re-derive a
path in a build script.

⚠ **`cargo-ndk` dumps the entire environment into its panic report.** A single bad flag
printed every variable in the shell, including `all-keys.env` secrets, which `~/.zshrc`
loads into every shell. Filter the output of any `cargo ndk` invocation before it reaches a
log, a paste, or a CI artifact. `android/scripts/build-core.sh` is not itself a leak, but a
failing run inside a shared log is.

## Building

```bash
# 1. The Rust core for Android, plus the Kotlin bindings generated from it.
android/scripts/build-core.sh            # release, arm64-v8a + armeabi-v7a + x86_64
android/scripts/build-core.sh --debug    # faster, much larger .so

# 2. The app.
cd android && ./gradlew :app:assembleDebug
```

Step 1 is not optional and not cached across a clean checkout: **the UniFFI Kotlin bindings
are generated from the built `.so` and are not committed**, so `:core` cannot compile without
it. That is deliberate. Generating the bindings off the library means the two can never
disagree about the FFI contract version, which is the mismatch that aborts the iOS app at
launch and is why `uniffi` is pinned to `=0.31.1` in the workspace `Cargo.toml`.

Both outputs are gitignored:

- `android/core/src/main/jniLibs/<abi>/libnimmesh_core.so`
- `android/core/src/main/kotlin/uniffi/nimmesh_core/nimmesh_core.kt` (8,181 lines)

`webui/` is synced into `android/app/src/main/assets/webui/` by the `:app` `syncWebui` task.
The source of truth stays at the repo root, shared with the iOS app.

## Gates

```bash
cd android
./gradlew testDebugUnitTest          # JVM, no device. Includes the bridge parity gate.
./gradlew connectedDebugAndroidTest  # needs a device or the emulator
```

To bring the emulator up headlessly:

```bash
. android/scripts/android-env.sh
"$ANDROID_HOME/emulator/emulator" -avd nimmesh-test -no-window -no-audio -no-boot-anim \
  -gpu swiftshader_indirect -no-snapshot &
adb wait-for-device
```

Three gates matter, and all three were mutation-checked (deliberately broken to confirm they
go red, then restored):

**`CoreFfiSmokeTest`** drives the real `BleRadio` foreign trait from Kotlin. Rust calls out
(`startAdvertising`, `startScanning`), Kotlin calls in (`onPeerConnected`, `submitLocalTx`),
and the packet the radio is handed must contain the transaction bytes verbatim. A failure
here means the Android plan is wrong, not that a string changed.

**`BridgeRoundTripTest`** drives `window.nimmesh` in a real WebView, so the JSON encoding,
the JavascriptInterface hop, the executor and the `__nimmeshResolve` callback all have to
line up. It also asserts that an unbuilt method **rejects by name** instead of resolving
something empty.

**`BridgeMethodParityTest`** reads the method list straight out of
`apple/NimmeshApp/Sources/WebHostView.swift` and fails if the Android shim has drifted.

The `android` CI job runs the JVM tests and compiles the instrumented ones. The instrumented
tests need a device, so they run on the Mini.

## Decisions

**minSdk 31.** `BLUETOOTH_SCAN` can then declare `neverForLocation` and the app never asks
for location at all. Below API 31 a BLE scan is impossible without `ACCESS_FINE_LOCATION`,
and a wallet asking for your location at install is a worse trade than the pre-12 tail.

**compileSdk 36, targetSdk 36.** Not 37: `androidx.core:core-ktx:1.19.0` demands compileSdk
37, so it was dropped instead. Nothing in the app needed it.

**AGP 9.3.1 with Gradle 9.7.1.** AGP 9 has built-in Kotlin support, so there is no separate
`org.jetbrains.kotlin.android` plugin, and applying one is a hard error. AGP 9 also rejects
`sourceSets["main"].kotlin.srcDir(...)`; the paths used here are AGP's defaults anyway.

**ABIs: arm64-v8a, armeabi-v7a, x86_64.** x86 is dead at minSdk 31; x86_64 is the emulator.

**`applicationId = com.nimmesh.app`, the same as iOS.** An application id is an identity, not
a label. Changing it makes the OS treat the next build as a different app, installing
alongside the old one and orphaning every existing install and its stored wallet. The
display name is `NIMmesh`; the identifier stays lowercase.

**Direct APK, not Play.** Play means a developer account, a review, and the same crypto
wallet policy questions that sank TestFlight.

## Traps already paid for

**`View.post()` silently never runs on an unattached view.** The bridge originally resolved
page Promises with `webView.post { ... }`. Before a view is attached to a window that only
*queues* the runnable, so every answer sat unposted and every page Promise hung forever with
nothing logged. It works inside `MainActivity` (the view is attached by `setContentView`) and
fails everywhere else, which is the worst shape a bug can have. The bridge uses a
`Handler(Looper.getMainLooper())`, which does not care about attachment.

**No `WebChromeClient` means `window.confirm()` silently returns false.** This is the exact
twin of the iOS bug where a missing `WKUIDelegate` made every confirm-gated action a no-op on
device: delete wallet, log out, the mainnet switch. Written down so it is not rediscovered a
third time.

**Android serves `webui/` over https, iOS serves it over `file://`.** `WebViewAssetLoader`
puts the page on `https://appassets.androidplatform.net`, a real origin where `fetch()`
works. On iOS, `file://` makes WKWebView block network `fetch()` outright, which is the only
reason `PolygonReads.swift` exists as a native proxy at all. Android does not need that
proxy; the bridge keeps the methods for parity so `webui/` stays one codebase.

## Not built, and the app says so

Every bridge method the page can call but Android has not built **rejects with a named
reason**, for example `walletExists is not on Android yet: A3 (wallet, mnemonic, signer)`.

This is not cosmetic. Resolving `{exists: false}` for an unbuilt wallet method would render
as "you have no wallet", which a user reads as fact. A rejection cannot be mistaken for data,
and the web layer already wraps bridge calls in `try/catch`, so it degrades quietly.

⚠ One consequence is visible today: with `walletExists` rejecting, the page skips onboarding
and renders a home screen for a wallet that does not exist. A3 fixes it. Do not read that
screen as a working wallet.

`MeshHost.UnimplementedRadio` is the same idea in the radio seam. It satisfies `BleRadio`
without touching Bluetooth so the core can be constructed and read, and it logs on every
call, so a build that reaches a device with it still installed says so in logcat instead of
quietly reporting an empty mesh.
