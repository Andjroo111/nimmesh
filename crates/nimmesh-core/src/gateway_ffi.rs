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
            ))
        }
        #[cfg(not(feature = "gateway-rpc"))]
        {
            let _ = (sender_id, radio, rpc_url);
            Err(GatewayInitError::Unsupported)
        }
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
}
