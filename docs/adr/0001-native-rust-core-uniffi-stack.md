# ADR-0001 — Native Swift + Kotlin sharing a Rust core via UniFFI

**Status:** accepted (2026-06-26) · **Context:** the founding stack decision for nimiq.bitmesh.

## Decision

Build native **iOS (Swift, CoreBluetooth)** and **Android (Kotlin, `android.bluetooth.le`)**
apps that share **one Rust core crate** exposed through **UniFFI**. The Rust core owns
all safety- and protocol-critical logic; each platform contributes only a thin BLE
transport shim and UI.

## Why (the hard requirement)

The non-negotiable is a **true multi-hop mesh**: every device must act as BLE
**central *and* peripheral simultaneously** (scan + advertise + relay) on **both** iOS
and Android, and keep relaying in the background. Only native CoreBluetooth /
`android.bluetooth.le` give first-class concurrent central+peripheral and real
background BLE lifecycle (iOS `UIBackgroundModes` + state restoration; Android
foreground service). This is the architecture **Bitchat itself proved** (native
CoreBluetooth on iOS, protocol-compatible Kotlin on bitchat-android).

A single Rust crate makes the high-value logic — Nimiq signing, packet codec,
TTL/hop relay, dedup, GCS store-and-forward, Noise sessions — **headless
unit/property/fuzz-testable in CI**, identical across both platforms, with no
WASM/JS bridge in the hot path. (It is the same language `@nimiq/core`'s WASM is
compiled from, so signing is byte-identical.)

**Scorecard:** this stack **24/25** · Flutter 19 · React Native 15 · Rust-btleplug 11.

## Alternatives rejected

- **React Native / Expo** — `react-native-ble-plx` is central-only; peripheral needs a
  weakly-maintained, non-co-designed module with a poor iOS GATT path; the JS thread
  suspends in background (relay dies); and `@nimiq/core` WASM won't run in Hermes — you'd
  need a native Rust signer anyway, defeating the "stay in JS" rationale.
- **Flutter** — second-best; two uncoordinated plugins (`flutter_blue_plus` +
  `ble_peripheral`); limited iOS background peripheral; the Dart isolate suspends in
  background. Same iOS limits with less control.
- **Rust core via btleplug** — `btleplug` is central-only; `ble-peripheral-rust` is
  peripheral-only; `bluster` is stale. Desktop-first, no mobile background BLE / app-
  extension support. (We keep the Rust *core*, just not its BLE layer.)
- **Web PWA** — Web Bluetooth has **no peripheral/advertise** capability → true mesh is
  impossible. Andjroo has ruled it out; OK to diverge from the web fleet here.

## Key libraries

- **iOS:** CoreBluetooth (`CBCentralManager` + `CBPeripheralManager`), CryptoKit
  (Ed25519 fallback), URLSession (gateway POST).
- **Android:** `android.bluetooth.le` (scanner+GATT central, advertiser+GATT server),
  Nordic `no.nordicsemi.android:ble` or BLESSED-Kotlin to tame GATT, OkHttp,
  `connectedDevice` foreground service.
- **Rust core:** `nimiq-keys` / `nimiq-transaction-builder` / `nimiq-serde` /
  `nimiq-primitives` (core-rs-albatross) **or** portable `ed25519-dalek` + a ported
  deterministic serializer (the `sendhome` `wire.ts`/`hex.ts` template, proven
  byte-exact); `snow` (Noise_XX_25519_ChaChaPoly_SHA256); custom packet codec + LRU
  deduper + GCS gossip-sync.
- **Bindings/build:** UniFFI + uniffi-bindgen; `cargo-swift` → iOS `.xcframework`;
  `cargo-ndk` → Android `.so` + Kotlin bindings.

## Consequences

- **Cost:** a two-toolchain tax (Xcode + Android) and a UniFFI build step. Accepted —
  it is the price of a real mesh that survives backgrounding.
- **Honest background limit:** when an iOS app backgrounds, its local name drops and
  service UUIDs move to Apple's 128-bit overflow area — discoverable only by another
  iOS device scanning that exact UUID. **iOS-background → Android-central is a dead
  spot.** Mitigations: CB state restoration, `bluetooth-central`/`bluetooth-peripheral`
  background modes, the overflow-area discovery path, an Android foreground service,
  and the Bitchat "keep ≥1 device foregrounded" UX. (See `needs:owner`.)
- **Build/test home:** the Rust core is CI-gated headless; native app builds run on
  CI macOS/Linux runners; **on-device mesh interop is Andjroo's Mac + real phones.**
