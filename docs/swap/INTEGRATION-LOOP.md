# Cross-Chain Swap — Integration Loop Contract

The autonomous build loop that works the [`INTEGRATION-AGENDA.md`](./INTEGRATION-AGENDA.md) queue
until the cross-chain HTLC swap is hardened and integrated into the Nimmesh app. This extends the
base [`../LOOP.md`](../LOOP.md); where they differ, this file wins for swap-integration work.

---

## North star
The swap runs over the real BLE mesh, safely, with all three legs (NIM ⇄ BTC ⇄ USDC-Polygon), on
**testnet/signet/Amoy**. Only two things stay human: **on-device BLE** and **mainnet / real funds**.

## Scope (set by Andjroo, 2026-07-01)
Drive **Phases 0 → 3** (green → money-path safety core → USDC-real → app integration). **Stop** at
G8 (independent contract review), G12's physical device run, and all of Phase 4. Everything up to
those gates is in-scope for autonomous work.

## Cadence
Self-paced, one goal per cycle. After each cycle, schedule the next with `ScheduleWakeup` (~1200s
idle tick; sooner when actively watching a CI run). A durable launchd runner is provided
(`~/scripts/nimmesh-swap-loop*.sh`) for hands-off operation — Andjroo arms it (launchd won't
service a loop bootstrapped from an agent shell; start it from a real Terminal or on reboot).

## Per-cycle recipe
1. **Read** this file + `INTEGRATION-AGENDA.md` + the goal's cited findings/docs + `RISKS.md`.
2. **Pick** the top open goal whose deps are merged (agenda dependency order).
3. **Branch** `feat/gN-...` off `main`. Keep every file **< 800 lines** (`scripts/size-guard.sh`).
   Record real decisions in `docs/adr/`.
4. **Test-first for safety goals (G1–G4):** write the adversarial/regression test that *fails*
   first, then make it pass. `assert_no_one_sided` must stay green throughout.
