//! # provider — the `kind: mock | real` mesh provider seam
//!
//! Mirrors the fleet's `ChainProvider { kind: mock | real }` pattern (RISKS.md Part A):
//! one place that bundles the two things a node needs — a **radio** ([`BleRadio`], the
//! ADR-0002 byte-stream seam) and a **gateway** ([`MeshGateway`], the one online hop) —
//! behind a single `kind`-tagged handle. The node/UI layers depend on `MeshProvider`,
//! never on a concrete radio or gateway, so swapping mock for real is a one-line change.
//!
//! - **Mock** (G5): a pure-Rust [`crate::mock_radio::MockRadio`] + a record-only
//!   `MockGateway`. Fully wired and exercised by the headless end-to-end pay-loop.
//! - **Real** (G5 native + G8): a BLE radio (`CoreBluetooth` / `android.bluetooth.le`,
//!   the native shim) + an RPC gateway (`sendRawTransaction`). The native shim and the
//!   RPC client are not built yet — see the `// G5:` / `// G8:` anchors. Constructing one
//!   returns [`MeshError::NotStarted`] until they land.

use std::sync::Arc;

use crate::gateway::MeshGateway;
use crate::radio::BleRadio;
use crate::transport::MeshError;

/// Which backing a [`MeshProvider`] wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Pure-Rust, radio-free mock (CI / headless tests).
    Mock,
    /// Real native BLE radio + RPC gateway (G5 native + G8; money-path, gated).
    Real,
}

/// A radio + gateway pair behind one `kind`-tagged handle.
///
/// This is the seam the native shim (G5) and the RPC gateway (G8) plug into; today only
/// [`ProviderKind::Mock`] can be built.
pub struct MeshProvider {
    kind: ProviderKind,
    radio: Arc<dyn BleRadio>,
    gateway: Arc<dyn MeshGateway>,
}

impl MeshProvider {
    /// Build a mock provider from a pure-Rust radio + a mock-capable gateway.
    pub fn mock(radio: Arc<dyn BleRadio>, gateway: Arc<dyn MeshGateway>) -> Self {
        MeshProvider {
            kind: ProviderKind::Mock,
            radio,
            gateway,
        }
    }

    /// Attempt to build the real provider.
    ///
    /// G5: real native BLE [`BleRadio`] shim (concurrent central + peripheral).
    /// G8: real RPC [`MeshGateway`] (`sendRawTransaction`, money-path / gated).
    /// Neither is merged, so this is a deliberate seam stub.
    pub fn real() -> Result<Self, MeshError> {
        Err(MeshError::NotStarted)
    }

    /// Which kind this provider is.
    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// The radio (shared handle) — the ADR-0002 byte-stream seam a [`crate::node::MeshNode`]
    /// drives.
    pub fn radio(&self) -> Arc<dyn BleRadio> {
        self.radio.clone()
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
    use crate::mock_radio::{MockEther, MockRadio};

    #[test]
    fn mock_provider_exposes_its_seam() {
        let ether = MockEther::new();
        let radio: Arc<dyn BleRadio> = MockRadio::new("p1", ether.clone());
        let gateway: Arc<dyn MeshGateway> = Arc::new(MockGateway::new(default_network()));
        let provider = MeshProvider::mock(radio, gateway);

        assert_eq!(provider.kind(), ProviderKind::Mock);
        // The gateway handle is live and record-only.
        assert!(provider.gateway().submit(b"x".to_vec()).is_ok());
        // The radio handle is the real seam (start it without panicking).
        provider.radio().start_scanning();
        ether.shutdown();
    }

    #[test]
    fn real_provider_is_not_available_yet() {
        assert_eq!(MeshProvider::real().map(|_| ()), Err(MeshError::NotStarted));
    }
}
