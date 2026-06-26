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
| G3  | Offline Nimiq signing core (TESTNET) — `signOffline()`      |   **yes**  |   **no**   | G1            | todo   |
| G4  | bitmesh wire protocol + packet codec (pure Rust)           |     no     |    yes     | G1            | ✅ done |
| G5  | BLE mesh transport — concurrent central+peripheral (iOS+Android) | no   |    yes     | G2, G4        | 🟡 Rust core done · native shim pending (Xcode/Apple ID) |
| G6  | Relay engine — TTL/hop-cap + dedup + degree-adaptive + frag |     no     |    yes     | G4, G5        | ✅ done |
| G7  | Store-and-forward — GCS gossip-sync catch-up               |     no     |    yes     | G6            | ✅ done |
| G8  | Gateway broadcast node (TESTNET) — `sendRawTransaction`     |   **yes**  |   **no**   | G3, G7        | todo   |
| G9  | Head-beacon + validity-window guard + packet GC            |     no     |    yes     | G4, G8        | todo   |
| G10 | Wallet + UI — keygen/import, address validation, pending→settled | **yes** |  **no**   | G3, G8        | todo   |
| G11 | Optional encrypted memo / chat (Noise XX)                  |     no     |    yes     | G5, G6        | ✅ done |
| G12 | Hardening — verify-before-relay, rate limits, NACK, anti-spam | **yes** |  **no**   | G8, G10       | todo   |
| G13 | TESTNET end-to-end demo + mainnet-gating doc               |   **yes**  |   **no**   | G8,G9,G10,G12 | todo   |

\* G1 landed the **Rust core crate + UniFFI scaffolding + a green local `cargo test`**
(the native iOS/Android app targets are deferred to G5, when their toolchains land).
Like every non-money-path goal it **auto-merges on green** (build + an independent
verify agent); Andjroo reviews post-hoc on GitHub. Money-path goals still stop for Andjroo.

**MVP (testnet):** G1–G10 + G12–G13. G11 is an enhancement.

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
