# nimiq.bitmesh — Autonomous Build Loop

> The standing contract the self-paced build loop reads at the start of every cycle.
> **GitHub issues + PRs are the single source of truth for progress; this file is the
> map.** If the loop is interrupted, resume from the open issue/PR state with no lost
> work. North star + values live in [GOAL.md](./GOAL.md); the wire format in
> [PROTOCOL.md](./PROTOCOL.md); the hazards in [RISKS.md](./RISKS.md).

## North star (one line)

Sign a Nimiq transaction with no internet; relay it phone-to-phone over a true BLE
mesh until one device with a connection broadcasts it. Testnet-first, money-path-gated.

## Stack (see [ADR-0001](./adr/0001-native-rust-core-uniffi-stack.md) + [ADR-0002](./adr/0002-ble-layer.md))

- **Shared Rust core** (one crate) owns everything safety- and protocol-critical:
  Nimiq signing (Ed25519 + deterministic serializer), the bitmesh packet codec,
  TTL/hop relay, LRU dedup, GCS gossip-sync store-and-forward, Noise_XX session crypto,
  gateway broadcast queue, and the `MeshNode` orchestrator. **No WASM, no consensus
  client.** Headless unit/property/fuzz tested. ~95% of the code is pure Rust.
- **BLE layer = thin native radio shim** (ADR-0002, decided on merit, 21/25 vs 11/25):
  the radio stays native; Rust owns everything above the byte-stream seam. Wired via
  **UniFFI foreign traits** — Rust calls out to a `BleRadio` trait (start_advertising /
  start_scanning / send / disconnect) the shim implements; the shim calls in to
  `MeshNode` on every BLE event (on_packet_received / on_peer_* / on_send_result). The
  radio never sees a TTL; the brain never sees a `CBPeripheral`. **Radio-in-Rust
  (objc2/JNI/btleplug) is rejected** — background survival is OS-governed (no Rust gain)
  and Android's ART has no JNI `DefineClass` (a Kotlin shim is forced regardless).
- **iOS app** — Swift + CoreBluetooth (`CBCentralManager` + `CBPeripheralManager`
  concurrently), consuming the core as an `.xcframework` (cargo-swift / UniFFI). ~600–900 LOC.
- **Android app** — Kotlin + `android.bluetooth.le` (scanner+GATT central and
  advertiser+GATT server concurrently), consuming the core as a `.so` + Kotlin
  bindings (cargo-ndk / UniFFI). ~800–1100 LOC.
- Protocol ported from **Bitchat** (`permissionlesstech/bitchat`, **The Unlicense /
  public domain** — free to port, no attribution/copyleft obligation).

## Operating model — autonomy & gating

**Auto-merge non-money-path PRs when CI is fully green. Gate the money path.**

- **Money-path goals are PR-only behind Andjroo** (`auto-merge: no`, label
  `money-path` + `needs:owner`): anything that signs, handles keys/seed, or
  **broadcasts** a transaction. They default to **Nimiq testnet** (networkId=5).
- **Mainnet is flipped only by Andjroo** — never by the loop. No real-fund, mainnet
  RPC, or store-distribution action without explicit approval.
- **Non-money-path goals auto-merge** when green (scaffold, codec, relay, store-and-
  forward, transport plumbing, mock harness, head-beacon, encrypted-memo transport).

### Toolchain — build locally, Rust-first

The stack was chosen **on merit** (a true mesh needs native BLE; Rust owns the brain),
**not** on what the Mini happened to have. Toolchains are installed as the work needs
them, and the loop builds + verifies **locally**:

- **Rust is installed on the Mini** (rustup stable + clippy + rustfmt). The **Rust core
  is the green gate, built + tested LOCALLY**: `cargo test` + `cargo clippy -D warnings`
  + `cargo fmt --check` + property/fuzz tests + the full mock pay-loop end-to-end. No CI
  round-trip needed for core work (G1–G4, G6–G9, and the logic of G3/G8).
