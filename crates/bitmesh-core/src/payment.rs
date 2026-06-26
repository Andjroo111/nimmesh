//! # payment — the origin → relay → gateway → receipt orchestrator
//!
//! Ties the transport + gateway seams into the offline pay-loop the whole project
//! exists for (see `docs/GOAL.md`, "The demo loop"):
//!
//! 1. An **origin** (airplane mode) floods an opaque signed-tx blob as a `Tx` frame.
//! 2. One or more **relays** decrement TTL, dedup, and re-flood — never inspecting the
//!    opaque payload (core value #3, trustless relay).
//! 3. A **gateway** submits the bytes (records them, in the mock) and floods a
//!    `Receipt` frame back.
//! 4. The receipt floods back to the origin, whose payment flips `Pending → Settled`.
//!
//! The payload is an **opaque `Vec<u8>`** throughout. G3 fills it with real signed
//! Nimiq tx bytes from `sign_offline()` (money-path, gated) — see the `// G3:` anchor.
//! The framing is the temporary `MeshFrame` (G4 replaces it with the real codec).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::gateway::{MeshGateway, ReceiptStatus};
use crate::transport::{
    mock_tx_id, FrameKind, MeshError, MeshFrame, MeshTransport, NodeId, PacketHandler, TxId,
    DEFAULT_TTL,
};

/// Where a payment stands from the origin's point of view.
///
/// `Pending` until a gateway receipt arrives; honours unconfirmed-until-inclusion —
/// only an `Accepted` receipt yields `Settled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentStatus {
    /// Sent (or relaying); no gateway receipt seen yet.
    Pending,
    /// A gateway accepted the tx into the mempool.
    Settled,
    /// A gateway rejected the tx (expired / failed).
    Failed,
}

struct OriginShared {
    payments: HashMap<TxId, PaymentStatus>,
    seen: HashSet<(u8, TxId)>,
}

/// The origin endpoint: starts payments and settles them when receipts return.
pub struct OriginHandler {
    transport: Weak<dyn MeshTransport>,
    shared: Mutex<OriginShared>,
    settled: Condvar,
}

impl OriginHandler {
    fn new(transport: Weak<dyn MeshTransport>) -> Self {
        OriginHandler {
            transport,
            shared: Mutex::new(OriginShared {
                payments: HashMap::new(),
                seen: HashSet::new(),
            }),
            settled: Condvar::new(),
        }
    }

    fn start_payment(&self, tx_wire: Vec<u8>) -> TxId {
        // G3: `tx_wire` is OPAQUE here. The real signed Nimiq tx bytes come from
        //     `sign_offline()` (money-path, gated) and ride this exact `Vec<u8>` path.
        let tx_id = mock_tx_id(&tx_wire);
        let frame = MeshFrame {
            kind: FrameKind::Tx,
            ttl: DEFAULT_TTL,
            status: 0,
            tx_id,
            payload: tx_wire,
        };
        {
            let mut s = self.shared.lock().unwrap();
            s.payments.insert(tx_id, PaymentStatus::Pending);
            s.seen.insert(frame.dedup_key());
        }
        if let Some(t) = self.transport.upgrade() {
            let _ = t.broadcast(frame.encode());
        }
        tx_id
    }

    fn status(&self, tx_id: &TxId) -> PaymentStatus {
        self.shared
            .lock()
            .unwrap()
            .payments
            .get(tx_id)
            .copied()
            .unwrap_or(PaymentStatus::Pending)
    }

