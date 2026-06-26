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

use crate::engine::{
    emit_head_beacon, emit_request_sync, flood_local_tx, maintenance_tick, process_inbound,
    PaymentStatus, WorkerCtx, WorkerState,
};
use crate::gateway::MeshGateway;
use crate::packet::PEER_ID_LEN;
use crate::radio::BleRadio;
use crate::relay::RelayPolicy;
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
fn run_worker(ctx: Arc<WorkerCtx>, rx: Receiver<Job>, policy: RelayPolicy) {
    let mut st = WorkerState::new(policy);
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
                }));
            }
            Job::BeaconTick => {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    emit_head_beacon(&ctx, &mut st);
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
        Self::build(sender_id, radio, None, RelayPolicy::production())
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
    pub fn submit_signed_transfer(&self, signed: crate::nimiq::SignedTransfer) -> Vec<u8> {
        match crate::nimiq::signer::signed_transfer_wire(&signed) {
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

    /// The current status of a payment by its `txId` bytes (non-blocking).
    pub fn payment_status(&self, tx_id: Vec<u8>) -> PaymentStatus {
        self.ctx.status(&to_tx_id(&tx_id))
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
    ) -> Arc<Self> {
        let ctx = Arc::new(WorkerCtx::new(
            to_sender_id(&sender_id),
            radio.clone(),
            gateway,
        ));
        let (tx, rx) = channel();
        let worker_ctx = ctx.clone();
        let worker = std::thread::spawn(move || run_worker(worker_ctx, rx, policy));
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
        Self::build(sender_id, radio, None, policy)
    }

    /// Build a gateway node with a caller-chosen relay policy (deterministic in tests).
    pub(crate) fn new_gateway_with_policy(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        gateway: Arc<dyn MeshGateway>,
        policy: RelayPolicy,
    ) -> Arc<Self> {
        Self::build(sender_id, radio, Some(gateway), policy)
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
