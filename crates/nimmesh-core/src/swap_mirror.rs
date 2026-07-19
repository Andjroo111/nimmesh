//! # swap_mirror — the observable FFI mirror of a participant's live swaps (phase + verify verdict
//! + the settlement stopwatch)
//!
//! A [`crate::swap_session::SwapSession`] lives on the worker thread; the FFI side must read a swap's
//! progress without reaching into it. So after every worker turn the swap driver rebuilds a shared
//! mirror in [`WorkerCtx::swaps`](crate::engine::WorkerCtx) — each swap's `swap_id → SwapMirror`
//! (phase, verify note, timing) — and the app reads it as a `Vec` of [`FfiSwapMatch`]. The verify
//! note is the diagnostics surface: the LAST counterparty-funding verdict the verifier reached
//! (`verify ▸ NIM too shallow 3/10`), so a stalled swap shows WHY instead of leaving the operator
//! to guess. The timing pair is the settlement stopwatch: `started_at_ms` stamps the swap's first
//! appearance (its coordinator registered) and `settled_in_ms` is stamped once when the phase first
//! reads `Settled` — the swap sheet's "settled in Xs". Both are telemetry wall-clock only
//! ([`crate::swap_session::telemetry_now_ms`]); consensus stays head-anchored (ADR-0005).

use crate::engine::{WorkerCtx, WorkerState};
use crate::nimiq::hex::bytes_to_hex;
use crate::swap::{SwapPhase, SwapRole};
use crate::swap_coordinator::CoordError;
use crate::swap_funding_verify::{FundingRejected, MismatchReason};
use crate::swap_intent::{Asset, FfiSwapMatch};
use crate::swap_wire::SwapLegId;

/// One entry of the observable swap mirror ([`crate::engine::WorkerCtx::swaps`]): a swap's current
/// phase, its last counterparty-funding verify verdict (`None` until a `FundingProof` has been
/// verified), and the settlement stopwatch. [`crate::swap_mirror`] rebuilds the map from this and
/// reads it back as `FfiSwapMatch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwapMirror {
    /// This node's current phase for the swap.
    pub(crate) phase: SwapPhase,
    /// The last verify verdict (diagnostics), `None` until one exists.
    pub(crate) note: Option<SwapVerifyNote>,
    /// Stopwatch start: Unix-ms wall clock when this swap FIRST entered the mirror — i.e. when
    /// its coordinator registered (initiation/confirm). Telemetry only, same contract as
    /// [`SwapVerifyNote::at_ms`]: the app renders it; consensus stays head-anchored (ADR-0005).
    pub(crate) started_at_ms: u64,
    /// Initiation → Settled, in ms — stamped ONCE when the phase first reads `Settled` (clamped
    /// to ≥ 1 so it is distinguishable from the 0 = "not settled yet" sentinel). The swap
    /// sheet's "settled in Xs".
    pub(crate) settled_in_ms: u64,
}

/// The LAST counterparty-funding verification outcome for one swap, kept purely so the app can SEE
/// the verifier's live verdict instead of guessing why a swap is stalled (the diagnostics deliverable).
/// It is telemetry — nothing in the swap state machine reads it, and it never authorizes a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwapVerifyNote {
    /// A short plain-English verdict, e.g. `"NIM too shallow 3/10"` or `"verified — funding USDC"`.
    pub(crate) text: String,
    /// How many verification attempts (message-arrival + tick re-checks) this swap has seen.
    pub(crate) attempts: u32,
    /// Unix-ms wall clock at the last attempt (telemetry only — the app renders it as "Xs ago";
    /// the consensus swap logic remains head-anchored and clock-free, ADR-0005).
    pub(crate) at_ms: u64,
}

/// Telemetry wall clock (Unix ms) for the verify note's "Xs ago" and the mirror's settlement
/// stopwatch — display only, never consensus (the swap logic stays head-anchored, ADR-0005).
pub(crate) fn telemetry_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The human name of the counterparty leg being verified: the NIM leg is always `"NIM"`; the
/// counterparty leg carries whichever chain this session settles on.
pub(crate) fn leg_word(leg: SwapLegId, counter: Asset) -> &'static str {
    match leg {
        SwapLegId::Nim => "NIM",
        SwapLegId::Counterparty => match counter {
            Asset::Usdc => "USDC",
            Asset::Btc => "BTC",
            Asset::Nim => "NIM",
        },
    }
}