    fn wait(&self, tx_id: TxId, timeout: Duration) -> PaymentStatus {
        let deadline = Instant::now() + timeout;
        let mut guard = self.shared.lock().unwrap();
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
}

impl PacketHandler for OriginHandler {
    fn on_packet(&self, _from: NodeId, packet: Vec<u8>) {
        let Some(frame) = MeshFrame::decode(&packet) else {
            return;
        };
        if frame.kind != FrameKind::Receipt {
            return;
        }
        let mut s = self.shared.lock().unwrap();
        if !s.seen.insert(frame.dedup_key()) {
            return; // already processed this receipt
        }
        if let Some(slot) = s.payments.get_mut(&frame.tx_id) {
            *slot = match ReceiptStatus::from_code(frame.status) {
                ReceiptStatus::Accepted => PaymentStatus::Settled,
                _ => PaymentStatus::Failed,
            };
            drop(s);
            self.settled.notify_all();
        }
    }
}

/// A blind relay: dedup, TTL-decrement, re-flood. Never inspects the opaque payload.
pub struct RelayHandler {
    transport: Weak<dyn MeshTransport>,
    seen: Mutex<HashSet<(u8, TxId)>>,
    forwarded: Mutex<usize>,
}

impl RelayHandler {
    fn new(transport: Weak<dyn MeshTransport>) -> Self {
        RelayHandler {
            transport,
            seen: Mutex::new(HashSet::new()),
            forwarded: Mutex::new(0),
        }
    }
}

impl PacketHandler for RelayHandler {
    fn on_packet(&self, _from: NodeId, packet: Vec<u8>) {
        let Some(mut frame) = MeshFrame::decode(&packet) else {
            return;
        };
        if !self.seen.lock().unwrap().insert(frame.dedup_key()) {
            return; // already relayed this one
        }
        // PROTOCOL.md hop cap: drop at ttl <= 1. The payload is never read here.
        if frame.ttl <= 1 {
            return;
        }
        frame.ttl -= 1;
        if let Some(t) = self.transport.upgrade() {
            if t.broadcast(frame.encode()).is_ok() {
                *self.forwarded.lock().unwrap() += 1;
            }
        }
    }
}

/// A gateway endpoint: submits `Tx` frames once, floods receipts, relays both onward.
pub struct GatewayHandler {
    transport: Weak<dyn MeshTransport>,
    gateway: Arc<dyn MeshGateway>,
    seen: Mutex<HashSet<(u8, TxId)>>,
}

impl GatewayHandler {
    fn new(transport: Weak<dyn MeshTransport>, gateway: Arc<dyn MeshGateway>) -> Self {
        GatewayHandler {
            transport,
            gateway,
            seen: Mutex::new(HashSet::new()),
        }
    }

    fn relay_onward(&self, mut frame: MeshFrame) {
        if frame.ttl <= 1 {
            return;
        }
        frame.ttl -= 1;
        if let Some(t) = self.transport.upgrade() {
            let _ = t.broadcast(frame.encode());
        }
    }
}

impl PacketHandler for GatewayHandler {
    fn on_packet(&self, _from: NodeId, packet: Vec<u8>) {
        let Some(frame) = MeshFrame::decode(&packet) else {
            return;
        };
        if !self.seen.lock().unwrap().insert(frame.dedup_key()) {
            return; // dedup: submit / relay each frame at most once
        }
        match frame.kind {
            FrameKind::Tx => {
                // Submit exactly once (G8 seam: real sendRawTransaction lives behind
                // MeshGateway). The mock records the opaque bytes.
                if let Ok(receipt) = self.gateway.submit(frame.payload.clone()) {
                    let reply = MeshFrame {
                        kind: FrameKind::Receipt,
                        ttl: DEFAULT_TTL,
                        status: receipt.status.code(),
                        tx_id: receipt.tx_id,
                        payload: Vec::new(),
                    };
                    if let Some(t) = self.transport.upgrade() {
                        let _ = t.broadcast(reply.encode());
                    }
                }
                // PROTOCOL.md: a gateway relays the tx anyway so other gateways can try
                // (mempool dedups on tx hash, so a double submit is harmless).
                self.relay_onward(frame);
            }
            // A receipt passing through is relayed toward the origin like any flood.
            FrameKind::Receipt => self.relay_onward(frame),
        }
    }
}

/// A handle to one in-flight payment from the origin's perspective.
pub struct MeshPayment {
    tx_id: TxId,
    origin: Arc<OriginHandler>,
}

impl MeshPayment {
    /// The transaction id (receipt + dedup key) of this payment.
    pub fn tx_id(&self) -> TxId {
        self.tx_id
    }

