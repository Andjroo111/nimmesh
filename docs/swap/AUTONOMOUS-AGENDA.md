# nimmesh swap — autonomous build loop (agenda + contract)

This is the standing agenda for the self-running swap loop. Each iteration: read this + the off-repo
log (`~/nimmesh-swap-loop/LOG.md`), pick the **next unchecked goal**, build it, gate it, commit on
green, log one line, and schedule the next iteration. No human prompt between goals.

## Goal ladder (do in order; each is one iteration-sized chunk)

- [x] **G1 — Refund / timeout-safety in the sim.** `SwapSim` can drive the stall→refund path (both
      legs refunded past their timeout); a test proves "worst case is a refund, never a loss."
- [x] **G2 — Refund path in the demo.** Surface G1 in the demo server + UI: a stall→refund flow
      ending in "Refunded — your funds are safe" (real `refund_after_timeout`).
- [x] **G3 — swap_wire proptest hardening.** Fuzz `decode_swap` (panic-free on arbitrary bytes) and
      round-trip any valid envelope incl. the new `btc_pubkey`.
- [x] **G4 — Mesh swap message builder.** Build `Propose`/`Accept`/`FundingProof`/`PreimageReveal`
      envelopes from engine state (the engine↔wire bridge), with tests.
- [x] **G5 — Full swap negotiated over the mock mesh.** Drive a complete swap between two MockRadio
      nodes (Propose→Accept→fund→reveal) — swaps actually working over the transport.
- [x] **G6 — Demo polish.** Engine-driven auto-advance + a responder-perspective toggle.
- [x] **G7 — Docs sweep.** Refresh SWAP / FEASIBILITY / BTC-LEG to match the shipped engine+UI+demo.
- [x] **G8 — Swap coordinator (message handler).** A pure `SwapCoordinator` that consumes an incoming
      swap envelope, advances the local swap state, and produces the next outgoing envelope(s) — the
      brain a mesh node runs. Test: drive a full swap between two coordinators exchanging envelopes
      (no hand orchestration).
- [x] **G9 — Adversarial wire/sim paths.** Robustness over a lossy mesh: a wrong-preimage reveal is
      rejected, out-of-order / duplicate messages are idempotent, an unsafe ladder refuses to fund.
- [x] **G10 — Swap fees tooltip (UI).** Port the wallet's real `SwapFeesTooltip` into the Confirm
      screen (fee breakdown) — actual component, no approximation.

When the ladder is exhausted again, re-scan for the next-highest-value work (mesh integration depth,
more hardening, perf) and append goals — keep building.

## Guardrails (hard)

- **Branch `feat/mesh-swap` only. NEVER touch `main`** (another chat owns it; merge is Andjroo's call).
- **Sim / testnet only. NO mainnet, NO real funds, NO live broadcast.** The money-path stays gated.
- **Gate every goal before commit:** `cargo test -p nimmesh-core` (default) + `--features bitcoin-leg`
  green · `cargo clippy --features bitcoin-leg --all-targets` (no warnings) · `cargo fmt --check` ·
  `scripts/size-guard.sh` · `nq lint` for any UI page. Auto-commit on green; push to `feat/mesh-swap`.
- **Presentation/UI is Andjroo's domain** — match the real Nimiq references, never approximate
  (see the [[feedback_nimiq_ui_use_real_components]] lesson).
- A goal needing a human decision (merge / mainnet / real funds / the native WebView↔Rust bridge) →
  log it **BLOCKED**, skip to the next. If all remaining are blocked → stop the loop and surface.

## Loop mechanics

Self-paced via `ScheduleWakeup` with the `<<autonomous-loop-dynamic>>` sentinel. Durable state is on
disk (this file + the off-repo log + the commits), so the loop survives context compaction.
- [x] **G11 — Swap-in-progress toast.** When a running swap is minimized, surface it as the real
      `toast-notification` "Performing swap X/5" info state (hexagon spinner); clear on done/refund.
- [x] **G12 — Setup amount validation.** The Swap Currencies screen disables Confirm + shows an
      inline message on invalid amounts (zero / over balance / below a min), like the real swap UI.
- [ ] **G13 — SwapSession router.** A node-side `swap_session` that routes incoming swap packets by
      `swap_id` to the right `SwapCoordinator` and collects its outgoing envelopes — the glue between
      `MeshNode` packet receipt and the coordinator. Tested by feeding it packets directly.
