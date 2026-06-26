//! # transport — the `MeshTransport` seam + an in-memory `MockMeshTransport`
//!
//! G2 stands up the **frozen transport seam**: the trait the relay/gateway layers
//! talk to, plus a radio-free, channel-based mock that wires several virtual nodes
//! into a relay graph so the whole offline pay-loop is testable headless in CI.
//!
//! - The seam carries **opaque `Vec<u8>` payloads** end to end. The transport never
//!   inspects what it ships (core value #3, trustless relay).
//! - The real BLE transport (`CoreBluetooth` / `android.bluetooth.le`) plugs in behind
//!   this same trait at **G5** — see the `// G5:` anchor in `provider.rs`.
//! - [`MeshFrame`] is a **temporary mock framing** so relays can read TTL + a dedup
//!   key without ever touching the opaque payload. The real bitmesh packet codec
//!   (`nimiqTx` 0x30 / `nimiqTxReceipt` 0x31, 256-byte padding, 14-byte header)
//!   replaces it at **G4** — see the `// G4:` anchors below.
//!
//! No Bluetooth, no `tokio`, no external deps — just `std` channels + threads, so the
//! mock behaves like an async radio (deliver-via-callback) while staying deterministic
//! enough for the end-to-end test in `payment.rs`.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Default + cap TTL for a mesh packet (`PROTOCOL.md`: "Default and cap **TTL = 7**").
pub const DEFAULT_TTL: u8 = 7;

/// Errors the transport seam can surface. Deliberately tiny and `Clone`/`Eq` so tests
/// and the FFI layer (later) can match on them without a heavyweight error crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeshError {
    /// `broadcast`/`stop` called on a transport that was never `start`ed.
    NotStarted,
    /// `start` called twice on the same transport.
    AlreadyStarted,
    /// `start` called before a receive handler was registered with [`MeshTransport::set_receiver`].
    NoReceiver,
}

impl fmt::Display for MeshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeshError::NotStarted => write!(f, "mesh transport is not started"),
            MeshError::AlreadyStarted => write!(f, "mesh transport is already started"),
            MeshError::NoReceiver => write!(f, "mesh transport has no receive handler"),
        }
    }
}

impl std::error::Error for MeshError {}

/// A node's stable id on the mock mesh.
///
/// Maps to the protocol's 8-byte `senderID`; kept as a `u64` here because the mock
/// only needs identity + graph wiring, not the on-wire encoding (that lands at G4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

impl NodeId {
    /// Construct a node id from a raw value.
    pub const fn new(v: u64) -> Self {
        NodeId(v)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node#{}", self.0)
    }
}

/// A transaction identity used as the receipt + dedup key.
///
/// In the real protocol this is the 32-byte Blake2b hash of the signed tx wire. The
/// mock derives it deterministically from the opaque payload via [`mock_tx_id`] so the
/// harness can key receipts and dedup without pulling a hash dependency.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxId(pub [u8; 32]);

impl fmt::Debug for TxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TxId(")?;
        for b in &self.0[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "..)")
    }
}

/// Derive a deterministic 32-byte id from an opaque payload (mock only).
///
/// G4: the real `txId` is the 32-byte Blake2b hash of the canonical signed Nimiq tx
/// wire. This FNV-1a expansion is a stand-in so dedup + receipt keying work in the
/// harness; it is never used on the money path.
pub fn mock_tx_id(payload: &[u8]) -> TxId {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut h = FNV_OFFSET;
    for b in payload {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h ^= payload.len() as u64;
    h = h.wrapping_mul(FNV_PRIME);

    let mut out = [0u8; 32];
    for (chunk, slot) in out.chunks_mut(8).enumerate() {
        h = h.wrapping_mul(FNV_PRIME) ^ (chunk as u64 + 1);
        slot.copy_from_slice(&h.to_be_bytes());
    }
    TxId(out)
}

/// The kind of frame riding the mock mesh.
///
/// The discriminant bytes mirror the real `PROTOCOL.md` message types so the codec at
/// G4 can drop straight in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A signed Nimiq tx flooding toward a gateway (`nimiqTx`, 0x30).
    Tx,
    /// A gateway's accepted/expired/failed ack flowing back (`nimiqTxReceipt`, 0x31).
    Receipt,
}