    /// The current status without blocking.
    pub fn status(&self) -> PaymentStatus {
        self.origin.status(&self.tx_id)
    }

    /// Block (up to `timeout`) until the payment leaves `Pending`, returning its final
    /// status (`Settled`/`Failed`), or the last-known status on timeout.
    pub fn wait_settled(&self, timeout: Duration) -> PaymentStatus {
        self.origin.wait(self.tx_id, timeout)
    }
}

/// An offline origin node: signs (later) and floods payments, settles on receipt.
pub struct OriginNode {
    transport: Arc<dyn MeshTransport>,
    handler: Arc<OriginHandler>,
}

impl OriginNode {
    /// Attach an origin node to the mesh and start delivering.
    pub fn join(mesh: &crate::transport::MockMesh, id: NodeId) -> Result<Self, MeshError> {
        let transport: Arc<dyn MeshTransport> = Arc::new(mesh.attach(id));
        let handler = Arc::new(OriginHandler::new(Arc::downgrade(&transport)));
        transport.set_receiver(handler.clone());
        transport.start()?;
        Ok(OriginNode { transport, handler })
    }

    /// This node's id.
    pub fn id(&self) -> NodeId {
        self.transport.node_id()
    }

    /// Flood an opaque signed-tx blob and get a handle to track its settlement.
    pub fn pay(&self, tx_wire: Vec<u8>) -> MeshPayment {
        let tx_id = self.handler.start_payment(tx_wire);
        MeshPayment {
            tx_id,
            origin: self.handler.clone(),
        }
    }
}

/// A blind relay node.
pub struct RelayNode {
    transport: Arc<dyn MeshTransport>,
    handler: Arc<RelayHandler>,
}

impl RelayNode {
    /// Attach a relay node to the mesh and start delivering.
    pub fn join(mesh: &crate::transport::MockMesh, id: NodeId) -> Result<Self, MeshError> {
        let transport: Arc<dyn MeshTransport> = Arc::new(mesh.attach(id));
        let handler = Arc::new(RelayHandler::new(Arc::downgrade(&transport)));
        transport.set_receiver(handler.clone());
        transport.start()?;
        Ok(RelayNode { transport, handler })
    }

    /// This node's id.
    pub fn id(&self) -> NodeId {
        self.transport.node_id()
    }

    /// How many frames this relay has forwarded (test/observability hook).
    pub fn forwarded_count(&self) -> usize {
        *self.handler.forwarded.lock().unwrap()
    }
}

/// A gateway node: the one online hop that submits and emits receipts.
pub struct GatewayNode {
    transport: Arc<dyn MeshTransport>,
    gateway: Arc<dyn MeshGateway>,
}

impl GatewayNode {
    /// Attach a gateway node (backed by `gateway`) to the mesh and start delivering.
    pub fn join(
        mesh: &crate::transport::MockMesh,
        id: NodeId,
        gateway: Arc<dyn MeshGateway>,
    ) -> Result<Self, MeshError> {
        let transport: Arc<dyn MeshTransport> = Arc::new(mesh.attach(id));
        let handler = Arc::new(GatewayHandler::new(
            Arc::downgrade(&transport),
            gateway.clone(),
        ));
        transport.set_receiver(handler);
        transport.start()?;
        Ok(GatewayNode { transport, gateway })
    }

    /// This node's id.
    pub fn id(&self) -> NodeId {
        self.transport.node_id()
    }

