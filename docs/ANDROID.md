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
| A3 wallet, mnemonic, signer | done |
| A4 network and native UI methods | done |
| A5 the BLE radio | built, on-air unproven |
| A6 permissions, foreground service, Doze | not started |
| A7 packaging and distribution | not started |
| A8 two device field test | not started |

The app launches, runs onboarding, creates or imports a real wallet, derives its `NQ`
address, reads its balance and history from the chain, sends online, scans a QR code, shares,
and gates its recovery words behind the device unlock. It answers 42 of the 56 bridge methods
out of the live Rust core.

The radio is now built and both BLE roles start on a real Bluetooth stack, but **no byte has
crossed the air**: that needs two Android phones, and none exist yet. Until then the mesh
honestly reads `offline · 0 nearby`.

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

## A3: the wallet

A 24-word Nimiq recovery phrase, the signing key derived at `m/44'/242'/0'/0'`, ported from
`Mnemonic.swift` and `Wallet.swift`. The phrase and seed never cross the Rust FFI; only a
public key and detached signatures do, through the `EnclaveKey` foreign trait.

**Ed25519 comes from BouncyCastle, not the platform.** `java.security` gained Ed25519 in API
33 and this app's minSdk is 31, so Android 12 and 12L have none. AndroidKeyStore is no help
either: it holds EC on NIST curves and RSA, and cannot hold an Ed25519 signing key at all.

That sounds like a downgrade against iOS and is not. iOS is **not** using the Secure Enclave
for this key: `Wallet.swift` stores raw `Curve25519.Signing` bytes as a Keychain generic
password and signs in-process with CryptoKit. Both platforms provide the same property, that
the key never crosses into Rust, plus encryption at rest. BouncyCastle is used through its
low-level API and never registered as a JCE provider, because registering one collides with
the cut-down BouncyCastle Android already ships.

**PBKDF2 also comes from BouncyCastle**, for a sharper reason. The JCE `SecretKeyFactory`
API takes a `char[]`, and Android's implementation keeps only the low 8 bits of each
character. That is invisible for the ASCII English wordlist and silently wrong for a
non-ASCII passphrase, which would derive a different wallet from the same words.

**At rest:** the phrase is AES-256-GCM encrypted under an AndroidKeyStore key, hardware-backed
where the device offers it and non-exportable regardless. Only ciphertext reaches disk, and a
test asserts the words are not in the clear there.

⚠ **`walletStatus.recovered` is always false on Android, and it is not a stub.** On iOS the
Keychain survives an uninstall, so a fresh install can find a previous install's wallet and
must ask whether to keep it. Android deletes both the app's data and its Keystore key on
uninstall, so a wallet cannot outlive the app and that state genuinely cannot occur.

**The sender id is random per launch, deliberately not wallet-derived**, matching iOS. It is
the protocol identity every relay sees, and tying it to the wallet would make a user's
payments linkable across the mesh by anyone listening.

### The gate that matters most

`WalletVectorsTest` is a JVM test, so it runs in CI on every PR, and it checks three layers:

1. The official BIP39 (Trezor), SLIP-0010 ed25519 and RFC 5869 HKDF vectors.
2. **iOS ground truth.** The `m/44'/242'/0'/0'` private keys and the rendered backup codes
   were produced by running the shipping Swift code on this machine and are pinned in the
   test. Standards conformance alone would not catch a place where the two platforms are
   wrong in different ways.
3. Rejection cases, because a wallet that accepts a bad phrase is worse than one that
   accepts none.

This is the one place where a silent divergence does not surface as a crash. It surfaces as
somebody's funds sitting at an address they cannot reach. Both parity assertions were
mutation-checked: changing the coin type from 242 to 243, and changing the HKDF info label,
each turn the right test red with a message that names what happened.

`WalletDeviceTest` covers what needs a device: Keystore persistence across instances, the
ciphertext-not-plaintext check, import and delete round trips, and the real interop question,
that a BouncyCastle Ed25519 signature is accepted by `ed25519-dalek`. Proven on device at
launch too: `wallet self-test: address=NQ37 7AAH ... signedOk=true`.

