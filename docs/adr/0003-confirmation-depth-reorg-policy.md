# ADR-0003 — Per-chain confirmation-depth + reorg policy for funding verification

**Status:** accepted (2026-07-02) · **Context:** closes **G3 / #74** (part of finding **S6**), building on the G1 on-chain funding-verification seam (`swap_funding_verify`, #72). G1 gave every funded/reveal transition a go/no-go against a real HTLC on-chain; it already refused an HTLC that was on-chain but not yet buried to a `min_confirmations` depth. That depth was a **single flat floor** (`DEFAULT_MIN_CONFIRMATIONS = 1`), which is wrong across chains and said nothing about **reorgs**. This ADR fixes the depth *per chain* and pins down what "re-verify on reorg" means for a forward-only state machine.

## Decision

Introduce a pure **`ConfirmationPolicy`** (in `swap_funding_verify`) carrying one minimum-confirmation depth **per chain** (NIM / BTC / USDC), and resolve the depth from the **leg being verified** rather than passing one flat number:

- `required(chain: Asset) -> u32` — the depth for a chain (reuses the existing `swap_intent::Asset`, the same enum discovery already advertises as `counter_asset`; no new enum).
- `required_for_leg(leg: SwapLegId, counterparty: Asset) -> u32` — the NIM leg always uses the NIM depth; the `Counterparty` leg uses whichever chain that swap settles on (BTC or USDC). This is the seam the coordinator/session gate calls to turn its `HtlcExpectation.leg` into a concrete `min_confirmations` for `require_funded`.
- Builder (`testnet_defaults`, `uniform`, `with_nim/with_btc/with_usdc`) and `Default = testnet_defaults`, so an un-configured node is **safe-by-default** — never zero-confirmation.

**Testnet defaults (deliberately low, to keep the sim/testnet loop fast):**

| Chain | Depth | Why |
|-------|-------|-----|
| NIM | 2 | Albatross PoS reaches macro-block finality within a batch; a couple of blocks past the funding batch is ample on testnet. |
| BTC | 3 | PoW reorgs of 1–2 blocks are routine; 3 is a moderate signet/testnet burial. |
| USDC (Polygon) | 5 | Polygon PoS has fast blocks but deeper probabilistic reorgs than BTC at equal wall-clock; a few more blocks. |

Depths increase with reorg risk. **These are testnet values and must not ship to mainnet** — Phase 4 mainnet gating (`MAINNET-GATING.md`) re-tunes them (BTC → 6, Polygon deeper) as a `needs:owner` decision.

**Reorg policy** — the property, not a background monitor: the gate (`SwapCoordinator::verify_and_observe_funding` → `require_funded`) is **stateless and re-runs on every `FundingProof`/tick with a fresh observation**, holding no "already funded" memory. So a leg that reorgs from deep back below its policy depth is **refused again** (`TooShallow`), and a leg whose funding tx is orphaned entirely reads as `Absent` (`NotFundedYet`). Because every *subsequent* money-path step (the responder funding its own leg, the initiator revealing `S`) re-runs the gate, a reorg that strikes between funding and the next step is caught before that step — no funded/reveal transition may rest on a leg that is currently below its chain's depth. The `LedgerVerifier` test oracle gained `reorg_to(depth)` (re-buries shallower) and `orphan_all()` (drops the tx) to prove both.

**Wiring:** `SwapSession` now holds a `ConfirmationPolicy` (default testnet) and a `counterparty_chain` (`Asset::Btc` today — the mesh `recv_propose` path is BTC-shaped; USDC swaps drive `SwapEngine` directly). Its `FundingProof` handler resolves the depth via `required_for_leg(coord.counterparty_expectation().leg, self.counterparty_chain)` instead of the flat `DEFAULT_MIN_CONFIRMATIONS`. The pure `require_funded` / `verify_and_observe_funding` signatures are unchanged (they still take the *resolved* `min_confirmations: u32`), so no coordinator call-site churned.

## Why

