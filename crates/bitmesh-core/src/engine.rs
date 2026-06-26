//! # engine — the real-packet relay / gateway / origin logic
//!
//! The protocol brain that runs **off** the radio's callback thread (ADR-0002 gotcha a):
//! [`crate::node::MeshNode`] cheaply enqueues every BLE event, and a dedicated worker
//! thread drains the queue and calls into this module. Everything here speaks the **real
//! G4 wire codec** ([`crate::codec`], [`crate::packet`], [`crate::envelope`]) — the
//! temporary G2 `MeshFrame` framing is gone.
//!
//! Three roles, one code path:
//!
//! - **relay** (every node, always): decode → **blind** LRU dedup on a packet-header
//!   identity → degree-adaptive relay decision → jittered TTL-decrement re-flood that
//!   **excludes the source link**. The opaque `txWire` is *never* inspected here (core
//!   value #3, trustless relay). The G6 relay sophistication (degree-adaptive flooding,
//!   injectable jitter, the loop-free TTL hop cap) lives in [`crate::relay`]; the
//!   `fragment = 0x20` split/reassemble path lives in [`crate::fragment`].
//! - **gateway** (a node with internet, modelled by an injected [`MeshGateway`]): on a
//!   `nimiqTx`, dedup on `txId`, submit once (the mock records; **G8** does the real
//!   `sendRawTransaction`), and flood a `nimiqTxReceipt` back.
//! - **origin** (the node that called `submit_local_tx`): on a matching
//!   `nimiqTxReceipt`, flip its payment `Pending → Settled / Failed`.
//!
//! `txWire` is **opaque** end to end — no signing, no broadcast, no key material (that is
//! G3/G8, money-path and Andjroo-gated).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use crate::codec::{decode, encode};
use crate::dedup::DedupCache;
use crate::envelope::{decode_envelope, encode_envelope, NimiqEnvelope};
use crate::fragment::{parse_fragment, Reassembler};
use crate::gateway::{MeshGateway, ReceiptStatus, SubmitContext};
use crate::gcs::GcsFilter;
use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT, DEFAULT_TTL, PEER_ID_LEN};
use crate::radio::BleRadio;
use crate::relay::{relayed_ttl, RelayPolicy};
use crate::store_forward::{packet_id, RecentCache, SyncScheduler};
use crate::transport::{mock_tx_id, TxId};

/// Where a payment stands from the origin's point of view (FFI-visible).
///
/// `Pending` until a gateway receipt arrives; honours unconfirmed-until-inclusion
/// (core value #5) — only an `Accepted` receipt yields `Settled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PaymentStatus {
    /// Sent (or relaying); no gateway receipt seen yet.
    Pending,
    /// A gateway accepted the tx into the mempool.
    Settled,
    /// A gateway rejected the tx (expired / failed).
    Failed,
}

/// LRU bound for the blind packet-relay dedup (hostile-flood ceiling, RISKS.md #4).
const RELAY_CACHE_CAP: usize = 2048;
/// LRU bound for the gateway's per-`txId` submit-once dedup.
const GATEWAY_CACHE_CAP: usize = 1024;
/// A `nimiqTxReceipt` payload is exactly `txId(32) | status(1)`.
const RECEIPT_PAYLOAD_LEN: usize = 33;

/// The **blind** relay dedup key: `(type, senderID, timestamp)` — all packet-header
/// fields, never the opaque payload (PROTOCOL.md dedup intent). For a flooded `nimiqTx`
/// this header tuple is the packet's identity; the endpoints separately correlate
/// receipts by the envelope `txId`.
type RelayKey = (u8, [u8; PEER_ID_LEN], u64);

fn relay_key(p: &Packet) -> RelayKey {
    (p.msg_type.to_u8(), p.sender_id, p.timestamp_ms)
}