5. **Verify locally** (the green gate, all must pass on the Mini):
   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all
   cargo test -p nimmesh-core --features bitcoin-leg
   cargo test -p nimmesh-core --features polygon-leg
   bash scripts/size-guard.sh
   ```
   For UI goals (G10): `nq lint` + a screenshot diff vs the `nimiq-ui` reference.
6. **PR** linking the issue; bump version + `CHANGELOG` entry; write an ADR if a real decision was made.
7. **Merge** per policy below once CI is green.
8. **Close** the issue; append a cycle-log entry to this file; file any new gaps as issues; leave
   Andjroo a "what to test on device" note when a goal reaches a device-facing state.

## Merge policy (set by Andjroo, 2026-07-01)
- **Auto-merge everything on testnet when CI is fully green.** All of Phases 0–3 target
  testnet/signet/Amoy and are considered non-money-gated. Do **not** rely on `gh pr merge --auto`
  (repo is unprotected): `gh pr checks <pr> --watch`, then `gh pr merge <pr> --squash --delete-branch`.
- **The only gates are MAINNET and real devices** — plus the two explicit owner-gated goals **G8**
  (independent contract review) and **G12**'s physical device run. Those are labelled `needs:owner`,
  left open, and reported — never merged by the loop.
- Because the safety core (G1–G4) auto-merges, **"green" must genuinely mean safe**: a safety goal
  is not done until its adversarial test (the one that fails without the fix) is committed and green.
  The gate is the reviewer.

## Guardrails — the "never" list
Inherited from `../LOOP.md`:
- Never broadcast on mainnet, flip `networkId` to mainnet, or take any real-fund / real-device action.
- Never let key/seed material touch the mesh, logs, or relay — only pubkey + signed bytes cross FFI.
- Never show "paid"/"settled" before on-chain inclusion at the required confirmation depth.
- Never relay a packet without verifying it is a well-formed signed tx.
- Never merge a red PR; never mark a goal done without CI proof; never introduce a second source of
  truth for the protocol (Rust core is canonical).

New for swap integration:
- **Never advance a funded-state transition (`fund` / `claim_and_reveal`) from a message alone** —
  it requires on-chain verification (the G1 invariant). This is the anti-theft rule.
- **Never weaken the timelock ladder** (`T_a − T_b ≥ Δ_safe`) or the reveal-deadline guard.

## Definition of done (for this loop)
Phases 0–3 merged to `main`, CI green, USDC live on Amoy with real integration tests, the swap
drivable end-to-end from the app UI over sim/testnet, and a "ready for device + mainnet review"
handoff written for Andjroo. G8 / G12-device / Phase 4 remain open by design.

---

## Cycle log
_(one line per completed cycle: date · goal · PR · result)_

- _Cycles through 2026-07-01 (G0–G4, #84 — PRs #85–#112) were logged on their PRs/issues; the in-file log picks up at G9._
- 2026-07-02 · G9 #80 slice 1 — discovery metrics over UniFFI (`MeshNode::intent_metrics()` → `FfiIntentMetrics`) · PR #114 · merged (v0.47.0)
- 2026-07-02 · G9 #80 slice 2 — active-swap match list over UniFFI (`MeshNode::active_swaps() -> Vec<FfiSwapMatch>`, `FfiSwapPhase` relocated to always-compiled `swap_intent`) · PR #116 · merged (v0.48.0)
- 2026-07-02 · loop maintenance — `node.rs` 799→785: inline tests → `node_tests.rs` sibling (headroom for G9 slice 3); cycle-log backfill · PR #118 · merged (v0.48.2)
- 2026-07-02 · G7 #78 decision — ADR-0006: relayer-sponsored gas (EIP-2771) + mandatory self-funded fallback; impl gated on G6 · PR #119 · merged (v0.48.3)
- 2026-07-02 · G5 #76 — the real USDC HTLC contract (`contracts/`): SHA-256 lock, permit single-tx funding, ERC-2771 seam, byte-match vector vs `usdc_swap_id`, 17 Foundry tests + `contracts (solidity)` CI job · PR #122 · merged (v0.49.0)
- 2026-07-03 · loop maintenance — `swap_node.rs` 799→741: `#[cfg(test)]` hooks → `swap_node_test_hooks.rs` child mod (headroom, companion to #118) · PR #130 · merged (v0.49.6)
- 2026-07-03 · permit builders — `evm_permit` (EIP-2612 digests, cast-vector-anchored) + `evm_abi::htlc_new_swap_with_permit`/`erc20_domain_separator`/`erc20_nonces`: the off-chain half of single-tx S4-race-free funding · PR #131 · merged (v0.49.7)
- 2026-07-03 · #72 tail slice 1 — `polygon_verifier::PolygonHtlcVerifier` (gateway-backed USDC FundingVerifier: recipient-indexed `NewSwap` logs + `getSwap` Live check + head depth, fail-closed) + `eth_blockNumber`/`eth_getLogs` codecs; live Amoy proof rides G6 · PR #132 · merged (v0.49.8)
- 2026-07-04 · G6 #77 — `NimmeshHtlc` LIVE on Amoy (`0xaaCa…F7f3`) + the real-RPC round-trip PROVEN through our own stack (byte-match, escrow→Live, SHA-256 `withdraw(S)`→Claimed, premature-refund revert, refund→Refunded, exact USDC round-trip; receipts in `docs/swap/AMOY.md`) · PR #149 · merged (v0.50.0)
- 2026-07-04 · G7 #78 — gas abstraction IMPLEMENTED: `NimmeshForwarder` (ERC-2771, ADR-0008) + `evm_forward` builders + forwarder-bound HTLC v2 on Amoy; LIVE gasless proof — a zero-POL user funded + settled a real escrow, account nonce 0 throughout; S4 fully closed · PR #150 · merged (v0.51.0)
- 2026-07-05 · #72 tail — `PolygonHtlcVerifier` PROVEN live on Amoy: a real escrow walks Found(depth 2→8, gate PASS at policy 5) → Mismatch(Hashlock) → Absent-after-withdraw; RPC getLogs 50-block cap + 236k newSwap gas recorded · PR #155 · merged (v0.51.2)
