//! # swap_mirror — the observable FFI mirror of a participant's live swaps (phase + verify verdict)
//!
//! A [`crate::swap_session::SwapSession`] lives on the worker thread; the FFI side must read a swap's
//! progress without reaching into it. So after every worker turn the swap driver rebuilds a shared
//! mirror in [`WorkerCtx::swaps`](crate::engine::WorkerCtx) — each swap's `swap_id → (phase, verify
//! note)` — and the app reads it as a `Vec` of [`FfiSwapMatch`]. The verify note is the diagnostics
//! surface: the LAST counterparty-funding verdict the verifier reached (`verify ▸ NIM too shallow
//! 3/10`), so a stalled swap shows WHY instead of leaving the operator to guess.

use crate::engine::{WorkerCtx, WorkerState};
use crate::nimiq::hex::bytes_to_hex;
use crate::swap_intent::FfiSwapMatch;

/// Refresh the node's observable swap mirror to exactly the session's live set — each swap's phase
/// plus its last counterparty-funding verify verdict — so the FFI side reads progress + the verdict
/// without touching the worker-thread-local session. Rebuilt (not merged) so a swap the GC tick
/// dropped vanishes from the mirror too.
pub(crate) fn sync_swap_phases(ctx: &WorkerCtx, st: &WorkerState) {
    let mut map = ctx.swaps.lock().unwrap();
    map.clear();
    if let Some(session) = st.swap.as_ref() {
        for (id, phase) in session.phases() {
            map.insert(id, (phase, session.verify_note(&id)));
        }
    }
}

/// The live swaps this node participates in, as FFI [`FfiSwapMatch`] records (`swap_id` hex + phase +
/// the verify verdict), sorted by id. Reads the observable mirror (kept current by
/// [`sync_swap_phases`]) so the app lists in-flight swaps without touching the worker session.
pub(crate) fn active_swaps(ctx: &WorkerCtx) -> Vec<FfiSwapMatch> {
    let map = ctx.swaps.lock().unwrap();
    let mut out: Vec<FfiSwapMatch> = map
        .iter()
        .map(|(id, (phase, note))| FfiSwapMatch {
            swap_id: bytes_to_hex(id),
            phase: (*phase).into(),
            verify_note: note.as_ref().map(|n| n.text.clone()).unwrap_or_default(),
            verify_attempts: note.as_ref().map_or(0, |n| n.attempts),
            verify_at_ms: note.as_ref().map_or(0, |n| n.at_ms),
        })
        .collect();
    out.sort_by(|a, b| a.swap_id.cmp(&b.swap_id));
    out
}
