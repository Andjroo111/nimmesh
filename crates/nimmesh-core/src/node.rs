//! # node — `MeshNode`, the object the native shim calls *in* to (ADR-0002)
//!
//! The Rust half of the UniFFI foreign-trait pair. The native BLE shim calls a
//! `MeshNode` method on **every** BLE event; the node calls **out** through its
//! `Arc<dyn BleRadio>`. The two design rules that make this safe on iOS/Android:
//!
//! - **`on_packet_received` is NON-BLOCKING (ADR-0002 gotcha a).** On iOS, doing real
//!   work here would re-enter CoreBluetooth's own dispatch queue. So it does the
//!   absolute minimum — push the bytes onto an internal channel and return. A dedicated
//!   **worker thread** drains the queue and runs the heavy decode → dedup → TTL-relay →
//!   gateway logic ([`crate::engine`]), calling `radio.send` **off** the callback thread.
//! - **The node→radio edge is strong; the radio→node edge is weak (gotcha d).** The node
//!   owns the radio; the shim/mock radio holds the node weakly. On teardown the node
//!   stops its worker, releases the radio, and is reclaimed with no leaked BLE handle.
//!
//! The worker wraps each job in `catch_unwind` (gotcha c) so a panic on a single hostile
//! frame can never abort the process or wedge the mesh — the hot path is infallible.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
#[cfg(test)]
use std::time::Duration;

use crate::balance::{flood_local_balance_query, CachedBalance};
use crate::beacon::emit_head_beacon;
use crate::citizen::{relay_stats, RelayStats};
use crate::engine::{
    emit_request_sync, flood_local_tx, maintenance_tick, process_inbound, WorkerCtx, WorkerState,
};
use crate::gateway::MeshGateway;
use crate::nimiq::address::Address;
use crate::packet::PEER_ID_LEN;
use crate::radio::BleRadio;
use crate::relay::RelayPolicy;
use crate::settlement::{PaymentStatus, Settlement};
use crate::swap_session::SwapSession;
use crate::transport::{mock_tx_id, TxId};

/// A unit of work handed from a BLE callback to the worker thread.
enum Job {
    /// Bytes that arrived on the radio, plus the peer they came from (`None` if the shim
    /// couldn't attribute the source — see `on_packet_received` vs the `_from` variant).
    Inbound {
        /// The connected peer the bytes arrived on (drives G6 source-link exclusion).
        src: Option<String>,
        /// The raw inbound frame.
        bytes: Vec<u8>,
    },
    /// A locally-originated tx to flood (`submit_local_tx`).
    LocalTx(Vec<u8>),
    /// G7: force-issue a `requestSync` now (e.g. just rejoined the mesh).
    RequestSync,
    /// G7: a maintenance poll — issues a `requestSync` only if the 30 s tick is due.
    SyncTick,
    /// G9: a beacon poll — a gateway floods a `nimiqHeadBeacon` only if the tick is due.
    BeaconTick,
    /// G15: flood a `nimiqBalanceQuery` for this address (asks any gateway for its balance).
    BalanceQuery(Address),
    /// G14 (test): register an initiator coordinator + flood its `Propose` (swap origination).
    #[cfg(test)]
    StartSwap {
        swap_id: [u8; crate::swap_wire::SWAP_ID_LEN],
        coordinator: Box<crate::swap_coordinator::SwapCoordinator>,
        propose: Box<crate::swap_wire::SwapEnvelope>,
    },
    /// Drain and exit (teardown).
    Shutdown,
}

/// Widen an FFI byte id into the fixed 8-byte protocol `senderID` (truncate/zero-pad).
fn to_sender_id(bytes: &[u8]) -> [u8; PEER_ID_LEN] {
    let mut id = [0u8; PEER_ID_LEN];
    let n = bytes.len().min(PEER_ID_LEN);
    id[..n].copy_from_slice(&bytes[..n]);
    id
}

/// Widen an FFI byte id into a 32-byte [`TxId`] (truncate/zero-pad).
fn to_tx_id(bytes: &[u8]) -> TxId {
    let mut id = [0u8; 32];
    let n = bytes.len().min(32);
    id[..n].copy_from_slice(&bytes[..n]);
    TxId(id)
}

