# Cross-Chain HTLC Swap — Integration Agenda

**Goal of this agenda:** take the cross-chain HTLC swap (NIM ⇄ BTC ⇄ USDC-on-Polygon) from
*testnet-proven, sim-wired* to *hardened and integrated into the Nimmesh app, ready to run over the
real BLE mesh* — leaving only on-device BLE and mainnet/real-funds as human gates.

- **Integration branch:** `feat/usdc-polygon` (the superset: NIM + BTC + USDC legs). Once G0 greens
  CI, this branch is merged to `main` and the loop continues on `main`.
- **Loop contract:** [`INTEGRATION-LOOP.md`](./INTEGRATION-LOOP.md) — read it before starting a cycle.
- **Companion docs:** [`SWAP.md`](./SWAP.md), [`RISKS.md`](../RISKS.md),
  [`MAINNET-GATING.md`](../MAINNET-GATING.md), [`USDC-GAS.md`](./USDC-GAS.md).
- **Provenance:** this agenda was produced from a 5-track security + architecture review of
  `feat/usdc-polygon` (2026-07-01). Findings are mapped to goals below as `S1…S6`.

---

## Security findings this agenda closes

| ID | Sev | Finding | Where | Closed by |
|----|-----|---------|-------|-----------|
| **S1** | 🔴 CRITICAL | Responder funds its BTC leg on the `FundingProof` **message** with **no on-chain check** that the initiator's NIM HTLC exists with the agreed hashlock/amount/timeout/recipient. Since the initiator knows `S` from the start, it can take the responder's BTC and never fund NIM. | `swap_node.rs:426`, `swap_coordinator.rs:264,283` | **G1** |
| **S2** | 🟠 HIGH | Settlement messages (`Propose/Accept/FundingProof/PreimageReveal/Abort`) are unauthenticated → a relay can tamper with proposed terms or inject proposals (griefing / mismatched-terms lockup even after S1). | `swap_wire.rs`, `swap_messages.rs` | **G2** |
| **S3** | 🟠 HIGH | USDC leg is a Rust behavioural model only — no Solidity contract, no Amoy deployment, no real-RPC integration test. | `swap_usdc_leg.rs` | **G5, G6** |
| **S4** | 🟡 MED | ERC-20 `approve`→`transferFrom` is front-runnable; gas-abstraction model (who pays MATIC) undecided. | `evm_abi.rs`, `USDC-GAS.md` | **G7** |
| **S5** | 🟡 MED | Discovery leaks side/amount/addresses in cleartext (ephemeral keys hide identity, not amounts); un-funded swaps hold concurrency slots (Sybil slot-jam). | `swap_intent.rs`, `swap_session.rs:164` | **G4, G8** |
| **S6** | 🟢 LOW | Mainnet nits: no BTC dust-limit check (`swap_btc_leg.rs:120`), ms→s truncation (`swap_ffi.rs:270`), CLTV y2136 `u32` (`btc.rs:144`), wrong doc comment (`nimiq/htlc.rs:61`), no confirmation-depth/reorg policy, no reveal-before-`T_B` guard. | various | **G3, G4** |

`assert_no_one_sided` and byte-exactness vs `@nimiq/core` 2.7.0 / `bitcoinjs-lib` 6.1.7 already pass —
the protocol math is real. Everything above is at the **money-path edge**, which is exactly what
"ready over the mesh with real funds" requires.

---

## Phase 0 — Green the base *(unblocks the loop; then merge superset → main)*

### G0 — Deterministic discovery-stress convergence
- **Problem:** `swap_discovery_stress_tests::many_complementary_pairs_all_discover_and_settle`
  (`:102`) fails 1/337 in CI. The docstring claims "deterministic by construction," but the body
  (`:91–105`) races a wall-clock budget (`80 × 5ms`) over a **threaded** `MeshHarness`, so it flakes
  under CI load.
- **Do:** replace the fixed clock budget with a pump-to-quiescence / bounded `wait_until` that drives
  message delivery to a fixpoint (or make `poll_sync` deliver synchronously in-test). No behaviour
  change to production code — this is test-harness determinism only.
- **Done when:** `cargo test --all` is green 10×/10× locally and CI `core (rust)` is green.
- **Then:** merge `feat/usdc-polygon` → `main` (single reviewed step), delete stale
  `feat/mesh-swap`, `feat/g4-wire-codec`, `feat/g5-core-mesh-node`. Loop continues on `main`.