/// Turn a verification result into the plain-English diagnostic the app surfaces. On success it names
/// the node's NEXT money-path step (a responder that just confirmed the NIM leg funds its own USDC/BTC
/// leg; an initiator that confirmed the counterparty leg reveals `S`); on refusal it names the leg and
/// the reason — a shallow depth shows the live `have/need` so the operator watches it climb.
pub(crate) fn describe_verify(
    role: SwapRole,
    leg: SwapLegId,
    counter: Asset,
    result: &Result<(), CoordError>,
) -> String {
    let cp = leg_word(leg, counter);
    match result {
        Ok(()) => match role {
            SwapRole::Responder => format!(
                "verified — funding {}",
                leg_word(SwapLegId::Counterparty, counter)
            ),
            SwapRole::Initiator => "verified — revealing S".to_string(),
        },
        Err(CoordError::FundingUnverified(r)) => match r {
            FundingRejected::NotFundedYet => format!("{cp} not funded yet"),
            FundingRejected::TooShallow { have, need } => {
                format!("{cp} too shallow {have}/{need}")
            }
            FundingRejected::Underfunded { have, need } => {
                format!("{cp} underfunded {have}/{need}")
            }
            FundingRejected::TimeoutTooShort { .. } => format!("{cp} timeout too short"),
            FundingRejected::Mismatch(MismatchReason::Hashlock) => {
                format!("{cp} hashlock mismatch")
            }
            FundingRejected::Mismatch(MismatchReason::Recipient) => {
                format!("{cp} recipient mismatch")
            }
        },
        // An illegal transition here means the phase moved on already — nothing to report.
        Err(_) => format!("{cp} not ready"),
    }
}

/// Refresh the node's observable swap mirror to exactly the session's live set — each swap's phase,
/// last verify verdict, and stopwatch — so the FFI side reads progress without touching the
/// worker-thread-local session. Rebuilt (not merged) so a swap the GC tick dropped vanishes from
/// the mirror too; the timing carries over from the previous rebuild (a new swap starts its
/// stopwatch now; a swap first seen `Settled` stamps its `settled_in_ms` once).
pub(crate) fn sync_swap_phases(ctx: &WorkerCtx, st: &WorkerState) {
    let now = telemetry_now_ms();
    let mut map = ctx.swaps.lock().unwrap();
    let prev: std::collections::HashMap<_, (u64, u64)> = map
        .iter()
        .map(|(id, m)| (*id, (m.started_at_ms, m.settled_in_ms)))
        .collect();
    map.clear();
    if let Some(session) = st.swap.as_ref() {
        for (id, phase) in session.phases() {
            let (started_at_ms, mut settled_in_ms) = prev.get(&id).copied().unwrap_or((now, 0));
            if phase == SwapPhase::Settled && settled_in_ms == 0 {
                // Stamp once; clamp ≥ 1 so a sub-ms sim settle is distinguishable from the
                // 0 = "not settled yet" sentinel.
                settled_in_ms = now.saturating_sub(started_at_ms).max(1);
            }
            map.insert(
                id,
                SwapMirror {
                    phase,
                    note: session.verify_note(&id),
                    started_at_ms,
                    settled_in_ms,
                },
            );
        }
    }
}

/// The live swaps this node participates in, as FFI [`FfiSwapMatch`] records (`swap_id` hex, phase,
/// the verify verdict, and the stopwatch), sorted by id. Reads the observable mirror (kept current
/// by [`sync_swap_phases`]) so the app lists in-flight swaps without touching the worker session.
pub(crate) fn active_swaps(ctx: &WorkerCtx) -> Vec<FfiSwapMatch> {
    let map = ctx.swaps.lock().unwrap();
    let mut out: Vec<FfiSwapMatch> = map
        .iter()
        .map(|(id, m)| FfiSwapMatch {
            swap_id: bytes_to_hex(id),
            phase: m.phase.into(),
            verify_note: m.note.as_ref().map(|n| n.text.clone()).unwrap_or_default(),
            verify_attempts: m.note.as_ref().map_or(0, |n| n.attempts),
            verify_at_ms: m.note.as_ref().map_or(0, |n| n.at_ms),
            started_at_ms: m.started_at_ms,
            settled_in_ms: m.settled_in_ms,
        })
        .collect();
    out.sort_by(|a, b| a.swap_id.cmp(&b.swap_id));
    out
}
