//! # swap_node — the SwapSession ↔ MeshNode hook (G14)
//!
//! The glue the agenda calls the "MeshNode swap hook": the worker's swap-packet handlers that sit
//! between a [`crate::node::MeshNode`]'s packet receipt and its per-swap [`crate::swap_session`]
//! coordinators. Packets in → coordinator dispatch → reply envelopes out over the radio.
//!
//! Two invariants hold the privacy line (core value #3): **every** node blind-relays a swap packet
//! onward exactly like a `nimiqTx` (the relay never parses terms, addresses, or the signed tx
//! blobs), and only a node that is also a swap **participant** — one carrying a `SwapSession` in its
//! [`WorkerState`] — additionally decodes its own `swap_id` off that otherwise-opaque stream to
//! advance its coordinator and flood a reply. A pure relay does neither.

use crate::codec::encode;
use crate::engine::{relay_key, relay_onward, remember, WorkerCtx, WorkerState};
use crate::packet::{MessageType, Packet};
use crate::swap::{SwapPhase, SwapRole};
use crate::swap_wire::{decode_swap, encode_swap, SwapEnvelope, SwapKind, SWAP_ID_LEN};

/// A swap packet (`0x40`–`0x44`). Blind-relay it onward like any flooded packet, and — if this node
/// is a participant — route it to our own [`crate::swap_session::SwapSession`] and flood whatever
/// reply it produces (a `Propose` → an `Accept`, etc.).
pub(crate) fn handle_swap_packet(
    ctx: &WorkerCtx,
    packet: Packet,
    src: Option<&str>,
    st: &mut WorkerState,
) {
    // First-seen gate: a re-flooded duplicate must neither re-relay nor re-drive a coordinator.
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    remember(ctx, st, &packet);

    // Participant path: route the packet to our own session and flood whatever it produces. A pure
    // relay (no session) skips this entirely. The `on_message` borrow of `st` is dropped before we
    // flood (which re-borrows `st` for dedup/remember), so the replies are collected first.
    if st.swap.is_some() {
        if let Some(kind) = SwapKind::from_message_type(packet.msg_type) {
            let head = ctx.cached_head().map(u64::from).unwrap_or(0);
            let mut replies = match st.swap.as_mut() {
                Some(session) => session
                    .on_message(kind, &packet.payload, head)
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            // G17: after the message advanced our coordinator, take this node's next phase-driven
            // chain action (fund / claim / settle, sim tx bytes) and flood its envelope(s) too.
            replies.extend(drive_swap(st, kind, &packet.payload, head));
            sync_swap_phases(ctx, st);
            for (mt, payload) in replies {
                flood_swap_reply(ctx, mt, payload, st);
            }
        }
    }

    // Blind relay onward (source link excluded), identical to the pure-relay path.
    relay_onward(ctx, packet, src, st);
}

/// G17: the node-side swap **driver**. After an incoming message advanced our coordinator for a
/// swap, take this node's next phase-driven chain action and return the envelope(s) to flood. Each
/// inbound message triggers at most one action — the coordinator's phase is the idempotency guard,
/// so a replayed/duplicate message (which doesn't change the phase) drives nothing twice. Uses
/// sim / stand-in tx bytes; the production path builds + signs real funding/claim txs (money-path,
/// gated) and feeds them to the same `fund` / `claim_and_reveal` calls.
fn drive_swap(
    st: &mut WorkerState,
    last_kind: SwapKind,
    last_payload: &[u8],
    head: u64,
) -> Vec<(MessageType, Vec<u8>)> {
    let Ok(env) = decode_swap(last_kind, last_payload) else {
        return Vec::new();
    };
    let swap_id = env.swap_id;
    let Some(session) = st.swap.as_mut() else {
        return Vec::new();
    };

    // A responder learns S off an incoming PreimageReveal (sim: tx_wire carries the 32-byte secret),
    // verifies it opens the hashlock, and advances to Revealed — it then claims its leg + settles
    // in the match below.
    if last_kind == SwapKind::PreimageReveal {
        if let Some(secret) = env
            .tx_wire
            .as_deref()
            .and_then(|w| w.get(..32))
            .and_then(|s| <[u8; 32]>::try_from(s).ok())
        {
            if let Some(coord) = session.coordinator(&swap_id) {
                let _ = coord.recv_reveal(&env, secret);
            }
        }
    }

    let Some(coord) = session.coordinator(&swap_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match (coord.role(), coord.phase()) {
        // Initiator: once the responder accepted, fund the NIM leg → FundingProof.
        (SwapRole::Initiator, SwapPhase::Accepted) => {
            if let Ok(fp) = coord.fund(head, vec![0x11; 248], sim_tx_id(&swap_id, 0xF1)) {
                push_env(&mut out, MessageType::SwapFundingProof, &fp);
            }
        }
        // Responder: once it has seen the initiator's funding, fund the BTC leg → FundingProof.
        (SwapRole::Responder, SwapPhase::InitiatorFunded) => {
            if let Ok(fp) = coord.fund(head, vec![0x22; 120], sim_tx_id(&swap_id, 0xF2)) {
                push_env(&mut out, MessageType::SwapFundingProof, &fp);
            }
        }
        // Initiator: once both legs are funded, claim the counterparty leg (revealing S) → settle.
        (SwapRole::Initiator, SwapPhase::BothFunded) => {
            if let Ok(rev) = coord.claim_and_reveal_sim(sim_tx_id(&swap_id, 0xF3)) {
                push_env(&mut out, MessageType::SwapPreimageReveal, &rev);
                let _ = coord.settle();
            }
        }
        // Responder: S is out and it has claimed its leg (sim) — settle. No mesh message needed.
        (SwapRole::Responder, SwapPhase::Revealed) => {
            let _ = coord.settle();
        }
        _ => {}
    }
    out
}

/// Encode a driven envelope and tag it with its flood [`MessageType`].
fn push_env(out: &mut Vec<(MessageType, Vec<u8>)>, mt: MessageType, env: &SwapEnvelope) {
    if let Ok(bytes) = encode_swap(env) {
        out.push((mt, bytes));
    }
}

/// A deterministic stand-in tx id for a sim leg action (swap_id-derived; real ids come from the
/// signed tx the money-path builds).
fn sim_tx_id(swap_id: &[u8; SWAP_ID_LEN], tag: u8) -> [u8; 32] {
    let mut id = [tag; 32];
    id[..SWAP_ID_LEN].copy_from_slice(swap_id);
    id
}

/// Flood a swap envelope this node originated (an `Accept` reply, or a `Propose`). Mirrors the
/// `flood_local_tx` discipline: remember our own packet so its echo is not re-flooded and so
/// gossip-sync can serve it to a peer that was out of range.
pub(crate) fn flood_swap_reply(
    ctx: &WorkerCtx,
    mt: MessageType,
    payload: Vec<u8>,
    st: &mut WorkerState,
) {
    let packet = ctx.build_packet(mt, payload);
    st.relay_seen.insert(relay_key(&packet));
    remember(ctx, st, &packet);
    if let Ok(bytes) = encode(&packet) {
        ctx.flood(bytes);
    }
}

/// Refresh the node's observable swap-phase mirror to exactly the session's live set, so the FFI
/// side can read a swap's progress without reaching into the worker-thread-local session. Rebuilt
/// (not merged) so a swap the GC tick dropped vanishes from the mirror too.
pub(crate) fn sync_swap_phases(ctx: &WorkerCtx, st: &WorkerState) {
    let mut map = ctx.swaps.lock().unwrap();
    map.clear();
    if let Some(session) = st.swap.as_ref() {
        for (id, phase) in session.phases() {
            map.insert(id, phase);
        }
    }
}

/// G16/G18/G19: GC tick — refund funds-locked-expired swaps + drop terminal/stale ones (so a
/// long-lived node sheds abandoned half-opened swaps), then (G19) flood a teardown `SwapAbort` for
/// each stale un-funded swap dropped so the counterparty frees its slot too. Refresh the phase
/// mirror. Driven off the worker's maintenance tick.
pub(crate) fn gc_tick(ctx: &WorkerCtx, st: &mut WorkerState) {
    if st.swap.is_none() {
        return;
    }
    let head = ctx.cached_head().map(u64::from).unwrap_or(0);
    let aborted = match st.swap.as_mut() {
        Some(session) => session.tick(head),
        None => Vec::new(),
    };
    for swap_id in aborted {
        if let Ok(payload) = encode_swap(&crate::swap_messages::abort(swap_id, 0)) {
            flood_swap_reply(ctx, MessageType::SwapAbort, payload, st);
        }
    }
    sync_swap_phases(ctx, st);
}

/// This node's current phase for a swap it participates in (test/observability hook).
#[cfg(test)]
pub(crate) fn swap_phase(
    ctx: &WorkerCtx,
    swap_id: [u8; crate::swap_wire::SWAP_ID_LEN],
) -> Option<crate::swap::SwapPhase> {
    ctx.swaps.lock().unwrap().get(&swap_id).copied()
}

/// G14 (test origination): register an initiator coordinator this node started and flood its
/// `Propose`. The real money-path origination API (build the funding txs, sign offline) is a later,
/// gated goal — this drives the negotiation half over the real node loop.
#[cfg(test)]
pub(crate) fn start_swap(
    ctx: &WorkerCtx,
    swap_id: [u8; crate::swap_wire::SWAP_ID_LEN],
    coordinator: crate::swap_coordinator::SwapCoordinator,
    propose: crate::swap_wire::SwapEnvelope,
    st: &mut WorkerState,
) {
    let Some(session) = st.swap.as_mut() else {
        return; // not a participant — nothing to originate.
    };
    session.add_initiator(swap_id, coordinator);
    sync_swap_phases(ctx, st);
    if let Ok(payload) = crate::swap_wire::encode_swap(&propose) {
        flood_swap_reply(ctx, MessageType::SwapPropose, payload, st);
    }
}