/// The mesh node: the brain behind the BLE radio. Constructed with an `Arc<dyn BleRadio>`
/// it drives; torn down with [`MeshNode::shutdown`] (also run on drop).
#[derive(uniffi::Object)]
pub struct MeshNode {
    ctx: Arc<WorkerCtx>,
    /// `Mutex` makes the `Sender` `Sync` (UniFFI objects must be `Sync`) and lets
    /// shutdown drop it. `None` once shut down.
    job_tx: Mutex<Option<Sender<Job>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    running: AtomicBool,
}

/// The worker thread: drain jobs and run the (heavy) protocol logic off the radio's
/// callback thread. Each job is wrapped in `catch_unwind` so a panic on one frame can't
/// kill the worker or the process (ADR-0002 gotcha c).
fn run_worker(
    ctx: Arc<WorkerCtx>,
    rx: Receiver<Job>,
    policy: RelayPolicy,
    verify_before_relay: bool,
    swap: Option<SwapSession>,
    signer: Option<Box<dyn crate::swap_signer::SwapSigner>>,
) {
    let mut st = WorkerState::new(policy, verify_before_relay, swap, signer);
    while let Ok(job) = rx.recv() {
        match job {
            Job::Shutdown => break,
            Job::Inbound { src, bytes } => {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    process_inbound(&ctx, src.as_deref(), &bytes, &mut st);
                }));
            }
            Job::LocalTx(wire) => {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    flood_local_tx(&ctx, wire, &mut st);
                }));
            }
            Job::RequestSync => {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    emit_request_sync(&ctx, &mut st);
                }));
            }
            Job::SyncTick => {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    maintenance_tick(&ctx, &mut st);
                    // G16: shed terminal/stale swaps on the same maintenance cadence.
                    crate::swap_node::gc_tick(&ctx, &mut st);
                }));
            }
            Job::BeaconTick => {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    emit_head_beacon(&ctx, &mut st);
                }));
            }
            Job::BalanceQuery(addr) => {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    flood_local_balance_query(&ctx, addr, &mut st);
                }));
            }
            #[cfg(test)]
            Job::StartSwap {
                swap_id,
                coordinator,
                propose,
            } => {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    crate::swap_node::start_swap(&ctx, swap_id, *coordinator, *propose, &mut st);
                }));
            }
        }
    }
}

// --- FFI surface (the shim calls these) ------------------------------------------

#[uniffi::export]
impl MeshNode {
    /// Create a node driving `radio` and bring the radio up (advertise + scan).
    ///
    /// `sender_id` is the 8-byte protocol id (truncated/zero-padded). The returned
    /// `Arc<MeshNode>` is the handle the shim holds **weakly**.
    #[uniffi::constructor]
    pub fn new(sender_id: Vec<u8>, radio: Arc<dyn BleRadio>) -> Arc<Self> {
        // Production verifies every `nimiqTx` before relaying (G12 spam filter, RISKS.md #4).
        Self::build(
            sender_id,
            radio,
            None,
            RelayPolicy::production(),
            true,
            None,
            None,
        )
    }

    /// A peer connected (`CBPeripheral` / GATT link up). Cheap: record it as a flood
    /// target.
    pub fn on_peer_connected(&self, peer_id: String) {
        self.ctx.add_peer(peer_id);
    }

    /// A peer disconnected. Cheap: stop flooding to it.
    pub fn on_peer_disconnected(&self, peer_id: String) {
        self.ctx.remove_peer(&peer_id);
    }

    /// Bytes arrived from a peer. **NON-BLOCKING (ADR-0002 gotcha a):** only enqueue and
    /// return — no decode, no dedup, and never a synchronous `radio.send`. The worker
    /// thread does all of that off this callback thread.
    ///
    /// Source-unattributed: prefer [`on_packet_received_from`](Self::on_packet_received_from)
    /// when the shim knows the originating peer, so the worker can apply G6 source-link
    /// exclusion (never echo a relay back out the link it arrived on).
    pub fn on_packet_received(&self, bytes: Vec<u8>) {
        self.enqueue_inbound(None, bytes);
    }

