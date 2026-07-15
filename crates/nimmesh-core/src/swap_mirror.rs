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
use crate::swap::SwapPhase;
use crate::swap_intent::FfiSwapMatch;
use crate::swap_session::{telemetry_now_ms, SwapMirror};

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