impl FrameKind {
    /// The on-wire type byte (mirrors `PROTOCOL.md`).
    pub const fn type_byte(self) -> u8 {
        match self {
            FrameKind::Tx => 0x30,
            FrameKind::Receipt => 0x31,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x30 => Some(FrameKind::Tx),
            0x31 => Some(FrameKind::Receipt),
            _ => None,
        }
    }
}

/// Mock framing version byte. Distinct from the real header `version = 1` so a stray
/// real packet can never be misread as a mock frame.
const MOCK_FRAME_VERSION: u8 = 0xA1;
/// Fixed prefix length: version(1)+kind(1)+ttl(1)+status(1)+txId(32)+payloadLen(2).
const MOCK_FRAME_HEADER: usize = 38;

/// A minimal mock packet: a routing header wrapped around an **opaque** payload.
///
/// G4: the real bitmesh packet codec (14-byte big-endian header, flags, 256-byte
/// PKCS#7 padding, optional 64-byte Ed25519 signature) replaces this whole type.
/// Until then the relay/gateway need only `ttl` + a dedup key, and they **never read
/// `payload`** — that is the opaque signed-tx blob G3 will fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshFrame {
    /// Tx (flood toward a gateway) or Receipt (ack back to the origin).
    pub kind: FrameKind,
    /// Remaining hop budget; relays decrement, drop at `<= 1` (`PROTOCOL.md`).
    pub ttl: u8,
    /// Receipt status code (see `gateway::ReceiptStatus::code`); 0 for a tx.
    pub status: u8,
    /// Dedup + receipt-correlation key.
    pub tx_id: TxId,
    /// The opaque application payload — never inspected by a relay.
    pub payload: Vec<u8>,
}

impl MeshFrame {
    /// Serialize to opaque bytes for the transport to ship.
    pub fn encode(&self) -> Vec<u8> {
        let len = self.payload.len().min(u16::MAX as usize);
        let mut v = Vec::with_capacity(MOCK_FRAME_HEADER + len);
        v.push(MOCK_FRAME_VERSION);
        v.push(self.kind.type_byte());
        v.push(self.ttl);
        v.push(self.status);
        v.extend_from_slice(&self.tx_id.0);
        v.extend_from_slice(&(len as u16).to_be_bytes());
        v.extend_from_slice(&self.payload[..len]);
        v
    }

    /// Parse from opaque bytes; returns `None` on any malformed frame (dropped silently).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < MOCK_FRAME_HEADER || bytes[0] != MOCK_FRAME_VERSION {
            return None;
        }
        let kind = FrameKind::from_byte(bytes[1])?;
        let ttl = bytes[2];
        let status = bytes[3];
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[4..36]);
        let len = u16::from_be_bytes([bytes[36], bytes[37]]) as usize;
        let end = MOCK_FRAME_HEADER + len;
        if bytes.len() < end {
            return None;
        }
        Some(MeshFrame {
            kind,
            ttl,
            status,
            tx_id: TxId(id),
            payload: bytes[MOCK_FRAME_HEADER..end].to_vec(),
        })
    }

    /// The LRU dedup key — `(type, txId)`, mirroring the protocol's dedup intent.
    pub fn dedup_key(&self) -> (u8, TxId) {
        (self.kind.type_byte(), self.tx_id)
    }
}

/// A sink for packets a node receives off the mesh.
///
/// The relay/gateway/origin logic in `payment.rs` implement this. `from` is the
/// neighbor that delivered the bytes; `packet` is opaque.
pub trait PacketHandler: Send + Sync {
    /// Called once per received packet, on the transport's delivery thread.
    fn on_packet(&self, from: NodeId, packet: Vec<u8>);
}

/// The frozen transport seam: start/stop, broadcast opaque bytes, deliver-via-callback.
///
/// G5 implements this over real BLE; [`MockMeshTransport`] implements it over an
/// in-memory graph. Everything above the radio (relay, gateway, store-and-forward)
/// depends only on this trait.
pub trait MeshTransport: Send + Sync {
    /// This transport's node id.
    fn node_id(&self) -> NodeId;
    /// Register the handler invoked for each received packet. Call before `start`.
    fn set_receiver(&self, handler: Arc<dyn PacketHandler>);
    /// Begin delivering received packets to the handler.
    fn start(&self) -> Result<(), MeshError>;
    /// Stop delivery and release the worker thread. Idempotent.
    fn stop(&self) -> Result<(), MeshError>;
    /// Flood `packet` to every mesh neighbor of this node.
    fn broadcast(&self, packet: Vec<u8>) -> Result<(), MeshError>;
}