    /// Like [`on_packet_received`](Self::on_packet_received) but records the connected
    /// `peer_id` the bytes arrived on, so the relay excludes that source link when it
    /// rebroadcasts (PROTOCOL.md). Equally non-blocking.
    pub fn on_packet_received_from(&self, peer_id: String, bytes: Vec<u8>) {
        self.enqueue_inbound(Some(peer_id), bytes);
    }

    /// The async outcome of a fire-and-forget `radio.send` (ADR-0002 gotcha b). Cheap:
    /// records the outcome; never re-enters the radio.
    pub fn on_send_result(&self, peer_id: String, ok: bool) {
        let _ = peer_id;
        self.ctx.note_send_result(ok);
    }

    /// Originate a payment from an already-signed [`SignedTransfer`] (G3). The
    /// `raw_hex` — produced by either `KeyOrigin` (the app-enclave [`AppSigner`] or the
    /// delegated Nimiq Pay / Hub [`DelegatedSigner`]) — is decoded to the self-contained
    /// ~139-byte wire blob and flooded as a real `nimiqTx`. Returns the 32-byte `txId` to
    /// poll with [`MeshNode::payment_status`], or an empty vec if the blob is not valid hex.
    /// **Still flows downstream as opaque bytes** — the mesh never inspects the payload.
    pub fn submit_signed_transfer(&self, signed_transfer: crate::nimiq::SignedTransfer) -> Vec<u8> {
        match crate::nimiq::signer::signed_transfer_wire(&signed_transfer) {
            Ok(wire) => self.submit_local_tx(wire),
            Err(_) => Vec::new(),
        }
    }

    /// Originate a payment: flood an opaque signed-tx blob as a real `nimiqTx` and track it
    /// until a receipt settles it. Returns the (mock) 32-byte `txId` to poll with
    /// [`MeshNode::payment_status`]. The flood itself runs on the worker thread.
    ///
    /// G3: prefer [`MeshNode::submit_signed_transfer`], which takes a typed
    /// [`crate::nimiq::SignedTransfer`] straight from a `KeyOrigin`; this raw-bytes entry
    /// point remains for callers that already hold the serialized wire (and the test harness).
    pub fn submit_local_tx(&self, tx_wire: Vec<u8>) -> Vec<u8> {
        // The bytes are opaque to the mesh; only a gateway (G8) ever parses them. Compute the
        // id eagerly so the caller gets it now; the worker recomputes the same id when it floods.
        let tx_id = mock_tx_id(&tx_wire);
        if let Some(tx) = self.job_tx.lock().unwrap().as_ref() {
            let _ = tx.send(Job::LocalTx(tx_wire));
        }
        tx_id.0.to_vec()
    }

    /// The current status of a payment by its `txId` bytes (non-blocking). Works for both a
    /// sent (`Outgoing`) and a watched incoming (`Incoming`) payment.
    pub fn payment_status(&self, tx_id: Vec<u8>) -> PaymentStatus {
        self.ctx.status(&to_tx_id(&tx_id))
    }

    /// G17: the full closure state (status + Outgoing/Incoming direction) of a tracked tx, or
    /// `None` if this node isn't tracking it. Lets the UI say "your payment landed" vs "you got
    /// paid" from the one shared receipt. Non-blocking, read-only.
    pub fn settlement(&self, tx_id: Vec<u8>) -> Option<Settlement> {
        self.ctx.settlement(&to_tx_id(&tx_id))
    }

    /// G17: watch for an **incoming** payment as the payee — register the `txId` of a payment
    /// this node expects (learned via the request / confirmation flow). The same gateway
    /// `nimiqTxReceipt` (G8) that settles the sender then closes this node's "did I get paid?"
    /// too (delivered live, or caught up via G7 store-and-forward if it was offline). Tracks a
    /// public txId only — no keys, and the relay stays blind. Non-blocking.
    pub fn watch_incoming(&self, tx_id: Vec<u8>) {
        self.ctx.record_incoming(to_tx_id(&tx_id));
    }

