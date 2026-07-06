//! # gateway_ffi — the FFI gateway constructor (`MeshNode::new_gateway`, G8)
//!
//! The one place a NATIVE shim can turn a relay into the mesh's internet exit: a second
//! `#[uniffi::export]` block on [`MeshNode`] exposing `new_gateway`, which wires the real
//! HTTP broadcast client ([`crate::rpc::HttpGatewayRpc`], feature `gateway-rpc`) into a
//! [`crate::gateway::RpcGateway`] and builds a gateway node. Split out of `node.rs` for
//! the 800-line guard — same pattern as the `beacon.rs`/`settlement.rs` extractions.
//!
//! **Testnet-only by construction (money-path safety):** the injected HTTP client refuses
//! known mainnet hosts and any network but testnet (`rpc::guard_testnet`, RISKS.md core
//! value #7), and the `RpcGateway` enforces the testnet `networkId` byte on every tx.
//! Lifting that is a deliberate, Andjroo-gated change — not a parameter.

use std::sync::Arc;

use crate::node::MeshNode;
use crate::radio::BleRadio;

/// A failure constructing a gateway node across FFI ([`MeshNode::new_gateway`]).
///
/// The variants keep the FFI surface **identical whether or not the `gateway-rpc`
/// feature is compiled in** — a build without the HTTP client still exports the
/// constructor and reports [`GatewayInitError::Unsupported`] at runtime, so the shared
/// generated bindings never diverge between the iOS app and the Mac node builds.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum GatewayInitError {
    /// This build of the core has no HTTP broadcast client (`gateway-rpc` feature off).
    Unsupported,
    /// The RPC client refused construction — malformed URL or a known **mainnet** host
    /// (`rpc::guard_testnet`; the gateway is testnet-only by construction).
    Rpc {
        /// A human-readable reason.
        reason: String,
    },
}

impl std::fmt::Display for GatewayInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayInitError::Unsupported => {
                write!(
                    f,
                    "gateway unsupported: core built without the gateway-rpc feature"
                )
            }
            GatewayInitError::Rpc { reason } => write!(f, "gateway rpc refused: {reason}"),
        }
    }
}

impl std::error::Error for GatewayInitError {}

#[uniffi::export]
impl MeshNode {
    /// Create a **gateway** node: a full mesh node that ALSO broadcasts every verified
    /// `nimiqTx` it hears to the Nimiq chain over JSON-RPC (G8) and floods the receipt
    /// back so the sender sees settlement. This is what turns a relay into the mesh's
    /// internet exit — the Mac node runs this.
    ///
    /// Requires the core built with the `gateway-rpc` cargo feature (the Mac node build
    /// passes `--features gateway-rpc`); otherwise returns
    /// [`GatewayInitError::Unsupported`].
    #[uniffi::constructor]
    pub fn new_gateway(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        rpc_url: String,
    ) -> Result<Arc<Self>, GatewayInitError> {
        #[cfg(feature = "gateway-rpc")]
        {
            let rpc = crate::rpc::HttpGatewayRpc::new(rpc_url, crate::NetworkId::Testnet).map_err(
                |e| GatewayInitError::Rpc {
                    reason: e.to_string(),
                },
            )?;
            let gateway: Arc<dyn crate::gateway::MeshGateway> =
                Arc::new(crate::gateway::RpcGateway::new(Arc::new(rpc)));
            // Verify-before-relay ON, same as the production `new` (G12 spam filter) —
            // a gateway must never relay OR broadcast junk bytes.
            Ok(Self::build(
                sender_id,
                radio,
                Some(gateway),
                crate::relay::RelayPolicy::production(),
                true,
                None,
                None,
                crate::NetworkId::Testnet,
            ))
        }
        #[cfg(not(feature = "gateway-rpc"))]
        {
            let _ = (sender_id, radio, rpc_url);
            Err(GatewayInitError::Unsupported)
        }
    }

    /// **OWNER-GATED (real money): create a MAINNET gateway node.** Same shape as
    /// [`MeshNode::new_gateway`] but on Albatross MAINNET (`networkId = 24`): it beacons
    /// the mainnet head, accepts ONLY mainnet-signed txs, and broadcasts them to a
    /// mainnet RPC. Authorized by Andjroo on 2026-07-06 for the real-funds mesh payment —
    /// the SENDER signs on their own device and taps Send; this node only delivers the
    /// already-signed, self-contained tx. Nothing autonomous constructs one: the Mac
    /// node requires an explicit `--mainnet` launch flag.
    ///
    /// A testnet-looking `rpc_url` is refused (the mirror of the testnet guard), so a
    /// mainnet gateway can never anchor beacons to or broadcast into the wrong chain.
    #[uniffi::constructor]
    pub fn new_gateway_mainnet(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        rpc_url: String,
    ) -> Result<Arc<Self>, GatewayInitError> {
        #[cfg(feature = "gateway-rpc")]
        {
            let rpc = crate::rpc::HttpGatewayRpc::new_mainnet(rpc_url).map_err(|e| {
                GatewayInitError::Rpc {
                    reason: e.to_string(),
                }
            })?;
            let gateway: Arc<dyn crate::gateway::MeshGateway> =
                Arc::new(crate::gateway::RpcGateway::new_mainnet(Arc::new(rpc)));
            Ok(Self::build(
                sender_id,
                radio,
                Some(gateway),
                crate::relay::RelayPolicy::production(),
                true,
                None,
                None,
                crate::NetworkId::Mainnet,
            ))
        }
        #[cfg(not(feature = "gateway-rpc"))]
        {
            let _ = (sender_id, radio, rpc_url);
            Err(GatewayInitError::Unsupported)
        }
    }

