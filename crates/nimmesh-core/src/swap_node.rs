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
use crate::swap_wire::SwapKind;

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
            let replies = match st.swap.as_mut() {
                Some(session) => session
                    .on_message(kind, &packet.payload, head)
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            sync_swap_phases(ctx, st);
            for (mt, payload) in replies {
                flood_swap_reply(ctx, mt, payload, st);
            }
        }
    }

    // Blind relay onward (source link excluded), identical to the pure-relay path.
    relay_onward(ctx, packet, src, st);
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

/// Refresh the node's observable swap-phase mirror from the session, so the FFI side can read a
/// swap's progress without reaching into the worker-thread-local session.
pub(crate) fn sync_swap_phases(ctx: &WorkerCtx, st: &WorkerState) {
    if let Some(session) = st.swap.as_ref() {
        let mut map = ctx.swaps.lock().unwrap();
        for (id, phase) in session.phases() {
            map.insert(id, phase);
        }
    }
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