/// Encode a `nimiqTxReceipt` payload: `txId(32) | status(1)`.
fn encode_receipt(tx_id: &TxId, status: ReceiptStatus) -> Vec<u8> {
    let mut v = Vec::with_capacity(RECEIPT_PAYLOAD_LEN);
    v.extend_from_slice(&tx_id.0);
    v.push(status.code());
    v
}

/// Decode a `nimiqTxReceipt` payload, returning `None` on any malformed input.
fn decode_receipt(payload: &[u8]) -> Option<(TxId, ReceiptStatus)> {
    if payload.len() != RECEIPT_PAYLOAD_LEN {
        return None;
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&payload[..32]);
    Some((TxId(id), ReceiptStatus::from_code(payload[32])))
}

/// State the origin tracks for its in-flight payments.
#[derive(Default)]
struct OriginShared {
    payments: HashMap<TxId, PaymentStatus>,
}

/// The shared context the worker thread operates on. Crucially it holds **no**
/// `Arc<MeshNode>` — only the radio (strong) plus plain state — so the node↔radio↔node
/// graph stays a tree and a torn-down node is reclaimed (ADR-0002 gotcha d).
pub struct WorkerCtx {
    /// This node's 8-byte protocol sender id (header `senderID`).
    pub(crate) sender_id: [u8; PEER_ID_LEN],
    /// The native radio. Held **strongly**; the radio holds the node weakly.
    pub(crate) radio: Arc<dyn BleRadio>,
    /// Present iff this node also acts as a gateway (internet + RPC at G8).
    pub(crate) gateway: Option<Arc<dyn MeshGateway>>,
    /// Currently-connected BLE peers; a flood writes to each.
    peers: Mutex<HashSet<String>>,
    /// The origin's payment ledger + its settle signal.
    origin: Mutex<OriginShared>,
    settled: Condvar,
    /// Monotonic per-node sequence used as the packet `timestamp_ms` so each flooded
    /// packet has a unique blind [`RelayKey`] (the mock's deterministic clock).
    seq: AtomicU64,
    /// Observability counters (test + future telemetry hooks).
    forwarded: AtomicUsize,
    send_attempts: AtomicUsize,
    send_ok: AtomicUsize,
    send_fail: AtomicUsize,
    /// G7: count of packets newly stored in the recent-packet cache (store-and-forward).
    recent_stored: AtomicUsize,
    /// G7: count of `isRSR` catch-up replies this node has unicast to a sync requester.
    rsr_sent: AtomicUsize,
    /// G7: count of inbound `isRSR` catch-up packets this node has received.
    rsr_received: AtomicUsize,
}

impl WorkerCtx {
    pub(crate) fn new(
        sender_id: [u8; PEER_ID_LEN],
        radio: Arc<dyn BleRadio>,
        gateway: Option<Arc<dyn MeshGateway>>,
    ) -> Self {
        WorkerCtx {
            sender_id,
            radio,
            gateway,
            peers: Mutex::new(HashSet::new()),
            origin: Mutex::new(OriginShared::default()),
            settled: Condvar::new(),
            seq: AtomicU64::new(1),
            forwarded: AtomicUsize::new(0),
            send_attempts: AtomicUsize::new(0),
            send_ok: AtomicUsize::new(0),
            send_fail: AtomicUsize::new(0),
            recent_stored: AtomicUsize::new(0),
            rsr_sent: AtomicUsize::new(0),
            rsr_received: AtomicUsize::new(0),
        }
    }

    pub(crate) fn add_peer(&self, peer: String) {
        self.peers.lock().unwrap().insert(peer);
    }

    pub(crate) fn remove_peer(&self, peer: &str) {
        self.peers.lock().unwrap().remove(peer);
    }

    #[cfg(test)]
    pub(crate) fn peer_count(&self) -> usize {
        self.peers.lock().unwrap().len()
    }

    /// This node's current peer degree, the input to the degree-adaptive relay decision
    /// ([`RelayPolicy::should_relay`]).
    fn peer_degree(&self) -> usize {
        self.peers.lock().unwrap().len()
    }

