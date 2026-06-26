# nimiq.bitmesh — Autonomous Build Loop

> The standing contract the self-paced build loop reads at the start of every cycle.
> **GitHub issues + PRs are the single source of truth for progress; this file is the
> map.** If the loop is interrupted, resume from the open issue/PR state with no lost
> work. North star + values live in [GOAL.md](./GOAL.md); the wire format in
> [PROTOCOL.md](./PROTOCOL.md); the hazards in [RISKS.md](./RISKS.md).

## North star (one line)

Sign a Nimiq transaction with no internet; relay it phone-to-phone over a true BLE
mesh until one device with a connection broadcasts it. Testnet-first, money-path-gated.

## Stack (see [ADR-0001](./adr/0001-native-rust-core-uniffi-stack.md))

- **Shared Rust core** (one crate) owns everything safety- and protocol-critical:
  Nimiq signing (Ed25519 + deterministic serializer), the bitmesh packet codec,
  TTL/hop relay, LRU dedup, GCS gossip-sync store-and-forward, Noise_XX session crypto,
  gateway broadcast queue. **No WASM, no consensus client.** Headless unit/property/fuzz
  tested in CI.
- **iOS app** — Swift + CoreBluetooth (`CBCentralManager` + `CBPeripheralManager`
  concurrently), consuming the core as an `.xcframework` (cargo-swift / UniFFI).
- **Android app** — Kotlin + `android.bluetooth.le` (scanner+GATT central and
  advertiser+GATT server concurrently), consuming the core as a `.so` + Kotlin
  bindings (cargo-ndk / UniFFI).
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

### Toolchain reality (READ THIS — differs from the web fleet)

This Mac Mini has **no Rust, no full Xcode (CLI tools only), no Android SDK** — it
**cannot compile this app locally.** Therefore:

- **The green gate is GitHub Actions CI**, not a local build. The gh token has
  `workflow` scope, so the loop arms and maintains `.github/workflows/`.
  - `core` job (ubuntu): `cargo test` + `cargo clippy -D warnings` + `cargo fmt --check`
    + property/fuzz tests for the protocol + the full mock pay-loop end-to-end.
  - `ios` job (macos runner): build the `.xcframework` + `xcodebuild build` (and test
    where simulator-runnable).
  - `android` job (ubuntu): `cargo ndk` + `./gradlew assembleDebug` + unit tests.
- **On-device BLE mesh interop** (iOS↔iOS, iOS↔Android, Android↔Android), **TestFlight/
  Play internal builds, and look-and-feel** are verified on **Andjroo's Mac + real
  phones** — the loop cannot do these and must hand Andjroo a "what to test" note.
- The loop may **optionally** request Rust be installed on the Mini for faster core
  iteration; until then, CI is the sole gate. (Local Rust would let `cargo test` the
  core without a CI round-trip — Andjroo's call.)

## Goals — worked top-down, one PR per goal

Scaffold + mock harness + codec first (all CI-testable headless), then the money path.
`money-path ⇒ auto-merge:no`.

| #   | Goal                                                        | money-path | auto-merge | deps          | status |
| --- | ---------------------------------------------------------- | :--------: | :--------: | ------------- | ------ |
| G1  | Scaffold + shared Rust core skeleton + dev-build + CI       |     no     |    yes*    | —             | todo   |
| G2  | Provider seam + `MockMeshTransport` (full pay loop in CI)   |     no     |    yes     | G1            | todo   |
| G3  | Offline Nimiq signing core (TESTNET) — `signOffline()`      |   **yes**  |   **no**   | G1            | todo   |
| G4  | bitmesh wire protocol + packet codec (pure Rust)           |     no     |    yes     | G1            | todo   |
| G5  | BLE mesh transport — concurrent central+peripheral (iOS+Android) | no   |    yes     | G2, G4        | todo   |
| G6  | Relay engine — TTL/hop-cap + dedup + degree-adaptive + frag |     no     |    yes     | G4, G5        | todo   |
| G7  | Store-and-forward — GCS gossip-sync catch-up               |     no     |    yes     | G6            | todo   |
| G8  | Gateway broadcast node (TESTNET) — `sendRawTransaction`     |   **yes**  |   **no**   | G3, G7        | todo   |
| G9  | Head-beacon + validity-window guard + packet GC            |     no     |    yes     | G4, G8        | todo   |
| G10 | Wallet + UI — keygen/import, address validation, pending→settled | **yes** |  **no**   | G3, G8        | todo   |
| G11 | Optional encrypted memo / chat (Noise XX)                  |     no     |    yes     | G5, G6        | todo   |
| G12 | Hardening — verify-before-relay, rate limits, NACK, anti-spam | **yes** |  **no**   | G8, G10       | todo   |
| G13 | TESTNET end-to-end demo + mainnet-gating doc               |   **yes**  |   **no**   | G8,G9,G10,G12 | todo   |

\* G1 is the first scaffold PR: **open it for Andjroo's review (don't auto-merge)** so
CI is proven once; subsequent green non-money-path PRs auto-merge.

**MVP (testnet):** G1–G10 + G12–G13. G11 is an enhancement.

## Per-cycle workflow (each non-trivial cycle = a dynamic Workflow)

1. Pick the top open goal whose deps are merged. Re-read this file + GOAL + RISKS.
2. Branch `feat/gN-...`. Build it. Keep every file **< 800 lines**. Record real
   decisions in `docs/adr/`.
3. **Verify via CI** (push branch → Actions): core tests + clippy + fmt green; for
   native goals, the relevant `ios`/`android` job green. Add tests proving the goal.
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
- **Install Rust on the Mini?** (faster local core iteration vs CI-only gate.)

## Cycle log

- **2026-06-26** — Loop initialized. Ran the `bitmesh-design-spike` dynamic workflow
  (4 research agents + synthesis). Confirmed empirically: signed basic transfer = 139 B;
  Nimiq single-sig = RFC-8032 Ed25519 (verifies under WebCrypto); Bitchat = Unlicense
  (portable); validity window = 120 batches × 60 blocks ≈ **2 h** (the mesh relay budget).
  Picked the stack (ADR-0001), wrote GOAL/LOOP/PROTOCOL/RISKS, filed G1–G13 + the
  `needs:owner` decisions issue. Next: G1 scaffold.