---

## Phase 1 — Money-path safety core *(the security fixes — write the failing test first)*

### G1 — On-chain funding verification seam  *(closes S1 — the headline fix)*
- **Do:** before a party funds/reveals, it must verify the counterparty's HTLC **on-chain** via the
  gateway: correct hashlock `H`, `amount ≥ agreed`, `timeout` satisfies the ladder (`T_a − T_b ≥
  Δ_safe`), recipient == my claim key, and `≥ N` confirmations. Gate the responder's `fund()`
  (`swap_node.rs:426`) on a verified initiator NIM-HTLC; gate the initiator's `claim_and_reveal()`
  on a verified responder BTC/USDC-HTLC. Add a `FundingVerifier` trait behind the gateway seam so
  sim keeps using the in-memory chain and device/testnet uses the real gateway.
- **Adversarial tests (must fail before the fix):** malicious initiator sends `FundingProof` for a
  NIM tx that was never broadcast / has wrong `H` / short timeout / wrong recipient → responder MUST
  refuse to fund. Extend `swap_adversarial_tests.rs` with `assert_no_one_sided` under these.
- **Done when:** no funded-state transition is reachable from a message alone; adversarial suite green.

### G2 — Authenticated & bound proposal terms  *(closes S2)*
- **Do:** the initiator signs `Propose` over `(swap_id ‖ terms ‖ H ‖ timeouts ‖ addrs)` with its
  discovery key; the responder rejects any `Propose` whose signed terms don't match the intent it
  matched. Carry a lightweight session authenticator on subsequent settlement messages. (Funding
  authenticity is already covered by G1's on-chain truth; G2 protects *terms* pre-funding.)
- **Done when:** a tampered/injected `Propose` is rejected; a replayed `Propose` for a reaped
  swap_id can't create a fresh responder coordinator; counters attribute the drop.

### G3 — Confirmation-depth + reorg policy  *(closes part of S6)*
- **Do:** configurable per-chain min-confirmations before a leg is treated as `funded`/`settled`;
  re-verify on reorg. Wire it into the G1 verifier. NIM/BTC/USDC each get sane testnet defaults.
- **Done when:** a leg observed at depth `< N` never advances the phase; tests cover a shallow-then-deep observation.

### G4 — Ladder & liveness guards + mainnet nits  *(closes rest of S6 + part of S5)*
- **Do:** engine-level reveal-deadline guard (`reveal_and_claim` refuses / loudly flags when head is
  within `Δ_safe` of `T_b`) + telemetry; BTC dust-limit check (`≥ 546 sat`) in `swap_btc_leg.rs`;
  fix ms→s truncation in `swap_ffi.rs:270` (round up / keep ms); fix the `nimiq/htlc.rs:61` comment;
  reclaim concurrency slots for un-funded swaps faster (shorten the un-funded reap window).
- **Done when:** each nit has a regression test; ladder guard covered by a tight-margin test.

---

## Phase 2 — Make USDC/Polygon real *(owner-gated deploy)*

### G5 — Solidity HTLC contract
- **Do:** write the HTLC contract — `newSwap` (single-tx fund via `permit`/EIP-2612 or
  `transferFrom` inside `newSwap`, no separate `approve` step → closes S4's race), `withdraw(S)`
  verifying `sha256(S) == H` via the **SHA-256 precompile (0x02)** to match the cross-chain hashlock,
  `refund` after timeout, and swap-id single-occupancy. Vector-match `usdc_swap_id()` against the
  contract's own id derivation.
- **Done when:** contract compiles, unit-tested (Foundry/Hardhat), swap-id derivation matches the Rust model byte-for-byte.

### G6 — Amoy deployment + real integration tests  *(closes S3)*
- **Do:** deploy G5 to Polygon **Amoy** testnet; add `swap_usdc_integration_tests` (behind a feature +
  testnet gate) that drives a real round-trip against Amoy RPC: `newSwap` → `withdraw(S)` → and a
  separate `refund` path — signing, broadcasting, polling the receipt, asserting on-chain state.
- **Done when:** a real Amoy USDC HTLC funds, claims with `S`, and refunds; integration test green (testnet only, real-funds still gated).

### G7 — Gas-abstraction decision + implementation  *(closes S4)*
- **Do:** record the chosen model in an ADR (options in `USDC-GAS.md`: user-holds-MATIC /
  relayer+EIP-2771 / ERC-4337 paymaster / counterparty-sponsored) with its trust + griefing surface,
  then implement. Note in the ADR: a relayer can **grief/censor but not steal** (contract checks
  `_msgSender()`), so document the fallback to self-funded claim.
- **Done when:** ADR merged; chosen path implemented + tested on Amoy.

### G8 — Independent contract review *(gate before any mainnet)*
- **Do:** independent review of the deployed contract + the money-path diff. `needs:owner`. Not merged by the loop.

---

## Phase 3 — Integrate into the Nimmesh app *(the "over the mesh" delivery)*

### G9 — Discovery over UniFFI  *(OG-6)*
- **Do:** `#[uniffi::export]` the discovery API (`SwapIntent` advertise/match, `IntentMetrics`) so the
  native app can advertise and see matches. Regenerate Swift + Kotlin bindings.
- **Done when:** the app can start/stop an intent advert and read match/metric state via FFI.

### G10 — WebView ↔ Rust swap bridge  *(OG-1)*  ✅ **DONE 2026-07-10 (testnet)**
- **Do:** drive the real `SwapEngine` + discovery from the in-app web UI (the `nimiq-ui` swap screens),
  retiring the `swap_demo_server` HTTP shim. Replace `loadIntents`/`loadStats` seams with the FFI bridge.
- **Done when:** a swap can be proposed, funded, revealed, and settled entirely from the app UI (sim/testnet chain).
- **Shipped:**
  - **G10a** (PR #192, v0.66.0) — the app-facing live constructors `MeshNode::new_live_swap_initiator`
    / `new_live_swap_responder` over UniFFI, carrying the Act-2 live signer + real funding verifiers
    (testnet/Amoy pinned, C1-asserting), plus the never-strand `LiveLockBook` + `NimHtlcRefunder`.
  - **G10b** (PR #193, v0.67.0) — `SwapMesh.swift`'s `{ real: true }` path wires the live participant
    from the wallet key + derived Amoy accounts; the Swap sheet gains the honest "Real testnet coins"
    toggle + labels + the `swapMeshRefund` never-strand seam; the ipa ships `polygon-gateway`.
  - **G10c** (PR #194) — the LIVE proof: a real NIM⇄USDC swap driven end to end through those exact
    FFI constructors (`live_ffi_mesh_swap` example + `mac-node --swap-responder-live`), receipts in
    [`G10-RECEIPTS.md`](./G10-RECEIPTS.md). The G8-review money-path fixes (C1/H2/M3/M4, PR #191)
    are baked into the constructors, so the run validated the safe path.
  - **Still testnet-only:** the on-device BLE run (G12) and mainnet (Phase 4) remain human/owner gates.

### G11 — Real secret + real signer on device  *(OG-2, OG-3)*
- **Do:** replace `swap_node::sim_secret` with a CSPRNG draw; wire the real `SwapSigner`/`EnclaveKey`
  + `BtcEnclaveKey` on device so the seed never crosses FFI (only signed bytes do).
- **Done when:** signing happens in the enclave seam; `assert` no secret/seed material crosses FFI or hits logs/mesh.

### G12 — On-device swap over real BLE mesh  *(Phase D — partly human)*
- **Do:** run the full swap end-to-end on hardware — iOS↔iOS and iOS↔Android over real BLE, multi-hop,
  including a lossy/partition run. The loop prepares everything; the physical device run is a human step.
- **Done when:** a real two-device (then three-device relay) swap settles on testnet/signet over BLE.

---

## Phase 4 — Mainnet *(Andjroo-only — NOT in the loop)*
Complete the [`MAINNET-GATING.md`](../MAINNET-GATING.md) pre-mainnet checklist, get an independent
money-path review, and Andjroo's explicit written authorization. No loop cycle flips `networkId` or
touches real funds.

---

## Dependency order
```
G0 ─▶ (merge→main) ─▶ G1 ─▶ G2 ─▶ G3 ─▶ G4 ─┐
                                            ├─▶ G9 ─▶ G10 ─▶ G11 ─▶ G12(human)
G5 ─▶ G6 ─▶ G7 ─▶ G8(gate) ─────────────────┘
```
G1–G4 (safety) and G5–G7 (USDC-real) are independent tracks after G0 and can interleave. G9–G12
(app) depend on the safety core (G1–G4) being merged. G8 and G12's device run and all of Phase 4 are
human/owner gates.