- **GitHub Actions CI is a backstop + the cross-platform check** (the gh token has
  `workflow` scope): it re-runs the core gate on Linux, and once native targets exist
  runs the `ios` job (macos runner: `.xcframework` + `xcodebuild`) and `android` job
  (`cargo ndk` + `./gradlew assembleDebug`).
- **The Mac Mini is the primary native build host** (ADR-0002): at G5 it gets full
  **Xcode + Android SDK/NDK/JDK** + the seven Rust cross-targets, and each cycle builds
  the Rust core, the **unsigned iOS `.xcframework`**, the **installable Android debug
  APK** (auto-signed → fully unattended), and **compile-gates** the iOS device build
  (`CODE_SIGNING_ALLOWED=NO`). GitHub Actions macOS stays a **metered backstop** gated to
  native-touching PRs. None of this is needed for the Rust core (G1–G4). *One-time at G5:*
  installing Xcode needs **Andjroo's Apple ID** + a `sudo xcodebuild -license accept`
  (`needs:owner`); the Android SDK installs unattended.
- **iOS signing / device installs / TestFlight / Play-release** require Andjroo's Apple
  Developer account and **on-device BLE mesh interop** (iOS↔iOS, iOS↔Android,
  Android↔Android — incl. the background overflow-UUID dead spot) needs **real phones**:
  all `needs:owner`. The loop hands Andjroo a "what to test" note each native cycle.

## Goals — worked top-down, one PR per goal

Scaffold + mock harness + codec first (all CI-testable headless), then the money path.
`money-path ⇒ auto-merge:no`.

| #   | Goal                                                        | money-path | auto-merge | deps          | status |
| --- | ---------------------------------------------------------- | :--------: | :--------: | ------------- | ------ |
| G1  | Scaffold + shared Rust core skeleton + dev-build + CI       |     no     |    yes*    | —             | ✅ done |
| G2  | Provider seam + `MockMeshTransport` (full pay loop in CI)   |     no     |    yes     | G1            | ✅ done |
| G3  | Offline Nimiq signing core (TESTNET) — `signOffline()`      |   **yes**  |   **no**   | G1            | 🟡 Rust core done (byte-exact) · native enclave/Pay-delegate pending (on-device) |
| G4  | bitmesh wire protocol + packet codec (pure Rust)           |     no     |    yes     | G1            | ✅ done |
| G5  | BLE mesh transport — concurrent central+peripheral (iOS+Android) | no   |    yes     | G2, G4        | 🟡 Rust core done · native shim pending (Xcode/Apple ID) |
| G6  | Relay engine — TTL/hop-cap + dedup + degree-adaptive + frag |     no     |    yes     | G4, G5        | ✅ done |
| G7  | Store-and-forward — GCS gossip-sync catch-up               |     no     |    yes     | G6            | ✅ done |
| G8  | Gateway broadcast node (TESTNET) — `sendRawTransaction`     |   **yes**  |   **no**   | G3, G7        | ✅ done + LIVE on-chain proof |
| G9  | Head-beacon + validity-window guard + packet GC            |     no     |    yes     | G4, G8        | ✅ done |
| G10 | Wallet + UI — keygen/import, address validation, pending→settled | **yes** |  **no**   | G3, G8        | todo   |
| G11 | Optional encrypted memo / chat (Noise XX)                  |     no     |    yes     | G5, G6        | ✅ done |
| G12 | Hardening — verify-before-relay, rate limits, NACK, anti-spam | **yes** |  **no**   | G8, G10       | todo   |
| G13 | TESTNET end-to-end demo + mainnet-gating doc               |   **yes**  |   **no**   | G8,G9,G10,G12 | todo   |

\* G1 landed the **Rust core crate + UniFFI scaffolding + a green local `cargo test`**
(the native iOS/Android app targets are deferred to G5, when their toolchains land).
Like every non-money-path goal it **auto-merges on green** (build + an independent
verify agent); Andjroo reviews post-hoc on GitHub. Money-path goals still stop for Andjroo.

**MVP (testnet):** G1–G10 + G12–G13. G11 is an enhancement.

## Phase 2 — the finish line (the app you can hold)