    /// Record the async outcome of a fire-and-forget `send` (ADR-0002 gotcha b). Cheap:
    /// just a counter bump, never a re-entrant `send`.
    pub(crate) fn note_send_result(&self, ok: bool) {
        if ok {
            self.send_ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.send_fail.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub(crate) fn forwarded_count(&self) -> usize {
        self.forwarded.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn send_attempts(&self) -> usize {
        self.send_attempts.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn send_ok(&self) -> usize {
        self.send_ok.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn send_fail(&self) -> usize {
        self.send_fail.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn recent_stored(&self) -> usize {
        self.recent_stored.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn rsr_sent(&self) -> usize {
        self.rsr_sent.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn rsr_received(&self) -> usize {
        self.rsr_received.load(Ordering::Relaxed)
    }

    pub(crate) fn status(&self, tx_id: &TxId) -> PaymentStatus {
        self.origin
            .lock()
            .unwrap()
            .payments
            .get(tx_id)
            .copied()
            .unwrap_or(PaymentStatus::Pending)
    }

    /// Block (up to `timeout`) until `tx_id` leaves `Pending`, returning the final status
    /// (or the last-known status on timeout).
    #[cfg(test)]
    pub(crate) fn wait(&self, tx_id: TxId, timeout: Duration) -> PaymentStatus {
        let deadline = Instant::now() + timeout;
        let mut guard = self.origin.lock().unwrap();
        loop {
            let status = guard
                .payments
                .get(&tx_id)
                .copied()
                .unwrap_or(PaymentStatus::Pending);
            if status != PaymentStatus::Pending {
                return status;
            }
            let now = Instant::now();
            if now >= deadline {
                return status;
            }
            let (g, _) = self.settled.wait_timeout(guard, deadline - now).unwrap();
            guard = g;
        }
    }

    fn record_pending(&self, tx_id: TxId) {
        self.origin
            .lock()
            .unwrap()
            .payments
            .entry(tx_id)
            .or_insert(PaymentStatus::Pending);
    }

    fn settle(&self, tx_id: TxId, status: PaymentStatus) {
        let mut g = self.origin.lock().unwrap();
        if let Some(slot) = g.payments.get_mut(&tx_id) {
            if *slot == PaymentStatus::Pending {
                *slot = status;
                drop(g);
                self.settled.notify_all();
            }
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Build a fresh broadcast packet originated by this node.
    fn build_packet(&self, msg_type: MessageType, payload: Vec<u8>) -> Packet {
        let mut p = Packet::new(msg_type, self.sender_id, payload);
        p.recipient_id = Some(BROADCAST_RECIPIENT);
        p.ttl = DEFAULT_TTL;
        p.timestamp_ms = self.next_seq();
        p
    }

    /// Fire-and-forget unicast of `bytes` to a single connected peer — the targeted
    /// delivery a G7 `isRSR` gossip-sync reply uses (never a flood).
    fn unicast(&self, peer: &str, bytes: Vec<u8>) {
        self.send_attempts.fetch_add(1, Ordering::Relaxed);
        self.radio.send(peer.to_string(), bytes);
    }

    /// Fire-and-forget flood of `bytes` to every connected peer. Snapshots the peer set
    /// first so the radio is never called while holding the lock (no re-entrancy hazard).
    fn flood(&self, bytes: Vec<u8>) {
        self.flood_excluding(bytes, None);
    }

    /// Like [`flood`](Self::flood) but skips `exclude` — the peer a relayed packet
    /// arrived from. Never echoing a packet back out its source link (PROTOCOL.md
    /// "source link is excluded") halves needless duplicate airtime on every hop.
    fn flood_excluding(&self, bytes: Vec<u8>, exclude: Option<&str>) {
        let peers: Vec<String> = self.peers.lock().unwrap().iter().cloned().collect();
        for peer in peers {
            if Some(peer.as_str()) == exclude {
                continue;
            }
            self.send_attempts.fetch_add(1, Ordering::Relaxed);
            self.radio.send(peer, bytes.clone());
        }
    }
}

/// The worker thread's persistent per-node state (single-threaded; no locks on the hot
/// path): the dedup caches, the G6 relay [`RelayPolicy`] (degree-adaptive decision +
/// injectable jitter/RNG), the fragment [`Reassembler`], and a monotonic clock used as
/// the reassembler's logical time.
pub(crate) struct WorkerState {
    relay_seen: DedupCache<RelayKey>,
    gateway_seen: DedupCache<TxId>,
    policy: RelayPolicy,
    reassembler: Reassembler,
    /// G7: the bounded recent-packet cache served by GCS gossip-sync.
    recent: RecentCache,
    /// G7: rate-limiter for the 30 s periodic `requestSync` maintenance tick.
    sync: SyncScheduler,
    start: Instant,
}

impl WorkerState {
    pub(crate) fn new(policy: RelayPolicy) -> Self {
        WorkerState {
            relay_seen: DedupCache::new(RELAY_CACHE_CAP),
            gateway_seen: DedupCache::new(GATEWAY_CACHE_CAP),
            policy,
            reassembler: Reassembler::new(),
            recent: RecentCache::new(),
            sync: SyncScheduler::new(),
            start: Instant::now(),
        }
    }

    /// Monotonic milliseconds since this worker started — the reassembler's logical clock.
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

/// G7: remember a packet in the recent-packet cache so this node can later serve it to a
/// peer catching up via gossip-sync. Bumps `recent_stored` only on a fresh insert (the
/// store-and-forward "no duplicates" invariant the rejoin test asserts).
fn remember(ctx: &WorkerCtx, st: &mut WorkerState, packet: &Packet) {
    let now = st.now_ms();
    if st.recent.insert(packet_id(packet), packet.clone(), now) {
        ctx.recent_stored.fetch_add(1, Ordering::Relaxed);
    }
}

/// Origin path: encode a real `nimiqTx` packet around the **opaque** `tx_wire` and flood
/// it. Records the payment as `Pending`. Returns the (mock) `txId`.
pub(crate) fn flood_local_tx(ctx: &WorkerCtx, tx_wire: Vec<u8>, st: &mut WorkerState) -> TxId {
    // G3: `tx_wire` is OPAQUE. Real signed Nimiq tx bytes come from `sign_offline()`
    //     (money-path, gated) and ride this exact `Vec<u8>` path unchanged.
    let tx_id = mock_tx_id(&tx_wire);
    let mut env = NimiqEnvelope::new(tx_wire);
    env.tx_id = Some(tx_id.0);
    env.want_receipt = true;
    let Ok(payload) = encode_envelope(&env) else {
        // Oversized opaque blob (> 255-B TLV value): refuse to flood, stay Pending.
        return tx_id;
    };
    let packet = ctx.build_packet(MessageType::NimiqTx, payload);
    // Remember our own packet so its echo back through the mesh is not re-flooded, and so
    // gossip-sync can later serve it to a peer that was out of range when we flooded it.
    st.relay_seen.insert(relay_key(&packet));
    remember(ctx, st, &packet);
    ctx.record_pending(tx_id);
    if let Ok(bytes) = encode(&packet) {
        ctx.flood(bytes);
    }
    tx_id
}

/// G7: build this node's "what I have" GCS filter and flood a `requestSync` (`0x21`,
/// **ttl 0 → local-only**, never relayed) to direct peers. Each peer replies by unicasting
/// back the packets the filter does not cover, flagged `isRSR`. Driven by an explicit
/// rejoin call (`request_sync`) and by the 30 s maintenance tick.
pub(crate) fn emit_request_sync(ctx: &WorkerCtx, st: &mut WorkerState) {
    let now = st.now_ms();
    let payload = st.recent.build_filter(now).to_bytes();
    let mut packet = ctx.build_packet(MessageType::RequestSync, payload);
    packet.ttl = 0; // local-only: peers receive it but the hop floor stops any relay.
    if let Ok(bytes) = encode(&packet) {
        ctx.flood(bytes);
    }
}

/// G7: the 30 s maintenance tick. Issues a `requestSync` at most once per
/// [`crate::store_forward::SYNC_TICK_MS`] (the scheduler rate-limits on the worker clock).
pub(crate) fn maintenance_tick(ctx: &WorkerCtx, st: &mut WorkerState) {
    let now = st.now_ms();
    if st.sync.due(now) {
        emit_request_sync(ctx, st);
    }
}

/// The hot path: one inbound frame from the radio, with the peer it arrived on (`src`,
/// `None` if the shim couldn't attribute it). Decodes with the real codec and dispatches
/// by message type. Panic-free and infallible — a malformed/hostile frame is silently
/// dropped (the caller additionally wraps this in `catch_unwind`, gotcha c).
pub(crate) fn process_inbound(
    ctx: &WorkerCtx,
    src: Option<&str>,
    bytes: &[u8],
    st: &mut WorkerState,
) {
    let Ok(mut packet) = decode(bytes) else {
        return; // not a well-formed bitmesh packet — drop.
    };
    // G7: an `isRSR` gossip-sync reply is a targeted catch-up for a packet we missed.
    // Deliver it locally only (never re-flood — the unicast already reached us), then it
    // settles / submits / caches through the normal handlers like any other packet.
    if packet.flags.is_rsr {
        ctx.rsr_received.fetch_add(1, Ordering::Relaxed);
        packet.flags.is_rsr = false;
        packet.ttl = 0; // zero TTL → the relay hop floor drops it: delivered, never re-flooded.
        dispatch_packet(ctx, packet, None, st);
        return;
    }
    // G7: a `requestSync` advertises what a peer has; reply with what it is missing. It is
    // ttl-0 local-only and is never relayed onward.
    if packet.msg_type == MessageType::RequestSync {
        handle_request_sync(ctx, packet, src, st);
        return;
    }
    dispatch_packet(ctx, packet, src, st);
}

/// Route a (non-`isRSR`, non-`requestSync`) packet to its type handler. Shared by the
/// inbound path and by locally-delivered reassembled / catch-up packets.
fn dispatch_packet(ctx: &WorkerCtx, packet: Packet, src: Option<&str>, st: &mut WorkerState) {
    match packet.msg_type {
        MessageType::NimiqTx => handle_tx(ctx, packet, src, st),
        MessageType::NimiqTxReceipt => handle_receipt(ctx, packet, src, st),
        // G6: the fragment path — carry the fragment onward and feed the reassembler.
        MessageType::Fragment => handle_fragment(ctx, packet, src, st),
        // HeadBeacon, G11 `noiseEncrypted` (0x11), and any other relayable type: blind
        // dedup + remember + adaptive TTL relay. A `noiseEncrypted` blob is **opaque** to
        // the relay (transport-privacy) — only its two endpoints decrypt it via a Noise
        // `Session`; the mesh just carries the sealed bytes like any flooded packet.
        _ => {
            if st.relay_seen.insert(relay_key(&packet)) {
                remember(ctx, st, &packet);
                relay_onward(ctx, packet, src, st);
            }
        }
    }
}

/// G7: handle an inbound `requestSync`. Decode the peer's GCS "have" filter and unicast
/// back every cached packet **not** covered by it, flagged `isRSR`. Requires the source
/// peer (the unicast target); a source-unattributed sync request is dropped.
fn handle_request_sync(ctx: &WorkerCtx, packet: Packet, src: Option<&str>, st: &mut WorkerState) {
    let Some(src) = src else {
        return; // can't unicast a reply without knowing who asked.
    };
    let Some(filter) = GcsFilter::from_bytes(&packet.payload) else {
        return; // malformed filter — drop.
    };
    let now = st.now_ms();
    for mut missing in st.recent.missing(&filter, now) {
        missing.flags.is_rsr = true;
        if let Ok(bytes) = encode(&missing) {
            ctx.unicast(src, bytes);
            ctx.rsr_sent.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn handle_tx(ctx: &WorkerCtx, packet: Packet, src: Option<&str>, st: &mut WorkerState) {
    // Blind relay dedup first — every role shares it.
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    // G7: cache it for store-and-forward (so we can serve a rejoining peer this tx later).
    remember(ctx, st, &packet);
    // Gateway role: the one endpoint allowed to read the envelope + submit.
    if let Some(gw) = &ctx.gateway {
        if let Ok(env) = decode_envelope(&packet.payload) {
            let tx_id = env
                .tx_id
                .map(TxId)
                .unwrap_or_else(|| mock_tx_id(&env.tx_wire));
            if st.gateway_seen.insert(tx_id) {
                // G8: the real `RpcGateway` validates `networkId` + the validity window
                //     (querying the live head) here, then calls `sendRawTransaction(rawHex)`
                //     against a public Albatross TESTNET RPC (money-path). The mock only
                //     RECORDS the bytes. A transient gateway error yields NO receipt (Err),
                //     so another gateway / a later retry can still carry the tx.
                let submit_ctx = SubmitContext {
                    tx_id,
                    tx_wire: env.tx_wire.clone(),
                    network_id: env.network_id,
                    valid_until: env.valid_until,
                };
                if let Ok(receipt) = gw.submit_validated(submit_ctx) {
                    let payload = encode_receipt(&receipt.tx_id, receipt.status);
                    let reply = ctx.build_packet(MessageType::NimiqTxReceipt, payload);
                    st.relay_seen.insert(relay_key(&reply));
                    if let Ok(reply_bytes) = encode(&reply) {
                        ctx.flood(reply_bytes);
                    }
                }
            }
        }
    }
    // PROTOCOL.md: a gateway relays the tx anyway so other gateways can also try (the
    // mempool dedups on tx hash). A plain relay just forwards.
    relay_onward(ctx, packet, src, st);
}

fn handle_receipt(ctx: &WorkerCtx, packet: Packet, src: Option<&str>, st: &mut WorkerState) {
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    // G7: cache it so an offline ORIGIN rejoining within 15 min catches up on its receipt.
    remember(ctx, st, &packet);
    // Origin role: correlate the receipt to a pending payment (idempotent; a no-op for
    // nodes that aren't the origin of this tx).
    if let Some((tx_id, status)) = decode_receipt(&packet.payload) {
        let payment_status = match status {
            ReceiptStatus::Accepted => PaymentStatus::Settled,
            _ => PaymentStatus::Failed,
        };
        ctx.settle(tx_id, payment_status);
    }
    relay_onward(ctx, packet, src, st);
}

/// G6 fragment path: carry the fragment onward (it's a normal flooded packet) and feed
/// its chunk to the bounded [`Reassembler`]. When a set completes, the original message
/// is reconstructed with **TTL zeroed** (PROTOCOL.md: "reassembled TTL zeroed") and
/// dispatched **locally only** — it is never re-flooded, since the individual fragments
/// already propagated it.
fn handle_fragment(ctx: &WorkerCtx, packet: Packet, src: Option<&str>, st: &mut WorkerState) {
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    // G7: cache the fragment packet so gossip-sync can replay it to a rejoining peer.
    remember(ctx, st, &packet);
    // Parse the chunk before we move `packet` into the relay step.
    let parsed = parse_fragment(&packet.payload);
    let now = st.now_ms();
    relay_onward(ctx, packet, src, st);

    let Some(fragment) = parsed else {
        return; // malformed fragment header — carried but not reassembled.
    };
    if let Some((original_type, payload)) = st.reassembler.accept(fragment, now) {
        if let Some(msg_type) = MessageType::from_u8(original_type) {
            // Reconstruct the original message, TTL zeroed so it is delivered locally and
            // never rebroadcast (the fragments already did the flooding).
            let mut reassembled = Packet::new(msg_type, [0u8; PEER_ID_LEN], payload);
            reassembled.ttl = 0;
            reassembled.timestamp_ms = st.now_ms();
            dispatch_reassembled(ctx, reassembled, st);
        }
    }
}

/// Dispatch a reassembled (TTL-0) message through the normal type handlers. `relay_onward`
/// drops it at the hop floor, so this only ever *delivers* (gateway submit / origin
/// settle), never re-floods. A reassembled fragment-of-a-fragment is impossible
/// (`fragment_message` never wraps a `fragment` payload), so this can't recurse.
fn dispatch_reassembled(ctx: &WorkerCtx, packet: Packet, st: &mut WorkerState) {
    // TTL is already zeroed, so `relay_onward` drops at the hop floor — this only ever
    // *delivers* (gateway submit / origin settle / cache), never re-floods.
    dispatch_packet(ctx, packet, None, st);
}

/// Blind degree-adaptive, jittered, source-excluding TTL re-flood. Never reads the opaque
/// payload. The caller has already dedup'd (`relay_seen`); this decides whether/when to
/// rebroadcast:
///
/// 1. **degree-adaptive** — drop here when a dense-mesh probability roll says so
///    ([`RelayPolicy::should_relay`]);
/// 2. **TTL hop cap** — [`relayed_ttl`] caps then decrements, dropping at the floor (the
///    loop-freedom invariant);
/// 3. **jitter** — a small injected delay before the write desynchronises neighbours
///    (zero under the test policy);
/// 4. **source exclusion** — never echo back out the link it arrived on (`src`).
fn relay_onward(ctx: &WorkerCtx, mut packet: Packet, src: Option<&str>, st: &mut WorkerState) {
    if !st.policy.should_relay(ctx.peer_degree()) {
        return;
    }
    let Some(ttl) = relayed_ttl(packet.ttl) else {
        return;
    };
    packet.ttl = ttl;
    st.policy.apply_jitter();
    if let Ok(bytes) = encode(&packet) {
        ctx.forwarded.fetch_add(1, Ordering::Relaxed);
        ctx.flood_excluding(bytes, src);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Packet;

    #[test]
    fn receipt_payload_round_trips() {
        let id = mock_tx_id(b"hello");
        for status in [
            ReceiptStatus::Accepted,
            ReceiptStatus::Expired,
            ReceiptStatus::Failed,
        ] {
            let bytes = encode_receipt(&id, status);
            assert_eq!(bytes.len(), RECEIPT_PAYLOAD_LEN);
            let (back_id, back_status) = decode_receipt(&bytes).unwrap();
            assert_eq!(back_id, id);
            assert_eq!(back_status, status);
        }
    }

    #[test]
    fn decode_receipt_rejects_wrong_length() {
        assert!(decode_receipt(&[]).is_none());
        assert!(decode_receipt(&[0u8; 32]).is_none());
        assert!(decode_receipt(&[0u8; 34]).is_none());
    }

    #[test]
    fn relay_key_is_header_only_and_payload_blind() {
        let mut a = Packet::new(MessageType::NimiqTx, [1; PEER_ID_LEN], vec![1, 2, 3]);
        a.timestamp_ms = 42;
        let mut b = a.clone();
        b.payload = vec![9, 9, 9, 9]; // different payload …
        assert_eq!(relay_key(&a), relay_key(&b)); // … same blind relay identity.
        b.timestamp_ms = 43;
        assert_ne!(relay_key(&a), relay_key(&b)); // header field changes the key.
    }
}