    /// G7: force-issue a gossip-sync round now — advertise what this node has as a GCS
    /// filter so peers unicast back whatever it is missing. Call on **rejoining** the mesh
    /// (a node that was out of range / offline) to catch up on packets within the 15-min
    /// retention window. **Non-blocking (ADR-0002 gotcha a):** only enqueues.
    pub fn request_sync(&self) {
        if let Some(tx) = self.job_tx.lock().unwrap().as_ref() {
            let _ = tx.send(Job::RequestSync);
        }
    }

    /// G7: the periodic maintenance poll. The shim calls this on a timer; it issues a
    /// `requestSync` only when the 30 s tick is actually due (the worker rate-limits it).
    /// **Non-blocking:** only enqueues.
    pub fn poll_sync(&self) {
        if let Some(tx) = self.job_tx.lock().unwrap().as_ref() {
            let _ = tx.send(Job::SyncTick);
        }
    }

    /// G9: a **gateway** floods a `nimiqHeadBeacon` (`0x32`) with its freshest head height
    /// so deep-offline signers anchor `validityStartHeight` to a current head (RISKS.md #1).
    /// The shim calls this on a timer; it emits only when the beacon tick is due (the worker
    /// rate-limits it) and only on a gateway node — a plain node enqueues a harmless no-op.
    /// **Non-blocking (ADR-0002 gotcha a):** only enqueues; the worker sources the height
    /// from the gateway RPC and floods off the callback thread.
    pub fn poll_beacon(&self) {
        if let Some(tx) = self.job_tx.lock().unwrap().as_ref() {
            let _ = tx.send(Job::BeaconTick);
        }
    }

    /// G9: the freshest chain-head height this node has heard via a `nimiqHeadBeacon`
    /// (`0x32`), or `None` before any beacon has arrived. The signer anchors
    /// `validityStartHeight` to this — a deep-offline signer uses the freshest head it has
    /// heard, never a stale one (non-blocking read).
    pub fn cached_head_height(&self) -> Option<u32> {
        self.ctx.cached_head()
    }

    /// G15: ask the mesh for `address`'s balance — flood a `nimiqBalanceQuery` (`0x33`) that any
    /// internet-bearing gateway answers with a `nimiqBalanceResponse` (`0x34`), which
    /// [`MeshNode::cached_balance`] then reads. `address` is the user-friendly `NQ…` form; an
    /// unparseable address is a no-op. **Non-blocking (ADR-0002 gotcha a):** only enqueues; the
    /// worker floods off the callback thread. Read-only — carries only a public address, no keys.
    pub fn query_balance(&self, address: String) {
        let Ok(addr) = Address::from_user_friendly(&address) else {
            return; // not a valid Nimiq address — nothing to ask.
        };
        if let Some(tx) = self.job_tx.lock().unwrap().as_ref() {
            let _ = tx.send(Job::BalanceQuery(addr));
        }
    }

    /// G15: the last-known balance for `address` heard over the mesh, or `None` before any
    /// `nimiqBalanceResponse` has arrived (or for an unparseable address). **Unverified /
    /// last-known**: a relay is untrusted, so the UI must show it as such — the returned
    /// `head_height` drives a "synced X ago" freshness stamp (compare to
    /// [`MeshNode::cached_head_height`]). A trustless accounts-proof is the follow-up. Non-blocking.
    pub fn cached_balance(&self, address: String) -> Option<CachedBalance> {
        let addr = Address::from_user_friendly(&address).ok()?;
        self.ctx.cached_balance(&addr)
    }

    /// G16: the honest "will it send?" answer from this node's live mesh state — `Online`
    /// (a gateway is reachable: this node has internet, or it has heard a gateway head beacon
    /// with peers connected), `Meshed` (peers but no gateway yet — a send relays + waits via
    /// store-and-forward), or `Offline` (no peers — queued until a device is in range).
    /// Read-only, non-blocking; reads only public mesh state, no keys.
    pub fn reachability(&self) -> crate::reachability::Reachability {
        crate::reachability::assess_reachability(
            self.ctx.gateway.is_some(),
            self.ctx.peer_degree() as u32,
            self.ctx.cached_head().is_some(),
        )
    }

