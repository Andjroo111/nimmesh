//! # provider — the `kind: mock | real` mesh provider seam
//!
//! Mirrors the fleet's `ChainProvider { kind: mock | real }` pattern: one place that
//! bundles a [`MeshTransport`] + a [`MeshGateway`] behind a single handle, selected by
//! a [`ProviderKind`]. The relay/UI layers depend on `MeshProvider`, never on a
//! concrete transport or gateway, so swapping mock for real is a one-line change.
//!
//! - **Mock** (G2): an in-memory [`MockMeshTransport`] + a record-only `MockGateway`.
//!   Fully wired and exercised by the end-to-end pay-loop in `payment.rs`.
//! - **Real** (G5 + G8): a BLE transport (`CoreBluetooth` / `android.bluetooth.le`) +
//!   an RPC gateway (`sendRawTransaction`). Not built yet — see the `// G5:` / `// G8:`
//!   anchors. Constructing one returns [`MeshError::NotStarted`] until those land.

use std::sync::Arc;

use crate::gateway::MeshGateway;
use crate::transport::{MeshError, MeshTransport};

/// Which backing implementation a [`MeshProvider`] wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// In-memory, radio-free mock (CI / headless tests).
    Mock,
    /// Real BLE transport + RPC gateway (G5 + G8; money-path, gated).
    Real,
}

/// A transport + gateway pair behind one `kind`-tagged handle.
///
/// This is the seam G5/G8 plug into: today only [`ProviderKind::Mock`] can be built.
pub struct MeshProvider {
    kind: ProviderKind,
    transport: Arc<dyn MeshTransport>,
    gateway: Arc<dyn MeshGateway>,
}

impl MeshProvider {
    /// Build a mock provider from an already-attached transport + a mock-capable gateway.
    pub fn mock(transport: Arc<dyn MeshTransport>, gateway: Arc<dyn MeshGateway>) -> Self {
        MeshProvider {
            kind: ProviderKind::Mock,
            transport,
            gateway,
        }
    }

    /// Attempt to build the real provider.
    ///
    /// G5: real BLE [`MeshTransport`] (concurrent central + peripheral).
    /// G8: real RPC [`MeshGateway`] (`sendRawTransaction`, money-path / gated).
    /// Neither is merged, so this is a deliberate seam stub.
    pub fn real() -> Result<Self, MeshError> {
        Err(MeshError::NotStarted)
    }

    /// Which kind this provider is.
    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// The mesh transport (shared handle).
    pub fn transport(&self) -> Arc<dyn MeshTransport> {
        self.transport.clone()
    }

    /// The gateway (shared handle).
    pub fn gateway(&self) -> Arc<dyn MeshGateway> {
        self.gateway.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default_network;
    use crate::gateway::MockGateway;
    use crate::transport::{MockMesh, NodeId};

    #[test]
    fn mock_provider_exposes_its_seam() {
        let mesh = MockMesh::new();
        let transport: Arc<dyn MeshTransport> = Arc::new(mesh.attach(NodeId::new(1)));
        let gateway: Arc<dyn MeshGateway> = Arc::new(MockGateway::new(default_network()));
        let provider = MeshProvider::mock(transport, gateway);

        assert_eq!(provider.kind(), ProviderKind::Mock);
        assert_eq!(provider.transport().node_id(), NodeId::new(1));
        // The gateway handle is live and record-only (no submissions yet).
        assert!(provider.gateway().submit(b"x".to_vec()).is_ok());
    }

    #[test]
    fn real_provider_is_not_available_yet() {
        assert_eq!(MeshProvider::real().map(|_| ()), Err(MeshError::NotStarted));
    }
}
