//! # node_tests — the MeshNode unit suite, extracted from `node.rs` so the logic module stays
//! under the 800-line ceiling (matches the repo's `swap_*_tests` sibling convention).
//! Also hosts the test-only swap-participant constructors (same guard motivation): they
//! touch no private `MeshNode` field — only the crate-visible `build` — so they live here.

use std::sync::Arc;

use crate::node::{to_sender_id, to_tx_id, MeshNode};
use crate::radio::BleRadio;
use crate::relay::RelayPolicy;
use crate::swap_session::SwapSession;
use crate::NetworkId;

impl MeshNode {
    /// G14 (test): a swap **participant** node — a plain node that also runs a [`SwapSession`] for
    /// `identity`, with the default sim signer (`MockSigner`), so it decodes its own `swap_id` off
    /// the swap stream, builds its tx bytes via the signer seam, and floods replies.
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
            NetworkId::Testnet,
        )
    }

    /// S2 / #73 (test): a participant node that also holds a NIM enclave key, so each `Propose` it
    /// originates over the discovery flow is authenticated (signed) before it floods. `propose_key`
    /// must own `identity.nim_address`.
    pub(crate) fn new_participant_signing(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        policy: RelayPolicy,
        identity: crate::swap_session::NodeIdentity,
        ladder: crate::swap::LadderParams,
        propose_key: Arc<dyn crate::nimiq::signer::EnclaveKey>,
    ) -> Arc<Self> {
        let session = SwapSession::new(identity, ladder).with_propose_signer(propose_key);
        Self::build(
            sender_id,
            radio,
            None,
            policy,
            false,
            Some(session),
            Some(Box::new(crate::swap_signer::MockSigner)),
            NetworkId::Testnet,
        )
    }

    /// G33 (test): a participant node restored from a crash-recovery snapshot (G31/G32) — its swap
    /// session is rebuilt from `snapshot` bytes so a funds-locked swap resumes its refund tick. A
    /// corrupt blob falls back to an empty session (the node starts clean rather than crashing).
    pub(crate) fn new_participant_restored(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        policy: RelayPolicy,
        identity: crate::swap_session::NodeIdentity,
        ladder: crate::swap::LadderParams,
        snapshot: Vec<u8>,
    ) -> Arc<Self> {
        let session = SwapSession::restore_bytes(identity.clone(), ladder, &snapshot)
            .unwrap_or_else(|_| SwapSession::new(identity, ladder));
        Self::build(
            sender_id,
            radio,
            None,
            policy,
            false,
            Some(session),
            Some(Box::new(crate::swap_signer::MockSigner)),
            NetworkId::Testnet,
        )
    }
}

#[test]
fn id_helpers_truncate_and_pad() {
    assert_eq!(to_sender_id(&[1, 2, 3]), [1, 2, 3, 0, 0, 0, 0, 0]);
    assert_eq!(to_sender_id(&[9; 12]), [9; 8]);
    let id = to_tx_id(&[7; 4]);
    assert_eq!(&id.0[..4], &[7, 7, 7, 7]);
    assert_eq!(id.0[4], 0);
}
