//! # swap_signer — the seam where a swap action becomes a signed on-chain tx (G26)
//!
//! The node-side swap driver ([`crate::swap_node`]) coordinates *which* chain action to take and
//! *when*; turning that action into the actual funding / claim transaction bytes is the **money-path**
//! — building and signing a real NIM HTLC or BTC P2WSH spend. That is gated (real keys, real funds),
//! so the driver calls it through this trait. A node holds one [`SwapSigner`]; today every participant
//! holds the deterministic [`MockSigner`] (sim stand-in bytes), and the real NIM/BTC signer drops in
//! later by implementing the same trait — no change to the driver or the protocol.
//!
//! The bytes are opaque to everything above the signer: the coordinator and the mesh carry them as a
//! `tx_wire` blob. The one semantic the responder relies on is that a **claim** reveals the secret
//! `S` (a real BTC claim embeds `S` in its witness; the sim carries `S` directly), so the responder
//! can read it back off the `PreimageReveal`.

use crate::swap_coordinator::SwapContext;
use crate::swap_wire::{SwapLegId, SWAP_ID_LEN};

/// Builds (in production: signs **and broadcasts**) the on-chain transactions a swap participant
/// must land. The driver feeds the returned `(tx_wire, tx_id)` straight into the coordinator and
/// floods the matching envelope. `Send + Sync` so a participant node can carry it across to its
/// worker thread.
///
/// v2 (Act 2): every method receives the swap's full [`SwapContext`] — hashlock, timelocks,
/// amounts, both parties' addressing — which is everything a REAL signer needs to build the
/// actual HTLC transactions. Methods are fallible (`None` = could not build/broadcast; the
/// driver simply doesn't advance, and the timelock refund path remains the safety floor). A
/// live signer does blocking chain IO here — acceptable on a dedicated rig node's worker; the
/// phone app keeps [`MockSigner`] until its own live wiring lands.
pub trait SwapSigner: Send + Sync {
    /// Build (live: and broadcast) the funding tx for this node's `leg` (initiator funds `Nim`,
    /// responder funds `Counterparty`). Returns the opaque `tx_wire` + its 32-byte `tx_id`.
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])>;

    /// Build (live: and broadcast) this node's claim, revealing/using `secret`. For the
    /// INITIATOR that is the counterparty-leg claim and the returned `tx_wire` MUST carry `S`
    /// in its first 32 bytes (the responder reads it off the `PreimageReveal`); for the
    /// RESPONDER (post-reveal) it is the NIM-leg claim, and the wire goes nowhere — the claim
    /// lives on-chain. Returns the opaque `tx_wire` + its `tx_id`.
    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])>;
}

/// The deterministic sim signer: stand-in tx bytes, no keys, no broadcast. The funding blobs are
/// fixed-length filler standing in for a signed NIM / BTC HTLC funding; the claim `tx_wire` is the
/// 32-byte secret itself (exactly what the responder extracts). This is the seam's reference
/// implementation until the real money-path signer is wired.
pub struct MockSigner;

impl SwapSigner for MockSigner {
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
        let (wire, tag) = match leg {
            SwapLegId::Nim => (vec![0x11; 248], 0xF1),
            SwapLegId::Counterparty => (vec![0x22; 120], 0xF2),
        };
        Some((wire, sim_tx_id(ctx.swap_id, tag)))
    }

    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        Some((secret.to_vec(), sim_tx_id(ctx.swap_id, 0xF3)))
    }
}

/// A deterministic stand-in tx id for a sim leg action (swap_id-derived; real ids come from the
/// signed tx the money-path builds).
pub(crate) fn sim_tx_id(swap_id: [u8; SWAP_ID_LEN], tag: u8) -> [u8; 32] {
    let mut id = [tag; 32];
    id[..SWAP_ID_LEN].copy_from_slice(&swap_id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> SwapContext {
        SwapContext {
            swap_id: [0x7A; SWAP_ID_LEN],
            terms: crate::swap::SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            hashlock: [0xC7; 32],
            nim_address: [0x11; 20],
            btc_address: vec![0x22; 20],
            btc_pubkey: [0x02; 33],
            give_amount: 200_000,
            take_amount: 100_000,
            network_id: 5,
        }
    }

    #[test]
    fn mock_signer_builds_the_expected_stand_ins() {
        let s = MockSigner;
        let c = ctx();

        let (nim_wire, nim_id) = s.build_funding(&c, SwapLegId::Nim).unwrap();
        assert_eq!(nim_wire, vec![0x11; 248]);
        let (btc_wire, btc_id) = s.build_funding(&c, SwapLegId::Counterparty).unwrap();
        assert_eq!(btc_wire, vec![0x22; 120]);
        assert_ne!(nim_id, btc_id); // distinct ids per leg

        // The claim carries S in its first 32 bytes — what the responder reads back.
        let secret = [0x42; 32];
        let (claim_wire, _) = s.build_claim(&c, secret).unwrap();
        assert_eq!(&claim_wire[..32], &secret);
    }
}