/// Internal inbox message: a delivered packet or a shutdown sentinel.
enum Inbound {
    Packet { from: NodeId, bytes: Vec<u8> },
    Shutdown,
}

#[derive(Default)]
struct MeshInner {
    inboxes: HashMap<NodeId, Sender<Inbound>>,
    edges: HashMap<NodeId, Vec<NodeId>>,
}

/// An in-memory "ether": a graph of virtual nodes and the radio edges between them.
///
/// Clone-shareable (it is just an `Arc`), so every [`MockMeshTransport`] attached to
/// it sees the same topology. A `broadcast` from a node is delivered to exactly its
/// graph neighbors — modelling one hop of BLE airtime with no radio.
#[derive(Clone, Default)]
pub struct MockMesh {
    inner: Arc<Mutex<MeshInner>>,
}

impl MockMesh {
    /// A fresh, empty mesh.
    pub fn new() -> Self {
        MockMesh::default()
    }

    /// Add a bidirectional radio link between two nodes (idempotent).
    pub fn connect(&self, a: NodeId, b: NodeId) {
        let mut inner = self.inner.lock().unwrap();
        let ea = inner.edges.entry(a).or_default();
        if !ea.contains(&b) {
            ea.push(b);
        }
        let eb = inner.edges.entry(b).or_default();
        if !eb.contains(&a) {
            eb.push(a);
        }
    }

    /// Attach a new node and hand back its transport. The transport owns its inbox.
    pub fn attach(&self, id: NodeId) -> MockMeshTransport {
        let (tx, rx) = channel();
        self.inner.lock().unwrap().inboxes.insert(id, tx);
        MockMeshTransport {
            node_id: id,
            mesh: self.clone(),
            inbox: Mutex::new(Some(rx)),
            handler: Mutex::new(None),
            running: AtomicBool::new(false),
            worker: Mutex::new(None),
        }
    }

    fn deliver(&self, from: NodeId, bytes: Vec<u8>) {
        let inner = self.inner.lock().unwrap();
        let Some(neighbors) = inner.edges.get(&from) else {
            return;
        };
        for n in neighbors {
            if let Some(tx) = inner.inboxes.get(n) {
                let _ = tx.send(Inbound::Packet {
                    from,
                    bytes: bytes.clone(),
                });
            }
        }
    }

    fn signal_shutdown(&self, id: NodeId) {
        if let Some(tx) = self.inner.lock().unwrap().inboxes.get(&id) {
            let _ = tx.send(Inbound::Shutdown);
        }
    }

    fn detach(&self, id: NodeId) {
        let mut inner = self.inner.lock().unwrap();
        inner.inboxes.remove(&id);
        inner.edges.remove(&id);
        for neighbors in inner.edges.values_mut() {
            neighbors.retain(|n| *n != id);
        }
    }
}

/// A radio-free [`MeshTransport`] backed by a [`MockMesh`] graph.
///
/// On `start` it spawns a worker thread that drains its inbox and hands each packet to
/// the registered [`PacketHandler`]. `broadcast` floods to graph neighbors. Cleans up
/// its thread on `stop` or on drop, so leaving nodes never leak threads.
pub struct MockMeshTransport {
    node_id: NodeId,
    mesh: MockMesh,
    inbox: Mutex<Option<Receiver<Inbound>>>,
    handler: Mutex<Option<Arc<dyn PacketHandler>>>,
    running: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl MeshTransport for MockMeshTransport {
    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn set_receiver(&self, handler: Arc<dyn PacketHandler>) {
        *self.handler.lock().unwrap() = Some(handler);
    }

    fn start(&self) -> Result<(), MeshError> {
        let handler = self
            .handler
            .lock()
            .unwrap()
            .clone()
            .ok_or(MeshError::NoReceiver)?;
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(MeshError::AlreadyStarted);
        }
        let rx = self
            .inbox
            .lock()
            .unwrap()
            .take()
            .ok_or(MeshError::NotStarted)?;
        let worker = std::thread::spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    Inbound::Packet { from, bytes } => handler.on_packet(from, bytes),
                    Inbound::Shutdown => break,
                }
            }
        });
        *self.worker.lock().unwrap() = Some(worker);
        Ok(())
    }

    fn stop(&self) -> Result<(), MeshError> {
        if !self.running.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        self.mesh.signal_shutdown(self.node_id);
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
        self.mesh.detach(self.node_id);
        Ok(())
    }

    fn broadcast(&self, packet: Vec<u8>) -> Result<(), MeshError> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(MeshError::NotStarted);
        }
        self.mesh.deliver(self.node_id, packet);
        Ok(())
    }
}