## A4: network and native UI

**`HttpURLConnection`, no HTTP dependency.** iOS uses `URLSession`, the platform's own
client, so the honest twin is the platform's own client. OkHttp was tried and dropped:
`okhttp-android` 5.5 demands compileSdk 37, which Google has not published a platform for
yet, and nothing here needs more than a POST and a GET.

**CameraX plus ZXing for the scanner, deliberately not ML Kit.** ML Kit's barcode scanner
needs Google Play Services, and this app ships as a direct APK precisely so it does not
depend on anyone's store being installed. CameraX binds to a `LifecycleOwner`; the scanner is
a plain Activity that owns the camera for exactly as long as it is on screen, so it binds to
a minimal always-resumed owner and unbinds in `onDestroy` rather than pulling in
androidx.activity to inherit a lifecycle.

⚠ That owner overrides `getLifecycle()` as a **method**, not a `val`. CameraX 1.6 resolves
lifecycle-runtime 2.3.1 transitively, where `LifecycleOwner` is still a Java interface with a
getter. Writing it the modern way compiles against 2.8 and up, and not against what is
actually on the classpath.

**The framework `BiometricPrompt`, not androidx.biometric**, for the same compileSdk reason.
minSdk is 31, so the framework class is always present. A device with no biometric and no
screen lock has nothing to unlock with, so it passes through, exactly like the iOS
`canEvaluatePolicy` path: such a device is unprotected either way, and refusing would lock
the owner out of their own words.

### Offline continuity is native, and it is not a nicety

`walletBalance` and `walletHistory` cache their last GOOD answer and serve it when the chain
is unreachable, flagged `cached: true`. The RPC client **throws** on failure precisely so this
layer can tell "offline" apart from "you have nothing".

This is not hypothetical. iOS shipped the other behaviour first: a failed read returned 0 and
an empty list, and the wallet rendered as drained during a Bluetooth-only test, then cached
that emptiness. A genuinely unfunded account still reads 0 from a SUCCESSFUL call, which is a
different fact entirely.

The Android version of that path was proved during a **real outage**: `rpc.nimiqwatch.com`
was returning `HTTP 503 no available server` while these tests ran, and
`aFailedBalanceReadServesTheCacheInsteadOfPretendingTheWalletIsEmpty` passed against it.

⚠ **Live-chain tests use `assumeTrue`, never a bare `return`.** An early return reports the
test as PASSED whether or not it checked anything, which is how a network test quietly becomes
a no-op nobody notices. A skip has to be visible in the results, and during the outage above
the results correctly read `skipped="2"`.

### The price proxy earns its place differently here

On iOS the CoinGecko proxy is necessary: the page runs on `file://` where WKWebView blocks
`fetch()` outright. On Android the page has a real https origin and could call CoinGecko
itself. The proxy is kept for parity so `webui/` stays one codebase, and the whitelist is its
own justification: the coin goes straight into the URL path, so anything not on the list is
refused rather than escaped and hoped for.

## A5: the radio

`BleRadio` is a five-method foreign trait. Small surface, large implementation: a
`BluetoothGattServer` plus advertiser for the peripheral role and a scanner plus GATT client
for the central role, running at the same time, on the same UUIDs the iOS radio uses.

**What is proved, and what is not.** `bothRolesComeUpConcurrentlyOnARealBluetoothStack` runs
against the emulator's real Bluetooth stack and confirms the advertiser started, the GATT
server opened with its service and descriptor, the scanner started with a service filter, and
neither role displaced the other. That last property is the mesh, and it is the one Web
Bluetooth cannot offer.

It does **not** prove a byte crossed the air. Discovery, connection, MTU negotiation, the
notify path and relaying are unproven until two Android phones exist. Do not read the green
suite as a working mesh.

### The Android-only traps, each paid for once