1. **A flat floor is unsafe or slow, depending which chain you pick it for.** One number that is safe for BTC over-waits NIM; one that is fast for NIM under-buries BTC/Polygon. The depth belongs to the chain, and the only place the swap knows the chain is the leg + the matched `counter_asset` — exactly what `required_for_leg` consumes.
2. **Reuse `Asset`, don't mint a parallel chain enum.** `Asset` (Nim/Btc/Usdc) already *is* the chain identity the discovery layer advertises; a new `SwapChain` enum would duplicate it and force a mapping. Keying the policy on `Asset` keeps one source of truth.
3. **"Re-verify on reorg" is a property of a stateless gate, not a new subsystem.** The honest, CI-testable guarantee for a forward-only state machine is "never *advance* a leg that is currently below depth, including right after a reorg re-exposes it shallow." Rolling *back* an already-advanced phase after a deep reorg is a different, chain-specific concern (continuous post-advance monitoring), which is the **gateway-backed verifier's** job on a real chain — explicitly gated (#72 tail, `needs:owner`, not CI-testable against the mock mesh). This ADR does not pretend to solve that in the sim.
4. **Safe-by-default.** `Default`/`testnet_defaults` never yields a zero-confirmation policy, so a node that forgets to configure depth still refuses unconfirmed fundings.

## Scope / non-goals

- **No continuous reorg monitor.** Un-advancing a *funded* leg after a deep reorg requires the real gateway to re-observe on a schedule; that lives with the gateway-backed `FundingVerifier` impls (#72 tail), which are gated on real chains and not exercised here.
- **`counterparty_chain` is per-session, not per-swap.** The mesh settlement path is BTC-only today (the coordinator's counterparty recipient is a `btc_pubkey`); USDC swaps drive `SwapEngine` directly. A node that later runs both BTC and USDC swaps over the *mesh* path will need a per-swap chain tag — noted, not built.
- **Depth values are testnet.** Mainnet re-tuning is a Phase-4 `needs:owner` gate.

## Consequences

- `require_funded` is unchanged and remains the single go/no-go; `ConfirmationPolicy` only *chooses its input*. The reorg guarantee is emergent from the gate being stateless — no new state to get wrong.
- New tests: policy per-chain defaults + leg resolution + builder; ledger `reorg_to` (deep→shallow refused again) and `orphan_all` (reads Absent); a session-level test proving the mesh gate applies the policy's NIM depth (2-deep refused, 3-deep advances under a `with_nim(3)` policy).
- The gateway-backed verifier (real NIM RPC / BTC / Polygon), when built (gated), mirrors this same policy and adds the schedule that turns the "re-verify on reorg" property into active post-advance monitoring.

## Addendum (2026-07-13) — M6 mainnet depths (the guard-lift)

The Phase-4 mainnet retune this ADR foresaw is now realized as
`ConfirmationPolicy::mainnet_defaults()`, selected only by the off-by-default mainnet swap path
(`mainnet_swap::MAINNET_SWAP_ENABLED`, false on any merged branch). It is calibrated to the **first
≤ $5 self-swaps** (both sides Andjroo's own wallets, timelock-refundable either way) — NOT to
custodial/high-value finality:

| Chain | Testnet | **Mainnet (≤ $5)** | Why this value |
|-------|---------|--------------------|----------------|
| NIM | 2 | **10** | Albatross PoS reaches macro-block finality within a batch; 10 blocks is several batches past the funding — ample for a small, refundable leg. |
| USDC (Polygon) | 5 | **64** | Polygon PoS (Bor) reorgs deeper than NIM at equal wall-clock; 64 blocks (~2 min at ~2 s) is the small-amount floor bridges/exchanges use for low-value Polygon deposits. |
| BTC | 3 | **2** | For a ≤ $5 self-swap whose timelock refund is the worst-case floor, 2 confirmations is a pragmatic small-amount burial. **This intentionally supersedes the earlier "BTC → 6" placeholder** in this ADR: 6 is right for a high-value BTC settlement, not a $5 self-swap. A larger BTC swap MUST raise it. |

**A larger mainnet swap MUST raise every one of these** (a separate, reviewed `needs:owner` change) —
`mainnet_defaults()` is the ≤ $5 envelope, nothing more. The mainnet secondary cross-read (ADR-0011)
remains required for USDC (two independent Polygon RPCs are wired); the NIM leg has no second public
mainnet RPC (only `rpc.nimiqwatch.com`), so it runs single-source unless the operator stands up their
own node — a residual risk the guard-lift PR names explicitly.