    /// G16: blocks remaining before a signed tx's validity window closes, judged against the
    /// freshest head this node has heard (G9). `None` if it can't be judged (no head heard, or
    /// no `valid_until`); `Some(0)` once expired. Pair with `secs_until_expiry` for a "~N min
    /// left" stamp and the re-sign nudge near expiry. Read-only, non-blocking, no keys.
    pub fn blocks_until_expiry(&self, valid_until: u32) -> Option<u32> {
        crate::reachability::blocks_until_expiry(self.ctx.cached_head(), Some(valid_until))
    }

    /// G16: the validity countdown as an estimated number of seconds (~1 block/s). `None` when
    /// it can't be judged. Non-blocking, read-only.
    pub fn secs_until_expiry(&self, valid_until: u32) -> Option<u32> {
        crate::reachability::secs_until_expiry(self.ctx.cached_head(), Some(valid_until))
    }

    /// G20: report this device's battery so the relay can be a **good citizen without
    /// costing the user**. The native shim calls this on battery changes (iOS `UIDevice`,
    /// Android `BatteryManager`): the relay then carries everything while charging / high,
    /// damps its fanout when mid, carries only payment-critical traffic when low, and stops
    /// relaying for others when critical — the user's own send/receive always work. `level_pct`
    /// is clamped to 0..=100. **Non-blocking** (writes shared state the worker reads); no keys.
    pub fn set_battery(&self, level_pct: u8, charging: bool) {
        self.ctx.citizen.set_battery(level_pct, charging);
    }

    /// G20: the good-citizen relay stats for the UI — "you helped N payments reach the
    /// network" (`payments_relayed`), total packets carried, and the current battery-derived
    /// `posture`. Read-only; counts only what this node rebroadcast for others. Non-blocking.
    pub fn relay_stats(&self) -> RelayStats {
        relay_stats(&self.ctx)
    }

    /// G9: build a [`crate::nimiq::TransferIntent`] anchored to the freshest cached head
    /// (`validityStartHeight = latest beacon height`), ready to hand to a `KeyOrigin` signer.
    /// Returns `None` when no beacon has been heard yet — refusing to pre-date a tx to a
    /// stale/zero head (RISKS.md #1: "set `validityStartHeight = latest known head`; never
    /// pre-date"). Testnet by default; this constructs no signature and touches no key.
    pub fn anchored_intent(
        &self,
        recipient: String,
        value: u64,
    ) -> Option<crate::nimiq::TransferIntent> {
        let head = self.ctx.cached_head()?;
        Some(crate::nimiq::TransferIntent {
            recipient,
            value,
            validity_start_height: head,
            network: crate::default_network(),
        })
    }

    /// Tear the node down: stop the worker and release the radio. Idempotent; also runs
    /// on drop (the weak edge that breaks the refcount cycle — ADR-0002 gotcha d).
    pub fn shutdown(&self) {
        self.do_shutdown();
    }
}

// --- Internal + test-facing surface (not exported across FFI) --------------------

impl MeshNode {
    /// Non-blocking enqueue shared by both inbound entry points.
    fn enqueue_inbound(&self, src: Option<String>, bytes: Vec<u8>) {
        if let Some(tx) = self.job_tx.lock().unwrap().as_ref() {
            let _ = tx.send(Job::Inbound { src, bytes });
        }
    }

    /// Shared constructor for the plain and gateway-enabled nodes. The `policy` carries
    /// the G6 relay tunables + injected RNG/jitter (production = real jitter + time seed;
    /// the harness/tests inject [`RelayPolicy::deterministic`] = zero sleep, fixed seed).
    pub(crate) fn build(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        gateway: Option<Arc<dyn MeshGateway>>,
        policy: RelayPolicy,
        verify_before_relay: bool,
        swap: Option<SwapSession>,
        signer: Option<Box<dyn crate::swap_signer::SwapSigner>>,
    ) -> Arc<Self> {
        let ctx = Arc::new(WorkerCtx::new(
            to_sender_id(&sender_id),
            radio.clone(),
            gateway,
        ));
        let (tx, rx) = channel();
        let worker_ctx = ctx.clone();
        let worker = std::thread::spawn(move || {
            run_worker(worker_ctx, rx, policy, verify_before_relay, swap, signer)
        });
        // Bring the radio up. Real BLE starts advertising + scanning concurrently.
        radio.start_advertising();
        radio.start_scanning();
        Arc::new(MeshNode {
            ctx,
            job_tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            running: AtomicBool::new(true),
        })
    }

