//! # mock_radio — a pure-Rust `BleRadio` + a virtual-topology mesh harness
//!
//! The headless test substrate ADR-0002 calls "decisive": because [`BleRadio`] is a
//! plain trait, it is implemented in **pure Rust** ([`MockRadio`]) and N
//! [`MeshNode`]s are wired into a virtual topology with controllable **latency / loss /
//! partition**. The whole GOAL.md demo loop runs deterministically under `cargo test`
//! with no phone — the same `kind: mock | real` seam RISKS.md Part A names.
//!
//! ### Topology
//!
//! - [`MockEther`] is the shared "air": a delivery thread that routes each
//!   `radio.send(peer, bytes)` to the destination node's `on_packet_received`, after an
//!   optional latency and subject to loss / partition, then reports the outcome back to
//!   the sender via `on_send_result` (modelling the async CB/GATT write result —
//!   ADR-0002 gotcha b).
//! - [`MockRadio`] is one node's radio: `send` hands bytes to the ether **fire-and-
//!   forget** and returns immediately. It holds its node **weakly** (gotcha d).
//! - [`MeshHarness`] wires nodes + edges and hands back `Arc<MeshNode>` handles.
//!
//! The ether keeps only **weak** references to nodes, so a node dropped by the test is
//! reclaimed even while the ether lives — the leak-discipline the real shim needs.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::gateway::MeshGateway;
use crate::node::MeshNode;
use crate::radio::{BleRadio, PeerId};
use crate::relay::RelayPolicy;

/// One queued transmission travelling through the ether.
struct Transmit {
    src_peer: PeerId,
    src_node: Weak<MeshNode>,
    dst_peer: PeerId,
    bytes: Vec<u8>,
}

/// Tunable channel conditions for the virtual mesh.
#[derive(Clone)]
struct EtherConfig {
    /// Per-hop delivery delay.
    latency: Duration,
    /// Per-hop drop probability in `[0.0, 1.0]` (deterministic via the ether's PRNG).
    loss: f64,
    /// Directed `(src, dst)` links that are cut (a partition). Add both directions to
    /// fully sever a link.
    partitions: HashSet<(PeerId, PeerId)>,
}

impl Default for EtherConfig {
    fn default() -> Self {
        EtherConfig {
            latency: Duration::ZERO,
            loss: 0.0,
            partitions: HashSet::new(),
        }
    }
}

struct EtherInner {
    registry: Mutex<HashMap<PeerId, Weak<MeshNode>>>,
    config: Mutex<EtherConfig>,
    tx: Mutex<Option<Sender<Transmit>>>,
    /// xorshift64 state for deterministic loss decisions.
    rng: Mutex<u64>,
}

impl EtherInner {
    fn roll_loss(&self, loss: f64) -> bool {
        if loss <= 0.0 {
            return false;
        }
        if loss >= 1.0 {
            return true;
        }
        let mut s = self.rng.lock().unwrap();
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        (x as f64 / u64::MAX as f64) < loss
    }
}

