//! # swap_head_gate — never negotiate LIVE-money swap terms against an unheard chain head
//!
//! The 2026-07-19 G10c soak stall. `swap_node::initiate_from_intent` mints its timelocks as
//! `head + 10_000 / head + 5_000` with `head = ctx.cached_head().unwrap_or(0)` — safe in the
//! deterministic sim (whose head IS 0), but on a live node that simply has not heard a
//! `nimiqHeadBeacon` yet it mints **absolute** heights `10_000 / 5_000` around REAL funds, and a
//! responder that is equally headless judges those terms against head 0 and accepts them. The
//! swap then negotiates and even funds normally — until the FIRST real beacon lands (testnet
//! heads are in the millions; the gateway's t0 RPC probe failing arms a ~60 s
//! [`crate::beacon::BEACON_RPC_BACKOFF_TICKS`] silence, so this window is wide):
//!
//! - the **funded initiator**'s `refund_after_timeout(head)` sees `head > T_A` and flips the
//!   coordinator `Refunded` — a phase-only transition on the live path (the REAL refund is the
//!   [`crate::swap_live_ffi::LiveLockBook`]'s job) — and the same GC pass reaps it;
//! - the **un-funded responder** is `is_stale(head)` (`head > T_B − min_claim_window`) and is
//!   reaped before its funding verification can ever pass;
//! - the #189 `initiated_ever` tombstone then forbids the pair (whose `derive_swap_id` inputs
//!   are fixed for the construction) from ever re-initiating — so one lost race bricks the
//!   node's discovery for its whole lifetime: `active_swaps()` reads empty forever while a real
//!   NIM HTLC sits funded on-chain.
//!
//! The gate: while a node pairs a **live** money-path signer with **no cached head**, the
//! discovery layer must neither initiate (the match window freezes — candidates keep — and
//! closes on the first tick after a beacon lands) nor accept a fresh `Propose` (deferred; the
//! peer's TTL-32 slow-tick retransmit budget, ~8 min, far outlasts the beacon backoff). Sim
//! nodes ([`crate::swap_signer::MockSigner`]) are exempt: the no-RNG suites run at head 0 by
//! design. This is the discovery-layer sibling of the G9 rule already enforced for plain
//! transfers ([`crate::node::MeshNode::anchored_intent`] refuses to anchor to an unheard head).

use crate::engine::{WorkerCtx, WorkerState};

/// Whether this node pairs a LIVE money-path signer with NO heard chain head — the state in
/// which the discovery layer must neither initiate nor accept a swap (module docs). Pure reads;
/// called from the worker thread only.
pub(crate) fn live_headless(ctx: &WorkerCtx, st: &WorkerState) -> bool {
    ctx.cached_head().is_none() && st.signer.as_ref().is_some_and(|s| s.is_live())
}

#[cfg(test)]
#[path = "swap_head_gate_tests.rs"]
mod tests;
