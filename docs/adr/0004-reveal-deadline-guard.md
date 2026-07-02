# ADR-0004 — Reveal-deadline guard: gate the reveal on the claim window before `T_B`

**Status:** accepted (2026-07-02) · **Context:** closes the reveal-deadline half of **G4 / #75** (part of finding **S6**). Before this, `Swap::reveal_and_claim` advanced `BothFunded → Revealed` on role+phase alone, with **no check on the head**. The initiator reveals `S` by claiming the counterparty leg (timeout `T_B`); if it does so too late, the claim may not confirm before `T_B` — letting the counterparty refund that leg AND use the now-public `S` to take the other leg. This ADR adds the gate and records the (deliberate) choice of threshold.

## Decision

Gate `Swap::reveal_and_claim(current_head, params)` on a new pure `assess_reveal_deadline`: refuse (`SwapError::RevealTooLate(RevealVerdict::DeadlineTooClose { have, need })`, phase unchanged, `S` never published) when

```
T_B − current_head  <  params.min_claim_window_blocks
```

i.e. the counterparty leg's timeout must be at least `min_claim_window_blocks` ahead of the head — the **same claim window** the fund-time `WindowTooShort` check already requires against the same leg. Thread `current_head` (+ the ladder `params`) through every reveal path: `SwapCoordinator::claim_and_reveal(head, …)`, `SwapEngine::reveal_and_claim_btc(head, params)`, the FFI `SwapEngineHandle::reveal_and_claim_btc(head_ms, ladder)` (signature change — native app regenerates bindings and passes the current head + its ladder), the mesh node, and the sim. Add `Swap::reveal_deadline_margin(head)` — blocks until `T_B` — as the telemetry accessor a node logs as the window shrinks.

## Why this threshold (and not literally "within `Δ_safe` of `T_B`")

The agenda (G4) frames the guard loosely as "refuses / loudly flags when head is within `Δ_safe` of `T_B`." The mechanically-correct quantity is **the claim window before `T_B`**, not `Δ_safe`:

- The initiator claims the leg whose timeout is `T_B`. The direct risk it controls is *its own* claim not confirming before `T_B`. The margin for that is the confirmation/claim window — exactly `min_claim_window_blocks`, the value fund-time already uses for "time to fund + observe + claim the counterparty leg." Reusing it makes the fund-time and reveal-time gates symmetric against the same leg.
- `Δ_safe` (`T_A − T_B`) is a *different* quantity: it protects the **responder's** post-reveal NIM claim (it needs time to claim `T_A` after `S` becomes public at `T_B`), and it is already enforced at accept/fund time and never weakened. It is not the initiator's reveal margin.

So the guard uses `min_claim_window_blocks`. The module docs and this ADR note the deviation from the agenda's wording so a reviewer sees it was a considered choice, not an oversight.

## Consequences

- No funded→revealed transition is reachable when the counterparty leg is within its claim window of expiry: the initiator keeps `S` secret and takes the refund path once `T_B` passes (worst case stays "refund, never theft").
- The gate is stateless and re-runs each call, so — like the G3 confirmation gate — a head that has crept past the deadline is refused every time; there is no cached "already safe."
- FFI `reveal_and_claim_btc` gains `head_ms` + `ladder` params (bindings must regenerate). The engine/coordinator gain a `head` (+ params for the engine) argument, mirroring their existing `fund`/`accept` signatures.
- New tests: pure `assess_reveal_deadline` boundary (safe at `window == need`, too-close one block past, saturates to 0 past `T_B`); `Swap` refuses a tight reveal and keeps the secret; a coordinator-level test proving the threaded `head` reaches the guard.
- **Deferred to G4 slice 2** (the remaining S6 nits, same issue): BTC dust-limit (≥ 546 sat), ms→s truncation in `swap_ffi.rs`, the `nimiq/htlc.rs:61` doc comment, and faster un-funded slot reclaim. Note: `swap.rs` is now 792/800 lines — slice 2 (or a follow-up) should extract its tests to a `swap_tests.rs` sibling before adding more.