    /// The backing gateway (e.g. to assert recorded submissions in a test).
    pub fn gateway(&self) -> Arc<dyn MeshGateway> {
        self.gateway.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default_network;
    use crate::gateway::MockGateway;
    use crate::transport::MockMesh;

    const WAIT: Duration = Duration::from_secs(3);

    #[test]
    fn mock_pay_loop_settles_through_one_relay() {
        let mesh = MockMesh::new();
        let origin = OriginNode::join(&mesh, NodeId::new(1)).unwrap();
        let relay = RelayNode::join(&mesh, NodeId::new(2)).unwrap();
        let gw = Arc::new(MockGateway::new(default_network()));
        let gateway = GatewayNode::join(&mesh, NodeId::new(3), gw.clone()).unwrap();

        // origin <-> relay <-> gateway: the origin is NOT directly linked to the
        // gateway, so the payment MUST traverse the relay.
        mesh.connect(origin.id(), relay.id());
        mesh.connect(relay.id(), gateway.id());

        // G3: an opaque stand-in for the real signed Nimiq tx wire.
        let payload = b"opaque-signed-tx-bytes-from-airplane-mode".to_vec();
        let payment = origin.pay(payload.clone());

        assert_eq!(payment.wait_settled(WAIT), PaymentStatus::Settled);
        // The gateway "broadcast" exactly the opaque bytes the origin signed, once.
        assert_eq!(gw.submission_count(), 1);
        assert_eq!(gw.submissions()[0], payload);
        // The relay actually carried traffic (tx forward + receipt forward).
        assert!(relay.forwarded_count() >= 1);
        // The receipt correlates to the origin's tx id.
        assert_eq!(payment.tx_id(), mock_tx_id(&payload));
    }

    #[test]
    fn gateway_submits_once_despite_two_relay_paths() {
        let mesh = MockMesh::new();
        let origin = OriginNode::join(&mesh, NodeId::new(1)).unwrap();
        let r1 = RelayNode::join(&mesh, NodeId::new(2)).unwrap();
        let r2 = RelayNode::join(&mesh, NodeId::new(3)).unwrap();
        let gw = Arc::new(MockGateway::new(default_network()));
        let gateway = GatewayNode::join(&mesh, NodeId::new(4), gw.clone()).unwrap();

        // Diamond: origin reaches the gateway via two independent relay paths.
        mesh.connect(origin.id(), r1.id());
        mesh.connect(origin.id(), r2.id());
        mesh.connect(r1.id(), gateway.id());
        mesh.connect(r2.id(), gateway.id());

        let payment = origin.pay(b"tx".to_vec());
        assert_eq!(payment.wait_settled(WAIT), PaymentStatus::Settled);
        // Dedup holds across multiple arrival paths: exactly one submission.
        assert_eq!(gw.submission_count(), 1);
    }

    #[test]
    fn rejected_tx_surfaces_failed_not_settled() {
        // Core value #5: never show "paid" without inclusion; a rejection is explicit.
        let mesh = MockMesh::new();
        let origin = OriginNode::join(&mesh, NodeId::new(1)).unwrap();
        let relay = RelayNode::join(&mesh, NodeId::new(2)).unwrap();
        let gw = Arc::new(MockGateway::new(default_network()));
        gw.force_status(ReceiptStatus::Expired);
        let gateway = GatewayNode::join(&mesh, NodeId::new(3), gw.clone()).unwrap();

        mesh.connect(origin.id(), relay.id());
        mesh.connect(relay.id(), gateway.id());

        let payment = origin.pay(b"stale-tx".to_vec());
        assert_eq!(payment.wait_settled(WAIT), PaymentStatus::Failed);
        assert_eq!(gw.submission_count(), 1);
    }

    #[test]
    fn unreachable_gateway_leaves_payment_pending() {
        // No gateway in the graph: the origin honestly stays Pending (never "settled").
        let mesh = MockMesh::new();
        let origin = OriginNode::join(&mesh, NodeId::new(1)).unwrap();
        let relay = RelayNode::join(&mesh, NodeId::new(2)).unwrap();
        mesh.connect(origin.id(), relay.id());

        let payment = origin.pay(b"into-the-void".to_vec());
        assert_eq!(
            payment.wait_settled(Duration::from_millis(250)),
            PaymentStatus::Pending
        );
        assert_eq!(payment.status(), PaymentStatus::Pending);
    }
}