    /// Build a plain node with a caller-chosen relay policy (the harness/tests pass
    /// [`RelayPolicy::deterministic`] so there are no real jitter sleeps).
    pub(crate) fn new_with_policy(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        policy: RelayPolicy,
    ) -> Arc<Self> {
        // Verify off: the harness floods opaque stand-in bytes (not real signed txs).
        Self::build(sender_id, radio, None, policy, false, None, None)
    }

    /// G14 (test): a swap **participant** node — a plain node that also runs a [`SwapSession`] for
    /// `identity`, with the default sim signer (`MockSigner`), so it decodes its own `swap_id` off
    /// the swap stream, builds its tx bytes via the signer seam, and floods replies.
    #[cfg(test)]
    pub(crate) fn new_participant(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        policy: RelayPolicy,
        identity: crate::swap_session::NodeIdentity,
        ladder: crate::swap::LadderParams,
    ) -> Arc<Self> {
        Self::new_participant_with_signer(
            sender_id,
            radio,
            policy,
            identity,
            ladder,
            Box::new(crate::swap_signer::MockSigner),
        )
    }

    /// G26 (test): a participant node with a caller-supplied [`crate::swap_signer::SwapSigner`] —
    /// proving the signer seam is pluggable (a different signer drops in unchanged).
    #[cfg(test)]
    pub(crate) fn new_participant_with_signer(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        policy: RelayPolicy,
        identity: crate::swap_session::NodeIdentity,
        ladder: crate::swap::LadderParams,
        signer: Box<dyn crate::swap_signer::SwapSigner>,
    ) -> Arc<Self> {
        let session = SwapSession::new(identity, ladder);
        Self::build(
            sender_id,
            radio,
            None,
            policy,
            false,
            Some(session),
            Some(signer),
        )
    }

    /// Build a gateway node with a caller-chosen relay policy (deterministic in tests).
    pub(crate) fn new_gateway_with_policy(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        gateway: Arc<dyn MeshGateway>,
        policy: RelayPolicy,
    ) -> Arc<Self> {
        Self::build(sender_id, radio, Some(gateway), policy, false, None, None)
    }

    /// G12 harness helper: a verifying plain node (verify-before-relay ON) — used by the
    /// spam-filter e2e test, which floods real signed transfers + a junk one.
    pub(crate) fn new_with_policy_verifying(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        policy: RelayPolicy,
    ) -> Arc<Self> {
        Self::build(sender_id, radio, None, policy, true, None, None)
    }