The protocol core above (G1–G9, G11) is **built + live-proven on testnet**. Phase 2 turns it
into a usable wallet, then layers the life-improvement mesh features. Same autonomy rule:
**auto-merge non-money-path on green; gate the money path / devices / vision.** One PR per goal,
files < 800 lines, real decisions in `docs/adr/`.

### Phase A — "an app you can hold" (autonomous, non-money-path)

| #  | Goal                                                              | money-path | auto-merge | deps    | status |
| -- | ---------------------------------------------------------------- | :--------: | :--------: | ------- | ------ |
| A1 | WebView host + read-only JS↔core bridge (`WKWebView` loads `webui/`) | no | yes | G5-core | ✅ done |
| A2 | Home polish — mobile-header fix + data via the bridge (390px diff) | no | yes | A1 | ✅ done |
| A3 | Send + Receive screens (UI; Receive fully works, Send→sign stubbed→C1) | no | yes | A1, A2 | ✅ done |
| A4 | Mesh chrome + global language pill + connect-wallet pill (selection only) | no | yes | A1 | ✅ done |

A1's bridge is **read-only** (version/network/mesh status/peer count/cached tx list) — no keys,
no signing, no broadcast. A1 is also the foundation merge of `feat/g5-ios-shell` → `main`.

### Phase B — life improvements over the mesh (autonomous, non-money-path)

Each reuses primitives already built (gateway, head-beacon, store-and-forward, receipts):
Rust-core logic (cargo-tested) + web UI (screenshot-verified). None handle keys → auto-merge.

| #   | Goal                                                          | money-path | auto-merge | deps | status |
| --- | ----------------------------------------------------------- | :--------: | :--------: | ---- | ------ |
| G15 | Balance over mesh — gateway balance query + fiat + "synced X ago"; last-known → accounts-proof | no | yes | A2, G9 | 🟡 core+FFI done · UI stamp + accounts-proof = follow-ups |
| G16 | Reachability + smart send queue ("will it send?")            | no | yes | A3 | todo |
| G17 | Settlement closure both ways (receipt → "landed" for sender + receiver) | no | yes | A3 | todo |
| G18 | Contacts + amount requests (request packet carries no keys)  | no | yes | A3 | todo |
| G19 | Backup nudge (self-custody protection)                       | no | yes | A2 | ✅ done |
| G20 | Good-citizen + battery-aware relay (stats + throttle)        | no | yes | G6 | todo |

### Phase C — money path & hardening (GATED: PR-only, `needs:owner`)