⚠ **The CCCD, descriptor 0x2902, has no CoreBluetooth counterpart** and is the most common
reason an Android GATT server never delivers a notification. iOS synthesises it and
`didSubscribeTo` simply fires. On Android the server must ADD the descriptor and the client
must WRITE `ENABLE_NOTIFICATION_VALUE` into it. `setCharacteristicNotification` alone sets a
local flag and tells the remote device nothing. Miss it and connections succeed, writes
succeed one way, and the reverse direction is silently dead.

⚠ **The advertisement budget is 31 bytes and the nimmesh service UUID is 128-bit**, so
`setIncludeDeviceName(true)` pushes it over and the whole advertisement is REJECTED. Not a
theory: flipping that flag turns the dual-role test red with `adv:off`.

⚠ **The default ATT MTU is 23**, leaving 20 usable bytes against a 256-byte packet, so the
client requests 517 and discovery waits for `onMtuChanged`.

⚠ **`CALLBACK_TYPE_ALL_MATCHES` reports the same device continuously.** Without a connecting
guard every advertisement opens another GATT connection, and Android caps concurrent
connections low enough that the mesh wedges within seconds.

⚠ **Two API paths for writes and notifications.** API 33 passes the value as an argument;
31 and 32 carry it on the characteristic object, which means two writes in flight to the same
characteristic race. Everything is serialised onto one worker so they cannot be. **Both
`onCharacteristicChanged` overloads must exist** or inbound bytes are silently dropped on
exactly the versions minSdk 31 was chosen to include.

⚠ **`adv:on` is stronger evidence than `scan:on`.** The advertiser reports success
asynchronously through `onStartSuccess`, so `adv:on` means the stack accepted it.
`startScan` has no success callback at all, only `onScanFailed`, so `scan:on` means only that
the call was made and has not failed yet. Worth knowing when debugging an empty mesh.

### The link ref-counting is where the bug history is

A pair of phones forms TWO directed links under one peer id. `PeerLinks` counts them so
`onPeerConnected` fires on the first and `onPeerDisconnected` only on the last. Reporting a
peer gone when one direction flapped, while the other was still carrying traffic, crashed the
peer count to zero on iOS and made a working mesh look empty.

It is pure and free of Android on purpose, so the part with the bug history is covered by a
JVM test that runs in CI rather than only on hardware nobody has. Mutation-checked by
reintroducing the original field bug, and by removing the per-role dedup.

The link table lives on the RADIO, which outlives any node, and `liveIds` is replayed onto a
newly installed node. Without that, a node installed after a peer linked sits at zero peers
forever.

### Degrading rather than failing

- **No permission yet.** The radio is constructed at launch, before the user is asked, so its
  first `startAdvertising` and `startScanning` are no-ops. `MeshHost.onPermissionsGranted()`
  exists because nothing else would ever call them again.
- **Cannot advertise.** A real limit on part of the Android fleet
  (`isMultipleAdvertisementSupported`). Such a phone still relays and still pays as a central;
  it simply cannot be discovered, and `debugSummary` says `adv:UNSUPPORTED`.
- **Bluetooth off, or no adapter at all.** Logged and reported, never a crash.

`debugSummary` reports `perm`, `bt`, `adv`, `scan`, the discovery counters and the peer count,
because an empty mesh with no reason shown is the state that wastes an afternoon.

⚠ Every callback into Rust is wrapped: an exception crossing the FFI surfaces as
`UnexpectedUniFFICallbackError` and **aborts the process**, so a transient GATT error during a
flood burst must never become a crash.

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

When A4 lands, point `BridgeRoundTripTest.anUnbuiltMethodRejectsByNameInsteadOfFakingAnAnswer`
at whatever is still unbuilt rather than deleting it. The day nothing is left to name there is
the day the bridge is complete, and that should be a deliberate edit rather than a test that
quietly stops checking anything.

`MeshHost.UnimplementedRadio` is the same idea in the radio seam. It satisfies `BleRadio`
without touching Bluetooth so the core can be constructed and read, and it logs on every
call, so a build that reaches a device with it still installed says so in logcat instead of
quietly reporting an empty mesh.