impl Drop for MockMeshTransport {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel as std_channel;

    #[test]
    fn mock_tx_id_is_deterministic_and_payload_sensitive() {
        assert_eq!(mock_tx_id(b"abc"), mock_tx_id(b"abc"));
        assert_ne!(mock_tx_id(b"abc"), mock_tx_id(b"abd"));
        // Even the empty payload yields a non-zero id (FNV is seeded).
        assert_ne!(mock_tx_id(b""), TxId([0u8; 32]));
    }

    #[test]
    fn frame_roundtrips_through_opaque_bytes() {
        let frame = MeshFrame {
            kind: FrameKind::Tx,
            ttl: 7,
            status: 0,
            tx_id: mock_tx_id(b"signed-tx"),
            payload: (0..=255u8).collect(),
        };
        let decoded = MeshFrame::decode(&frame.encode()).expect("decodes");
        assert_eq!(decoded, frame);
        assert_eq!(decoded.dedup_key(), frame.dedup_key());
    }

    #[test]
    fn malformed_frames_decode_to_none() {
        assert!(MeshFrame::decode(&[]).is_none());
        assert!(MeshFrame::decode(&[0x00; 40]).is_none()); // wrong version
        let mut buf = MeshFrame {
            kind: FrameKind::Receipt,
            ttl: 3,
            status: 1,
            tx_id: mock_tx_id(b"x"),
            payload: vec![1, 2, 3],
        }
        .encode();
        buf.truncate(buf.len() - 1); // payload shorter than declared length
        assert!(MeshFrame::decode(&buf).is_none());
    }

    struct CapturingHandler {
        tx: std::sync::Mutex<Sender<(NodeId, Vec<u8>)>>,
    }
    impl PacketHandler for CapturingHandler {
        fn on_packet(&self, from: NodeId, packet: Vec<u8>) {
            let _ = self.tx.lock().unwrap().send((from, packet));
        }
    }

    #[test]
    fn broadcast_reaches_only_graph_neighbors() {
        let mesh = MockMesh::new();
        let a: Arc<dyn MeshTransport> = Arc::new(mesh.attach(NodeId::new(1)));
        let b: Arc<dyn MeshTransport> = Arc::new(mesh.attach(NodeId::new(2)));
        let c: Arc<dyn MeshTransport> = Arc::new(mesh.attach(NodeId::new(3)));

        let (btx, brx) = std_channel();
        let (ctx, crx) = std_channel();
        b.set_receiver(Arc::new(CapturingHandler {
            tx: std::sync::Mutex::new(btx),
        }));
        c.set_receiver(Arc::new(CapturingHandler {
            tx: std::sync::Mutex::new(ctx),
        }));
        b.start().unwrap();
        c.start().unwrap();

        // A is linked to B only; C must hear nothing.
        mesh.connect(a.node_id(), b.node_id());
        a.set_receiver(Arc::new(CapturingHandler {
            tx: std::sync::Mutex::new(std_channel().0),
        }));
        a.start().unwrap();
        a.broadcast(b"hello".to_vec()).unwrap();

        let (from, bytes) = brx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("B receives");
        assert_eq!(from, NodeId::new(1));
        assert_eq!(bytes, b"hello");
        assert!(crx
            .recv_timeout(std::time::Duration::from_millis(150))
            .is_err());
    }

    #[test]
    fn start_requires_a_receiver_and_is_single_shot() {
        let mesh = MockMesh::new();
        let t: Arc<dyn MeshTransport> = Arc::new(mesh.attach(NodeId::new(9)));
        assert_eq!(t.start(), Err(MeshError::NoReceiver));
        t.set_receiver(Arc::new(CapturingHandler {
            tx: std::sync::Mutex::new(std_channel().0),
        }));
        assert!(t.start().is_ok());
        assert_eq!(t.start(), Err(MeshError::AlreadyStarted));
    }
}