| #   | Goal                                                          | money-path | auto-merge | deps | status |
| --- | ----------------------------------------------------------- | :--------: | :--------: | ---- | ------ |
| C1  | Keygen / import + real Send→sign→queue wire (=#10 money slice; seed stays behind `EnclaveKey`) | **yes** | **no** | A3, G3 | todo |
| G12 | Hardening — verify-before-relay, rate limits, NACK, anti-spam | **yes** | **no** | G8, C1 | todo |
| G13 | TESTNET end-to-end demo + mainnet-gating doc                 | **yes** | **no** | G8,G9,C1,G12 | todo |

### Phase D — native BLE on real devices (GATED: needs Andjroo's devices + Apple Dev account)

| #  | Goal                                                              | gate | status |
| -- | ---------------------------------------------------------------- | ---- | ------ |
| G5-shim | Native BLE radio shim (iOS CoreBluetooth + Android `bluetooth.le`) implementing the `BleRadio` trait + on-device interop | needs:owner (≥2 phones, $99 Apple Dev, signing) | todo |

### Phase E — vision (research-gated, `needs:owner`)

| #   | Goal                                                          | gate | status |
| --- | ----------------------------------------------------------- | ---- | ------ |
| G21 | Incentivized mesh — reputation now (G20 stat) → staking-funded inclusion pool | needs sybil-resistant proof-of-useful-relay design | todo |

**The loop runs Phase A then Phase B autonomously, and STOPS at Phase C/D/E** (money-path,
on-device, vision) — those wait for Andjroo, batched under `needs:owner`.

## Per-cycle workflow (each non-trivial cycle = a dynamic Workflow)

1. Pick the top open goal whose deps are merged. Re-read this file + GOAL + RISKS.
2. Branch `feat/gN-...`. Build it. Keep every file **< 800 lines**. Record real
   decisions in `docs/adr/`.
3. **Verify locally first** — `cargo test` + `cargo clippy -D warnings` + `cargo fmt
   --check` + the file-size guard, all green on the Mini. Push the branch so CI
   re-checks on Linux (+ the `ios`/`android` job for native goals). Add tests proving
   the goal.
4. Open a PR linking the issue. Bump version + add a CHANGELOG entry.
5. **Non-money-path + green → squash-merge.** New repo is unprotected, so don't trust
   `--auto`: poll `gh pr checks --watch`, then `gh pr merge --squash` once green.
   **Money-path → leave the PR open, label `needs:owner`, stop, and report.**
6. Close the issue; flip the status cell; file any new gaps as issues; append the
   cycle log; hand Andjroo a "what to test on a real device" note when relevant.

## Guardrails — the never list

- **Never** broadcast on **mainnet**, flip `networkId` to mainnet, point at a mainnet
  RPC, or take any real-fund / store-distribution action. Testnet only; mainnet is
  Andjroo's switch.
- **Never** auto-merge a **money-path** PR (sign / keys / broadcast). Leave it for Andjroo.
- **Never** let key/seed material touch the mesh, logs, or a relay — only public,
  broadcast-safe signed bytes ride the air. Seed stays in the secure enclave.
- **Never** show "paid" before on-chain inclusion. Honour unconfirmed-until-inclusion.
- **Never** relay a packet without verifying it is a well-formed signed Nimiq tx
  (free spam filter) once G12 lands; never carry an expired tx (GC it).
- **Never** merge a red or still-running PR; **never** mark a goal done without CI proof
  (and, for native/UI/on-device, without Andjroo's device confirmation).
- **Never** introduce a second source of truth for the protocol — the Rust core is canonical.

## Reserved for Andjroo (collect under `needs:owner`, present batched, don't decide)

- **Platform priority** — iOS-first / Android-first / lockstep (bites at G5, not before).
- **Key origin** — enclave-stored in-app seed vs delegate to Nimiq Pay / Hub / Keyguard;
  and OK'ing a verified "sign-but-DON'T-broadcast" path on a real device (#1 unknown).
- **All money-path PRs** (G3, G8, G10, G12, G13) + the **first mainnet broadcast** authorization.
- **Branding / domain / TLD**, App Store + Play provisioning, and store-review posture
  (TestFlight/internal-only until the testnet demo is proven?).
- **Background-mesh UX honesty** — OK to ship the "keep one device foregrounded" pattern
  (iOS-background→Android-central is a hard dead spot)? Invest in the iOS-26 Live Activity path?
- **Xcode install at G5** — needs Andjroo's **Apple ID** + a one-time `sudo xcodebuild
  -license accept` on the Mini. (Rust ✅ installed; Android SDK installs unattended.)

_Resolved on merit (no longer pending): Rust toolchain installed on the Mini; BLE-layer
architecture = thin native shim (ADR-0002); native build host = Mac Mini primary (ADR-0002)._

## Milestone — LIVE testnet proof (2026-06-26)

**A transfer signed by `bitmesh-core` settled on the real Nimiq Albatross testnet.**
G3 signer → G8 `HttpGatewayRpc::send_raw_transaction` → confirmed in block **4428402**
(networkId 5, 139-byte basic transfer, fee 0, `executionResult=true`, 248 confirmations).
tx `9be04b74c02c277de2c77ae11e8f0069fb8387cb24c8d609cc6b1da9d0e5c570` —
[explorer](https://nimiq-testnet.observer/transactions/9be04b74c02c277de2c77ae11e8f0069fb8387cb24c8d609cc6b1da9d0e5c570).
Proves our byte-exact serializer produces transactions the live network accepts.
(Honest caveat: the chain proves a valid tx settled; chain-data-alone can't attribute
the signer — but our live example drives our core end-to-end for every money-critical step.)

## Cycle log

- **2026-06-26** — Loop initialized. Ran the `bitmesh-design-spike` dynamic workflow
  (4 research agents + synthesis). Confirmed empirically: signed basic transfer = 139 B;
  Nimiq single-sig = RFC-8032 Ed25519 (verifies under WebCrypto); Bitchat = Unlicense
  (portable); validity window = 120 batches × 60 blocks ≈ **2 h** (the mesh relay budget).
  Picked the stack (ADR-0001), wrote GOAL/LOOP/PROTOCOL/RISKS, filed G1–G13 + the
  `needs:owner` decisions issue. Next: G1 scaffold.
- **2026-06-26** — Installed Rust (rustup stable) on the Mini → build + gate locally.
  **G1 merged** (PR #15): `bitmesh-core` Cargo workspace + UniFFI proc-macro surface
  (`core_version`/`NetworkId`/`echo_bytes`, 5 tests) + `uniffi-bindgen` (Swift+Kotlin
  bindings generate, no Xcode/Android) + size-guard + CI `core` job — all green, verify
  passed. Ran the `bitmesh-ble-layer-decision` workflow → **ADR-0002** (thin native radio
  shim, Mac Mini primary build host; both decided on merit). Building G2 (mock pay-loop
  harness) + G4 (wire codec) in parallel isolated worktrees. Next wall: G3 (money-path).
- **2026-06-26** — **G4 merged** (PR #16, v0.2.0): wire packet codec + Nimiq TLV envelope,
  28 tests incl. 5 proptests. **G2 merged** (PR #17, rebased onto G4): provider seam
  (`MeshTransport`/`MeshGateway`/`MeshProvider kind:mock|real`) + `MockMeshTransport` +
  the end-to-end origin→relay→gateway→receipt mock pay-loop. Core now at **37 unit + 5
  proptests, all green**. Non-money-path runway left before Andjroo: G5-Rust-core (BleRadio
  trait + MeshNode + MockRadio, wiring G4 codec into the G2 seams) → G6 (relay) → G7
  (store-and-forward). Walls: **G3** (offline signing, money-path → needs key-origin call)
  and **G5 native shim** (needs Xcode on the Mini = Andjroo's Apple ID).
- **2026-06-26** — **G5 Rust core merged** (PR #18, v0.3.0): the ADR-0002 radio model —
  `BleRadio` foreign trait + `MeshNode` + `MockRadio` virtual-mesh harness + `engine.rs`
  relaying **real G4 codec packets** (NimiqTx 0x30 + TLV + receipt 0x31). G2's mock
  framing removed. **50 unit + 5 proptests green**, incl. tests for all four ADR-0002
  UniFFI gotchas (non-blocking receive via thread-id spy, fire-and-forget outcomes,
  panic-surviving worker, weak-edge teardown). Money-path still zero — txWire opaque
  throughout. Issue #5 stays **open** for the native Swift/Kotlin shim (Apple ID gate).
  Loop continues: G6 (relay refinements) → G7 (store-and-forward) → G11 (encrypted memo),
  then the money-path/native walls.
- **2026-06-26** — **G6 merged** (PR #19, v0.4.0): relay refinements in new `relay.rs` +
  `fragment.rs` — degree-adaptive probabilistic flood (threshold 6, injectable seeded
  RNG), injectable relay jitter (10–220 ms `RealDelay` in prod / `NoDelay` in tests),
  **source-link exclusion** (was missing in G5), and `fragment=0x20` split + bounded
  reassembler (128/30 s, TTL-zeroed). **76 tests green** incl. property tests for
  loop-freedom, dedup-at-most-once, sparse-tree reachability, and a fuzzed
  split→reassemble round-trip — all deterministic (ran twice byte-identical). Next: G7.
- **2026-06-26** — **G7 merged** (PR #20, v0.5.0): store-and-forward GCS gossip-sync —
  `gcs.rs` (BIP158-style Golomb-Rice filter, no false negatives, empirical fpr ~0.01,
  ≤400 B) + `store_forward.rs` (bounded clock-free RecentCache 1000/15 min + 30 s
  `SyncScheduler`). `requestSync` (0x21, ttl 0, local-only) advertises a GCS filter; a
  peer unicasts back only the packets it lacks, flagged `isRSR` (0x10, delivered locally,
  no re-flood). **93 tests** incl. a real offline→rejoin→catch-up (12 missed packets
  recovered, idempotent re-sync) + gap-only + rate-limit. Launching **G11** (encrypted
  memo/chat, Noise XX) — the last non-money-path goal before the walls.
- **2026-06-26** — **G11 merged** (PR #21, v0.6.0): `noise.rs` — Noise_XX_25519_ChaChaPoly
  _SHA256 (snow) mutual-auth handshake, a Curve25519 transport identity (SEPARATE from any
  wallet seed) + SHA-256 fingerprint, two ChaChaPoly states + a 1024-msg RFC-6479 replay
  window; `encMemo` (0x05) + `noiseEncrypted` (0x11) wired; relayed opaquely. **107 tests**
  incl. handshake, memo+chat round-trip, replay-rejection, wrong-key + tamper failure.
  ⏸️ **LOOP PAUSED.** Every non-money-path goal is done (G1/G2/G4/G5-core/G6/G7/G11).
  Remaining are all **Andjroo-gated**: money-path **G3** (offline signing — needs the
  key-origin decision), **G8/G10/G12/G13**, and the **G5 native shim** (needs Xcode on the
  Mini = Andjroo's Apple ID). G9 (head-beacon/GC) is coupled to the G8 gateway. The loop
  will resume on Andjroo's go.
- **2026-06-26** — **G3 built (PR pending Andjroo, v0.7.0): offline Nimiq signing (TESTNET),
  MONEY-PATH — DO NOT auto-merge.** Andjroo's key-origin call answered: a pluggable
  `KeyOrigin` seam (`src/nimiq/`) with **both** origins — `AppSigner` (offline-first;
  seed stays behind the `EnclaveKey` `with_foreign` trait → Secure Enclave / Keystore;
  **only pubkey + 64-B signature cross FFI**, never the seed) and `DelegatedSigner` (a
  `with_foreign` Nimiq Pay / Hub seam returning a pre-signed blob). Byte-exact Albatross
  signer: `serializeContent` (67 B) + 139-B `Basic` wire + Blake2b-256 hash + 98-B
  `SignatureProof` + `NQ`-IBAN address codec, **proven equal to `@nimiq/core` v2.7.0
  byte-for-byte** against 4 committed fixtures (generator: `scripts/fixtures/`). Confirms
  `ed25519-dalek` == `@nimiq/core`. `MeshNode::submit_signed_transfer` floods the `raw_hex`
  as opaque bytes through the existing mesh path. **No broadcast / no RPC / no networking**
  (G8 owns that). **115 tests** (107 lib + 8 new across tx/address/signer + the fixture
  acceptance suite), fmt + clippy + size-guard green. Awaiting Andjroo's review/merge; the
  native enclave + Nimiq Pay SDK sign-but-don't-broadcast paths are verified on-device later.
- **2026-06-26** — **G3 merged** (PR #22, Andjroo-authorized) → then **G8 merged** (PR #24,
  v0.8.0): gateway broadcast (`rpc.rs` `GatewayRpc`/`HttpGatewayRpc` via `ureq` behind the
  `gateway-rpc` feature + testnet guard; `RpcGateway` wired into the engine). **136 tests**,
  hermetic. **LIVE on-chain proof:** our core signed a testnet transfer + our gateway
  broadcast it → confirmed in block **4428402** (tx `9be04b74…d0e5c570`). #8 closed.
  Building **G9** (head-beacon + validity-window GC) — the last non-money-path core goal.
  After G9: only money-path (G10/G12/G13) + the **G5 native shim** (Apple ID, tonight w/ Andjroo).
- **2026-06-26** — **G9 merged** (PR #25, v0.9.0): head-beacon (`beacon.rs`) — gateways
  flood `nimiqHeadBeacon 0x32 {height,blockHash,networkId}` (rate-limited, reuses G8's
  read-only `block_number`); every node keeps a monotonic `HeadCache`; `anchored_intent()`
  refuses to pre-date and stamps `validityStartHeight` = freshest heard head; the engine
  drops/GCs txs past their validity window. **149 tests**, deterministic, non-money-path.
  #9 closed. ⏸️ **AUTONOMOUS CORE COMPLETE** — the full protocol (signing + wire codec +
  mesh node/radio + relay + store-and-forward + gateway broadcast + head-beacon/validity-GC
  + encrypted memo) is built, tested, and **live-proven on testnet**. Loop **paused**:
  everything left — **G10 wallet/UI**, **G5 native BLE shim** (Apple ID), **G12 hardening**,
  **G13 demo** — is the native session **tonight with Andjroo**.
- **2026-06-26** — **Phase 2 roadmap cut + loop resumed.** Andjroo: "create clear goals and a
  loop to finish them." Re-cut all remaining work into Phase A (app-you-can-hold: A1 WebView
  host + bridge → A2 home polish → A3 send/receive → A4 mesh chrome + language/connect pills),
  Phase B (life features G15–G20), Phase C (money path C1/G12/G13, gated), Phase D (G5 native
  shim, devices), Phase E (G21 vision). Found `feat/g5-ios-shell` still ships the **rejected
  hand-built SwiftUI HomeView** — the WebView pivot was never wired; A1 fixes that and is the
  foundation merge. Loop runs A+B autonomously, parks C/D/E for Andjroo. Starting **Cycle 1 = A1**.
- **2026-06-27** — **A1 merged** (PR #37, v0.10.0, #33 closed): the iOS app now hosts the real
  `nimiq-ui` web layer in a `WKWebView` (`WebHostView.swift`) bridged to the Rust core; the
  rejected SwiftUI `HomeView`/`Theme` are deleted. Read-only `bitmesh` JS bridge
  (version/network/meshStatus) — no keys/sign/broadcast, non-money-path. **Proven on the iOS 26.5
  simulator:** WebView renders the wallet UI + the mesh bar shows `core 0.10.0` sourced from
  `coreVersion()` through the bridge (JS↔Swift↔Rust round-trip on device). `nq lint` 0 errors;
  CI `core (rust)` green; iOS gated locally (`xcodebuild` BUILD SUCCEEDED). Next: **Cycle 2 = A2**
  (mobile header layout + data via the bridge; the `nq lint` "Mesh Wallet cut off" warning is A2's).
- **2026-06-27** — **A2 done** (#34): mobile-header polish. Root cause: the account-header is the
  wallet's **1440px desktop** component (48px side padding, 90px identicon, 24px type) crammed into
  390px → "Mesh Wallet" truncated to "M.". Fix re-scales the component's **own layout vars** for
  mobile (12px padding, 48px identicon, 22/14px type), fades the chunked address with the
  component's mask idiom (no hard ellipsis), and stacks the actions row (full-width search + 50/50
  Send/Receive). **Verified on the iOS 26.5 simulator at device width:** full "Mesh Wallet" label,
  legible balance/fiat, faithful Nimiq mobile home. `nq lint` 0 errors ("Mesh Wallet cut off" gone).
  Honest scope: real balance/address/tx data binds when there's a wallet (C1 keys) + mesh (Phase D);
  the displayed values are still demo. Next: **A3** (Send + Receive screens).
- **2026-06-27** — **A3 done** (#35): Send + Receive screens, built against **authentic live
  testnet-wallet captures** (a reusable Playwright capture pipeline now lives in nimiq-branding-cli
  + logged-in references). Per the verification finding, **Send/Receive moved to a bottom action
  bar** (Receive | Send | scan) matching the real mobile wallet, with bitmesh's **mesh status line
  right above it**. **Receive NIM** sheet = identicon + 3×3 Fira-Mono address grid (`address-display`)
  + "Create request link" + a real Nimiq-blue **QR** (`qr-creator`, on demand). **Send Transaction**
  sheet = Contacts + recent-identicon row + "ENTER ADDRESS" 3×3 input grid (auto-advancing) +
  "Create a Cashlink" — **compose-only; the sign+queue is a STUB** ("Signed offline, then relayed
  over the mesh. Signing arrives next.") → the actual signing is the gated money-path **C1**.
  Verified at 390px (playwright) + on the iOS 26.5 simulator; `nq lint` 0 errors. Non-money-path →
  auto-merge. Next: **A4** (mesh chrome + language pill + connect-wallet), then Phase B.
- **2026-06-27** — **A4 done** (#36) → **PHASE A COMPLETE** (A1–A4). Added the fleet chrome from
  the shared **nimiq-app-shell**: a global **language pill** (`mountLanguagePill`) + a
  **connect-wallet pill** (`mountWalletPill` over `createWallet` → Hub delegate; **selection only,
  no keys cross to us** → non-money-path; real signing via the delegate is C1). Loaded via a
  **graceful dynamic `import()` from jsDelivr** (fleet-standard) so an offline failure just hides
  the pills — the core mesh UX keeps working. **Mesh chrome:** the mesh status line is now
  **always visible** with a mesh-nodes glyph ("Bluetooth mesh · offline-ready" by default; the
  bridge enriches it to "mesh <state> · N nearby · <net> · core X" on device) — the unmistakable
  "this is the offline mesh" cue. **Verified the CDN import works in the real WKWebView** (device
  screenshot: both pills loaded + live mesh line). `nq lint` 0 errors, no overflow. Auto-merged.
  ⏸️ **Phase A done — the app you can hold is built.** Next: **Phase B** (G15 balance-over-mesh
  first), all autonomous; Phase C/D/E remain gated for Andjroo.
- **2026-06-27** — **G15 balance-over-mesh — core + FFI done** (v0.11.0, non-money-path). Andjroo's
  feature: get a balance with no internet by asking the mesh. New `balance.rs` (wire codecs for
  `nimiqBalanceQuery 0x33` + `nimiqBalanceResponse 0x34` + a clock-free per-address `BalanceCache`,
  monotonic by head height); `MeshGateway::balance_of` answers via the existing read-only
  `get_account` (RpcGateway) / a test value (MockGateway); engine `handle_balance_query`/
  `handle_balance_response` + `flood_local_balance_query`; node FFI `query_balance` /
  `cached_balance`. **146 tests** incl. 2 e2e mesh round-trips (query→gateway-answer→cache;
  no-balance→no-answer). Built as the two-part increment: part 1 = wire format + cache (PR'd as the
  first commit), part 2 = gateway/engine/FFI + e2e. **Honest scope:** balance is unverified/
  last-known (untrusted relay) until a trustless **accounts-proof** (follow-up); the UI **fiat +
  "synced X ago"** stamp lands when a `MeshNode` runs in-app (native shim, **Phase D**) — the FFI is
  ready. Two banner UI fixes also merged this session (#41 component swap, #42 grey container).
  Next autonomous: **G19** (backup nudge) / **G20** (good-citizen relay) — pure logic/UI, no node needed.
- **2026-06-27** — **G19 done** (#30, v0.12.0, non-money-path → auto-merge): the backup nudge. A
  self-custody offline wallet has no "forgot password", so the prompt must be persistent +
  proportionate. New `backup.rs` = `backup_urgency(BackupState) -> BackupUrgency` — a pure, **clock-free,
  key-free** policy over public facts only (`backed_up`, `balance_luna`, `days_since_first_funds`):
  escalates `None → Gentle → Important → Critical`, the higher of a balance-driven + an age-driven
  sub-score; `None` when backed up or no funds at stake. FFI-exported; iOS bridge gains read-only
  `backupUrgency(state)`. The webui backup banner now escalates from the policy: hidden → orange
  words-on-white (escalating copy) → the component's **solid-orange "file" variant** at Critical (real
  `--nimiq-orange-bg` gradient + white inverse pill, no invented colors). **165 tests** (8 new, incl.
  monotonicity), clippy/fmt/size-guard clean, `nq lint` 0 errors, iOS BUILD SUCCEEDED, all four banner
  tiers screenshot-verified at 390px. **Honest scope:** no keys/balance in-app yet (C1 + Phase D), so
  the in-app nudge reads the displayed balance for now; the same call drives it for real once data lands.
  Next: **G20** (good-citizen + battery-aware relay).
