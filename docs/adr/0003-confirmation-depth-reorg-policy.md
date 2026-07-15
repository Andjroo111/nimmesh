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

## Addendum (2026-07-15) — M7 fast-finality settlement profile (sub-30 s)

The first real capped mainnet swaps settled **correctly but slowly** (~3+ minutes): the money path
was waiting out probabilistic depth on chains that offer something strictly stronger —
**deterministic finality** — and re-checking on a ~15 s heartbeat. This addendum replaces
depth-waiting with finality where a chain certifies it, retunes the ≤ $5 profile, and speeds up
*when* the unchanged fail-closed gate runs. `require_funded` itself is untouched.

### 1. Polygon `finalized`-tag verification (the USDC leg's primary signal)

Polygon PoS ships **Heimdall v2 milestone finality** (live on mainnet **2025-07-10**): a
Tendermint-style milestone signs the canonical chain roughly every ~5 s, after which the finalized
prefix is **deterministically irreversible** — post-upgrade reorg depth is capped at ~2 blocks (the
un-finalized tip). Every standard RPC serves it as `eth_getBlockByNumber("finalized", false)`.

Both gateway-backed USDC verifiers (`PolygonHtlcVerifier`, `AmoyHtlcSwapVerifier`) now read that
tag: an escrow whose inclusion block is **at or below the finalized height** is reported at
`FINALIZED_CONFIRMATIONS = u32::MAX`.

**Why `u32::MAX` instead of a new "finalized" arm in the safety core:** `require_funded` /
`FundingObservation` are the audited go/no-go; adding a boolean "finality overrides depth" bypass
there would create a second authorization path to review and to get wrong. Deterministic finality
is, semantically, *maximal burial* — there is nothing deeper to wait for — so expressing it as the
maximum confirmation count makes the existing `confirmations >= min_confirmations` comparison do
exactly the right thing under ANY policy (including the paranoid one), while the amount / timeout /
hashlock / recipient gates still apply unchanged first. Finality is a *verifier-side report*, never
a new bypass.

**Fail-closed rules (unchanged in spirit):** a finalized read that errors, an RPC that predates the
tag (`finalized_head()`'s trait DEFAULT errors, so every fake/old endpoint keeps today's
behaviour), or an escrow above the finalized height all fall back to the exact pre-existing depth
count — strictly *slower*, never *weaker*. Under the M5 cross-read seam (ADR-0011), finality must
be vouched by BOTH endpoints: the determination is the **min** of the two finalized heights (capped
at the already-cross-checked head), so a lying/MITM'd primary cannot inflate finality past an
honest secondary, and a secondary that cannot vouch (absent tag / read error) blocks the fast path
entirely. The head-agreement tolerance (`HEAD_CROSS_TOLERANCE_BLOCKS`) and the live-escrow re-read
still run first, unchanged. Lying-RPC tests cover the finalized path on both verifiers.

### 2. The ≤ $5 mainnet depths, retuned (`mainnet_defaults`)

| Chain | M6 (2026-07-13) | **M7 (this addendum)** | Why |
|-------|-----------------|------------------------|-----|
| NIM | 10 | **2** | Albatross is a BFT PoS chain: 1 s micro-blocks under a single elected slot producer, so a micro-block reorg requires a **slashable fork proof** (equivocation) and observed forks are ≤ 1 block; 2 blocks (~2 s) covers that with margin. This is an honest *probabilistic* floor, NOT macro-block finality — the batch's macro block (the true BFT-final point) can be up to ~1 min away, which would blow the settlement budget for a ≤ $5, timelock-refundable leg. |
| USDC (Polygon) | 64 | **8** (fallback only) | The PRIMARY burial signal is now the `finalized` tag (~5 s, above). Depth 8 gates only an RPC that does not serve the tag: 4× the post-Heimdall-v2 ~2-block reorg cap, ~16 s at ~2 s blocks — versus 64 blocks ≈ 2–3 min of pure depth-count. |
| BTC | 2 | **2** | Unchanged; BTC only gates the unfunded leg in the current NIM⇄USDC path. |

**The old profile is not deleted:** `ConfirmationPolicy::mainnet_paranoid()` is the M6 NIM 10 /
USDC 64 / BTC 2 table verbatim — an explicitly named **one-line revert** at the assembly call site
(`swap_live_ffi_live_impl::mainnet_money_path`). Selecting it ignores the finality benefit and
waits out the deeper depths: strictly slower, never less safe. Use it to bisect a settlement-safety
concern, or as the deeper base a larger-value swap raises from. As before, **a larger mainnet swap
MUST raise every floor** — this is the ≤ $5 envelope, nothing more.

### 3. The ~3 s fast tick (when the gate runs, not what it checks)

Re-verification (0.81.0) rides `gc_tick` on the ~15 s BeaconTick keepalive — worst-case two
15 s quanta per leg on top of chain latency. New `swap_fast_tick`: the shims poll
`MeshNode::poll_swap_fast()` every ~3 s while a swap sheet is live (iOS: a native
`DispatchSourceTimer` for the demo's lifetime; mac-node: its existing 2 s beat); the job runs ONLY
the funding re-verification + the resulting `drive_phase_action` money-path step + a mirror
re-sync.

- **`RETRANSMIT_TTL` interaction (the budget trap):** the retransmit budget (32) is counted in
  *ticks*. Had retransmit ridden the fast cadence, 32 × 3 s ≈ 96 s would have gutted the ~8 min
  lossy-mesh recovery budget — so the fast tick performs **no retransmit drain**, no GC/refund
  sweep, no gossip-sync, no beacon emit, and no match-window close; all stay on the slow cadence
  and their budgets are unchanged.
- **RPC cost guards:** the fast poll is idle-free (no proof-seen swap awaiting counterparty
  funding ⇒ no consult AND no rate-limit slot burned) and core-side rate-limited to one consult
  per `FAST_VERIFY_TICK_MS` (3 s) on the worker's monotonic clock — a hammering shim or webui
  cannot hit the shared-IP RPC harder. Deterministic fence tests (ADR-0005), no wall-clock sleeps.

### 4. The settled-in-Xs stopwatch (keeping the budget honest)

The observable mirror entry now carries `started_at_ms` (first appearance = coordinator
registered) and `settled_in_ms` (stamped once at `Settled`), surfaced over FFI
(`FfiSwapMatch`) and rendered in the swap sheet ("settled in X.Xs"). Telemetry wall-clock only —
consensus stays head-anchored (ADR-0005) — but it makes the settlement budget a number Andjroo sees
on the phone after every swap, so a regression back toward minutes is visible on sight.

### Expected settlement budget (capped NIM⇄USDC phone↔phone)

| Quantum | Before | After |
|---------|--------|-------|
| USDC escrow burial | 64 blocks ≈ 2–3 min depth-count | `finalized` tag ≈ 5–10 s (depth-8 fallback ≈ 16 s) |
| NIM HTLC burial | 10 blocks ≈ 10 s | 2 blocks ≈ 2 s |
| Re-verify cadence | ~15 s × several beats | ~3 s × several beats |
| **Total (typ.)** | **~3+ min** | **~20–30 s** |

The relaxations named here — the finalized fast-path, NIM 10→2, USDC 64→8-as-fallback, and the
fast cadence — are the ONLY money-path changes in this PR; each is justified above, each fails
toward the slower path, and `mainnet_paranoid()` reverts the profile in one reviewed line.