/// The shared virtual "air" connecting every [`MockRadio`].
pub struct MockEther {
    inner: Arc<EtherInner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl MockEther {
    /// A fresh ether with zero latency, zero loss, no partitions, and its delivery
    /// thread running.
    pub fn new() -> Arc<Self> {
        let (tx, rx) = channel();
        let inner = Arc::new(EtherInner {
            registry: Mutex::new(HashMap::new()),
            config: Mutex::new(EtherConfig::default()),
            tx: Mutex::new(Some(tx)),
            rng: Mutex::new(0x9E37_79B9_7F4A_7C15),
        });
        let worker_inner = inner.clone();
        let worker = std::thread::spawn(move || Self::run(worker_inner, rx));
        Arc::new(MockEther {
            inner,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Register a node under its peer id so the ether can deliver to it (held weakly).
    fn register(&self, peer: PeerId, node: Weak<MeshNode>) {
        self.inner.registry.lock().unwrap().insert(peer, node);
    }

    /// Hand a transmission to the ether (called by [`MockRadio::send`], non-blocking).
    fn enqueue(&self, t: Transmit) {
        if let Some(tx) = self.inner.tx.lock().unwrap().as_ref() {
            let _ = tx.send(t);
        }
    }

    /// Set the per-hop delivery latency.
    pub fn set_latency(&self, latency: Duration) {
        self.inner.config.lock().unwrap().latency = latency;
    }

    /// Set the per-hop drop probability in `[0.0, 1.0]`.
    pub fn set_loss(&self, loss: f64) {
        self.inner.config.lock().unwrap().loss = loss.clamp(0.0, 1.0);
    }

    /// Cut the link between `a` and `b` in both directions (partition the mesh).
    pub fn partition(&self, a: &str, b: &str) {
        let mut cfg = self.inner.config.lock().unwrap();
        cfg.partitions.insert((a.to_string(), b.to_string()));
        cfg.partitions.insert((b.to_string(), a.to_string()));
    }

    /// Heal a previously-partitioned link between `a` and `b`.
    pub fn heal(&self, a: &str, b: &str) {
        let mut cfg = self.inner.config.lock().unwrap();
        cfg.partitions.remove(&(a.to_string(), b.to_string()));
        cfg.partitions.remove(&(b.to_string(), a.to_string()));
    }

    /// Stop the delivery thread (called by [`MeshHarness::shutdown`] / on drop).
    pub fn shutdown(&self) {
        // Dropping the sender closes the channel, so the delivery loop's `recv` ends.
        let _ = self.inner.tx.lock().unwrap().take();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }

    fn run(inner: Arc<EtherInner>, rx: Receiver<Transmit>) {
        while let Ok(t) = rx.recv() {
            let (latency, loss, blocked) = {
                let cfg = inner.config.lock().unwrap();
                let blocked = cfg
                    .partitions
                    .contains(&(t.src_peer.clone(), t.dst_peer.clone()));
                (cfg.latency, cfg.loss, blocked)
            };
            if latency > Duration::ZERO {
                std::thread::sleep(latency);
            }
            let delivered = !blocked && !inner.roll_loss(loss);
            if delivered {
                let dst = inner
                    .registry
                    .lock()
                    .unwrap()
                    .get(&t.dst_peer)
                    .and_then(Weak::upgrade);
                if let Some(node) = dst {
                    // Deliver with the source peer attributed (the destination's view of
                    // the sender) so the worker can apply G6 source-link exclusion.
                    node.on_packet_received_from(t.src_peer.clone(), t.bytes);
                }
            }
            // Report the async write outcome back to the sender (ADR-0002 gotcha b).
            if let Some(src) = t.src_node.upgrade() {
                src.on_send_result(t.dst_peer.clone(), delivered);
            }
        }
    }
}

impl Drop for MockEther {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One node's pure-Rust radio. Implements [`BleRadio`]; routes through a [`MockEther`].
pub struct MockRadio {
    peer_id: PeerId,
    ether: Arc<MockEther>,
    /// The node this radio belongs to — held **weakly** (ADR-0002 gotcha d). Set after
    /// the node is built via [`MockRadio::bind`].
    node: Mutex<Weak<MeshNode>>,
    advertising: AtomicBool,
    scanning: AtomicBool,
    stopped: AtomicBool,
}

impl MockRadio {
    /// A radio for `peer_id` attached to `ether` (not yet bound to a node).
    pub fn new(peer_id: &str, ether: Arc<MockEther>) -> Arc<Self> {
        Arc::new(MockRadio {
            peer_id: peer_id.to_string(),
            ether,
            node: Mutex::new(Weak::new()),
            advertising: AtomicBool::new(false),
            scanning: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        })
    }

    /// Bind the (weak) node handle so the radio can report outcomes back to it.
    pub fn bind(&self, node: Weak<MeshNode>) {
        *self.node.lock().unwrap() = node;
    }

    /// Whether [`BleRadio::stop`] has been called (test hook for the teardown assertion).
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Whether the radio is advertising + scanning (i.e. brought up by the node).
    pub fn is_up(&self) -> bool {
        self.advertising.load(Ordering::SeqCst) && self.scanning.load(Ordering::SeqCst)
    }
}

impl BleRadio for MockRadio {
    fn start_advertising(&self) {
        self.advertising.store(true, Ordering::SeqCst);
    }

    fn start_scanning(&self) {
        self.scanning.store(true, Ordering::SeqCst);
    }

    fn send(&self, peer_id: String, bytes: Vec<u8>) {
        // Fire-and-forget (ADR-0002 gotcha b): hand to the ether and return at once.
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let src_node = self.node.lock().unwrap().clone();
        self.ether.enqueue(Transmit {
            src_peer: self.peer_id.clone(),
            src_node,
            dst_peer: peer_id,
            bytes,
        });
    }

    fn disconnect(&self, _peer_id: String) {
        // Mock connections are modelled by the topology; nothing to tear down here.
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.advertising.store(false, Ordering::SeqCst);
        self.scanning.store(false, Ordering::SeqCst);
    }
}

/// A builder/owner for a virtual mesh of [`MeshNode`]s over one [`MockEther`].
///
/// The public façade tests drive: add plain or gateway nodes, wire edges, fetch handles,
/// and tune the ether. Owns the node `Arc`s so they live as long as the harness.
pub struct MeshHarness {
    ether: Arc<MockEther>,
    nodes: HashMap<PeerId, Arc<MeshNode>>,
    radios: HashMap<PeerId, Arc<MockRadio>>,
}

impl Default for MeshHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl MeshHarness {
    /// A new, empty harness with a running ether.
    pub fn new() -> Self {
        MeshHarness {
            ether: MockEther::new(),
            nodes: HashMap::new(),
            radios: HashMap::new(),
        }
    }

    /// The shared ether (to set latency / loss / partitions).
    pub fn ether(&self) -> Arc<MockEther> {
        self.ether.clone()
    }

    fn attach(
        &mut self,
        peer_id: &str,
        node: Arc<MeshNode>,
        radio: Arc<MockRadio>,
    ) -> Arc<MeshNode> {
        radio.bind(Arc::downgrade(&node));
        self.ether
            .register(peer_id.to_string(), Arc::downgrade(&node));
        self.nodes.insert(peer_id.to_string(), node.clone());
        self.radios.insert(peer_id.to_string(), radio);
        node
    }

    /// Add a plain relay/origin node at `peer_id` with protocol `sender_id`.
    ///
    /// Uses the **deterministic** relay policy (zero-sleep jitter + fixed seed) so the
    /// headless harness stays fast and reproducible — no real G6 jitter sleeps under test.
    pub fn add_node(&mut self, peer_id: &str, sender_id: &[u8]) -> Arc<MeshNode> {
        let radio = MockRadio::new(peer_id, self.ether.clone());
        let node = MeshNode::new_with_policy(
            sender_id.to_vec(),
            radio.clone(),
            RelayPolicy::deterministic(),
        );
        self.attach(peer_id, node, radio)
    }

    /// Add a node that also acts as a gateway (records submissions + emits receipts).
    pub fn add_gateway(
        &mut self,
        peer_id: &str,
        sender_id: &[u8],
        gateway: Arc<dyn MeshGateway>,
    ) -> Arc<MeshNode> {
        let radio = MockRadio::new(peer_id, self.ether.clone());
        let node = MeshNode::new_gateway_with_policy(
            sender_id.to_vec(),
            radio.clone(),
            gateway,
            RelayPolicy::deterministic(),
        );
        self.attach(peer_id, node, radio)
    }

    /// Wire a bidirectional BLE link between `a` and `b` (each learns the other as a
    /// connected peer).
    pub fn connect(&self, a: &str, b: &str) {
        if let (Some(na), Some(nb)) = (self.nodes.get(a), self.nodes.get(b)) {
            na.on_peer_connected(b.to_string());
            nb.on_peer_connected(a.to_string());
        }
    }

    /// A node handle by peer id.
    pub fn node(&self, peer_id: &str) -> Arc<MeshNode> {
        self.nodes.get(peer_id).expect("unknown node").clone()
    }

    /// The mock radio for a node by peer id (test hook).
    pub fn radio(&self, peer_id: &str) -> Arc<MockRadio> {
        self.radios.get(peer_id).expect("unknown radio").clone()
    }

    /// Tear down every node and stop the ether.
    pub fn shutdown(self) {
        for node in self.nodes.values() {
            node.shutdown();
        }
        self.ether.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn radio_send_is_fire_and_forget_and_respects_stop() {
        let ether = MockEther::new();
        let radio = MockRadio::new("p", ether.clone());
        assert!(!radio.is_stopped());
        // No node bound, no destination registered: send must not block or panic.
        let start = Instant::now();
        radio.send("q".into(), vec![1, 2, 3]);
        assert!(start.elapsed() < Duration::from_millis(50));
        radio.stop();
        assert!(radio.is_stopped());
        radio.send("q".into(), vec![4]); // stopped: silently dropped.
        ether.shutdown();
    }

    #[test]
    fn harness_brings_radios_up_and_wires_peers() {
        let mut h = MeshHarness::new();
        h.add_node("a", &[1]);
        h.add_node("b", &[2]);
        h.connect("a", "b");
        assert!(h.radio("a").is_up());
        assert_eq!(h.node("a").connected_peers(), 1);
        assert_eq!(h.node("b").connected_peers(), 1);
        h.shutdown();
    }
}