    /// Create a plain (non-gateway) node pinned to `network`. The mainnet phone app uses
    /// this so its head cache accepts the mainnet gateway's beacons, its envelopes carry
    /// the mainnet `networkId`, and [`MeshNode::anchored_intent`] yields mainnet intents.
    /// Constructing a node signs/broadcasts nothing — the network choice only matters
    /// once the OWNER of the device signs a tx into it.
    #[uniffi::constructor]
    pub fn new_on_network(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        network: crate::NetworkId,
    ) -> Arc<Self> {
        // Verify-before-relay ON, same as the production `new` (G12 spam filter).
        Self::build(
            sender_id,
            radio,
            None,
            crate::relay::RelayPolicy::production(),
            true,
            None,
            None,
            network,
        )
    }
}

#[cfg(test)]
mod gateway_ctor_tests {
    use super::*;
    use crate::mock_radio::{MockEther, MockRadio};

    fn radio() -> Arc<dyn BleRadio> {
        MockRadio::new("gw", MockEther::new())
    }

    /// Without `gateway-rpc` the constructor is still exported but honestly refuses,
    /// so the shared generated bindings never diverge between builds.
    #[cfg(not(feature = "gateway-rpc"))]
    #[test]
    fn new_gateway_unsupported_without_feature() {
        match MeshNode::new_gateway(
            b"gw".to_vec(),
            radio(),
            "https://rpc.testnet.nimiqwatch.com".into(),
        ) {
            Err(err) => assert_eq!(err, GatewayInitError::Unsupported),
            Ok(_) => panic!("must refuse without the gateway-rpc feature"),
        }
    }

    /// The money-path guard holds across FFI: a known mainnet host is refused outright.
    #[cfg(feature = "gateway-rpc")]
    #[test]
    fn new_gateway_refuses_mainnet_host() {
        match MeshNode::new_gateway(b"gw".to_vec(), radio(), "https://rpc.nimiqwatch.com".into()) {
            Err(err) => assert!(matches!(err, GatewayInitError::Rpc { .. })),
            Ok(_) => panic!("mainnet host must be refused"),
        }
    }

    /// A testnet URL constructs a live gateway node (no network I/O at construction).
    #[cfg(feature = "gateway-rpc")]
    #[test]
    fn new_gateway_builds_on_testnet_url() {
        let node = MeshNode::new_gateway(
            b"gw".to_vec(),
            radio(),
            "https://rpc.testnet.nimiqwatch.com".into(),
        )
        .expect("testnet gateway constructs");
        node.shutdown();
    }

    /// The mainnet ctor is the mirror image: it refuses a TESTNET-looking URL, so a
    /// mainnet gateway can never beacon/broadcast against the wrong chain.
    #[cfg(feature = "gateway-rpc")]
    #[test]
    fn new_gateway_mainnet_refuses_testnet_url() {
        match MeshNode::new_gateway_mainnet(
            b"gw".to_vec(),
            radio(),
            "https://rpc.testnet.nimiqwatch.com".into(),
        ) {
            Err(err) => assert!(matches!(err, GatewayInitError::Rpc { .. })),
            Ok(_) => panic!("testnet url must be refused by the mainnet ctor"),
        }
    }

    /// A mainnet URL constructs a live MAINNET gateway node (no network I/O at
    /// construction), and its intents anchor on the mainnet network.
    #[cfg(feature = "gateway-rpc")]
    #[test]
    fn new_gateway_mainnet_builds_on_mainnet_url() {
        let node = MeshNode::new_gateway_mainnet(
            b"gw".to_vec(),
            radio(),
            "https://rpc.nimiqwatch.com".into(),
        )
        .expect("mainnet gateway constructs");
        node.shutdown();
    }

    /// Without `gateway-rpc` the mainnet ctor refuses too — identical FFI surface.
    #[cfg(not(feature = "gateway-rpc"))]
    #[test]
    fn new_gateway_mainnet_unsupported_without_feature() {
        match MeshNode::new_gateway_mainnet(
            b"gw".to_vec(),
            radio(),
            "https://rpc.nimiqwatch.com".into(),
        ) {
            Err(err) => assert_eq!(err, GatewayInitError::Unsupported),
            Ok(_) => panic!("must refuse without the gateway-rpc feature"),
        }
    }

    /// A plain node pinned to mainnet constructs in every build (no gateway involved).
    #[test]
    fn new_on_network_builds_a_mainnet_plain_node() {
        let node = MeshNode::new_on_network(b"ph".to_vec(), radio(), crate::NetworkId::Mainnet);
        // No beacon heard yet — a mainnet node refuses to anchor, same as testnet.
        assert!(node
            .anchored_intent("NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".into(), 1)
            .is_none());
        node.shutdown();
    }
}