    fn do_shutdown(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return; // already shut down — idempotent.
        }
        if let Some(tx) = self.job_tx.lock().unwrap().take() {
            let _ = tx.send(Job::Shutdown);
        }
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
        // Release the radio (gotcha d). Safe to call even mid-flight; `send` is f-a-f.
        self.ctx.radio.stop();
    }

    /// Block until `tx_id` settles (test helper).
    #[cfg(test)]
    pub(crate) fn wait_payment(&self, tx_id: &[u8], timeout: Duration) -> PaymentStatus {
        self.ctx.wait(to_tx_id(tx_id), timeout)
    }

    /// G14 (test): originate a swap — register the initiator `coordinator` + flood its `Propose`.
    #[cfg(test)]
    pub(crate) fn start_swap(
        &self,
        swap_id: [u8; crate::swap_wire::SWAP_ID_LEN],
        coordinator: crate::swap_coordinator::SwapCoordinator,
        propose: crate::swap_wire::SwapEnvelope,
    ) {
        if let Some(tx) = self.job_tx.lock().unwrap().as_ref() {
            let _ = tx.send(Job::StartSwap {
                swap_id,
                coordinator: Box::new(coordinator),
                propose: Box::new(propose),
            });
        }
    }

    /// G14 (test): this node's phase for a swap it participates in (reads the observable mirror).
    #[cfg(test)]
    pub(crate) fn swap_phase(
        &self,
        swap_id: [u8; crate::swap_wire::SWAP_ID_LEN],
    ) -> Option<crate::swap::SwapPhase> {
        crate::swap_node::swap_phase(&self.ctx, swap_id)
    }

    /// How many packets this node has relayed onward (test/observability hook).
    #[cfg(test)]
    pub(crate) fn forwarded_count(&self) -> usize {
        self.ctx.forwarded_count()
    }
    /// How many `radio.send` writes this node has attempted.
    #[cfg(test)]
    pub(crate) fn send_attempts(&self) -> usize {
        self.ctx.send_attempts()
    }
    /// How many sends were reported delivered via `on_send_result`.
    #[cfg(test)]
    pub(crate) fn send_ok(&self) -> usize {
        self.ctx.send_ok()
    }
    /// How many sends were reported failed via `on_send_result`.
    #[cfg(test)]
    pub(crate) fn send_fail(&self) -> usize {
        self.ctx.send_fail()
    }
    /// Currently-connected peer count.
    #[cfg(test)]
    pub(crate) fn connected_peers(&self) -> usize {
        self.ctx.peer_count()
    }
    /// G7: packets newly stored in this node's recent-packet (store-and-forward) cache.
    #[cfg(test)]
    pub(crate) fn recent_stored(&self) -> usize {
        self.ctx.recent_stored()
    }
    /// G7: `isRSR` catch-up replies this node has unicast to sync requesters.
    #[cfg(test)]
    pub(crate) fn rsr_sent(&self) -> usize {
        self.ctx.rsr_sent()
    }
    /// G7: inbound `isRSR` catch-up packets this node has received.
    #[cfg(test)]
    pub(crate) fn rsr_received(&self) -> usize {
        self.ctx.rsr_received()
    }
    /// G9: `nimiqHeadBeacon` frames this gateway has flooded.
    #[cfg(test)]
    pub(crate) fn beacon_emitted(&self) -> usize {
        self.ctx.beacon_emitted()
    }
    /// G9: `nimiqTx` packets dropped (GC'd) for a closed validity window.
    #[cfg(test)]
    pub(crate) fn expired_dropped(&self) -> usize {
        self.ctx.expired_dropped()
    }
    /// G12: `nimiqTx` packets dropped by the verify-before-relay spam filter.
    #[cfg(test)]
    pub(crate) fn verify_dropped(&self) -> usize {
        self.ctx.verify_dropped()
    }
    /// G12: inbound frames dropped because the source peer exceeded its rate limit.
    #[cfg(test)]
    pub(crate) fn rate_limited(&self) -> usize {
        self.ctx.rate_limited()
    }
    /// G12: `nimiqTx` packets not re-carried because their txId was already ACKed.
    #[cfg(test)]
    pub(crate) fn stop_after_ack(&self) -> usize {
        self.ctx.stop_after_ack()
    }
    /// G15: `nimiqBalanceResponse` frames this gateway has answered + flooded.
    #[cfg(test)]
    pub(crate) fn balance_answered(&self) -> usize {
        self.ctx.balance_answered()
    }
    /// G15: the last-known cached balance for a user-friendly address (test read).
    #[cfg(test)]
    pub(crate) fn test_cached_balance(&self, address: &str) -> Option<CachedBalance> {
        let addr = Address::from_user_friendly(address).ok()?;
        self.ctx.cached_balance(&addr)
    }
}

impl Drop for MeshNode {
    fn drop(&mut self) {
        self.do_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_helpers_truncate_and_pad() {
        assert_eq!(to_sender_id(&[1, 2, 3]), [1, 2, 3, 0, 0, 0, 0, 0]);
        assert_eq!(to_sender_id(&[9; 12]), [9; 8]);
        let id = to_tx_id(&[7; 4]);
        assert_eq!(&id.0[..4], &[7, 7, 7, 7]);
        assert_eq!(id.0[4], 0);
    }
}
