//! # swap_builder — the per-chain swap-tx **builder** seam (mesh swap F5)
//!
//! Distinct from [`crate::swap_leg`] (the escrow *model* that proves atomicity): this is the seam
//! that **produces the signed tx bytes** a swap floods over the mesh. One [`LegBuilder`] per chain:
//!
//! - [`NimiqLeg`] — **built + working** for the funding tx: it assembles a byte-exact Nimiq HTLC
//!   creation tx (via [`crate::nimiq::htlc`]) and signs it through the same `EnclaveKey` seam as
//!   the basic-transfer signer (the seed never crosses FFI; only a pubkey + signature do). The
//!   resulting ~248 B wire is the one `@nimiq/core`'s validator already accepts
//!   (`scripts/fixtures/feasibility-test.mjs`). **Testnet by default.**
//! - The NIM **claim / refund** txs need the HTLC resolve **proof**, which is gated against
//!   `core-rs-albatross` / a live testnet redeem (`@nimiq/core` JS can't build it — see
//!   `docs/swap/F0-HTLC-FINDINGS.md`), so they return [`LegBuildError::RedeemProofPending`].
//! - [`BitcoinLeg`] — a documented **gated stub**: every method returns
//!   [`LegBuildError::Gated`]. The real P2WSH-HTLC signer + a BTC node/funds are **`needs:owner`**.
//!
//! No seed/preimage-before-reveal touches the mesh or a log: a funding tx is public + broadcast-safe,
//! and the preimage only appears inside a claim tx (which is meant to go on-chain).

use std::sync::Arc;

use crate::nimiq::address::{Address, ADDRESS_LEN};
use crate::nimiq::htlc::{HashAlgorithm, HtlcCreation, HtlcCreationData};
use crate::nimiq::signer::EnclaveKey;
use crate::nimiq::tx::{signature_proof_single_sig, PUBLIC_KEY_LEN, SIGNATURE_LEN};
use crate::swap_wire::SwapLegId;

/// Albatross testnet network id — the only network a leg builder targets (mainnet is gated).
pub const TESTNET_NETWORK_ID: u8 = 5;

/// The public terms needed to build a funding tx on one leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingTerms {
    /// `H` — the shared hashlock (`SHA-256(secret)`).
    pub hashlock: [u8; 32],
    /// Amount to lock, in the chain's base unit.
    pub amount: u64,
    /// The HTLC timeout block height for this leg.
    pub timeout: u64,
    /// The claimant's address on this chain (NIM: 20 raw address bytes).
    pub recipient: Vec<u8>,
    /// `validityStartHeight` for the funding tx (anchor to the head-beacon).
    pub validity_start_height: u32,
}

/// A failure building a leg's tx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegBuildError {
    /// This chain's leg is not available autonomously — it needs Andjroo (a real node + funds).
    Gated {
        /// The chain that is gated.
        chain: &'static str,
        /// What Andjroo must provide.
        needs: &'static str,
    },
    /// The NIM resolve (claim/refund) proof is not wired yet — gated against `core-rs-albatross`
    /// / a live testnet redeem (`@nimiq/core` JS cannot build or verify it).
    RedeemProofPending,
    /// A provided address / field had the wrong shape.
    BadField {
        /// What was wrong.
        reason: &'static str,
    },
    /// The enclave returned key material of the wrong length.
    BadKeyMaterial,
}

impl std::fmt::Display for LegBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LegBuildError::Gated { chain, needs } => {
                write!(f, "{chain} leg is gated (needs:owner): {needs}")
            }
            LegBuildError::RedeemProofPending => write!(
                f,
                "NIM resolve proof pending (gated against core-rs-albatross / a live testnet redeem)"
            ),
            LegBuildError::BadField { reason } => write!(f, "bad field: {reason}"),
            LegBuildError::BadKeyMaterial => write!(f, "enclave returned wrong-length key material"),
        }
    }
}

impl std::error::Error for LegBuildError {}

/// The per-chain tx-builder seam. `NimiqLeg` is real (funding); `BitcoinLeg` is a gated stub. The
/// swap engine calls these to get broadcast-safe bytes to flood; it never sees a key.
pub trait LegBuilder {
    /// Which leg this builder drives.
    fn leg_id(&self) -> SwapLegId;
    /// Build + sign the funding tx that locks `terms.amount` into a fresh HTLC.
    fn build_funding(&self, terms: &FundingTerms) -> Result<Vec<u8>, LegBuildError>;
    /// Build the claim tx that spends the funded HTLC by revealing `preimage`.
    fn build_claim(&self, funded_tx: &[u8], preimage: [u8; 32]) -> Result<Vec<u8>, LegBuildError>;
    /// Build the refund tx that reclaims the funded HTLC after its timeout.
    fn build_refund(&self, funded_tx: &[u8]) -> Result<Vec<u8>, LegBuildError>;
}

/// The Nimiq leg: builds a byte-exact, signed HTLC **funding** tx. Holds only the opaque
/// [`EnclaveKey`] seam — the seed never crosses it; only a pubkey + a 64-byte signature do.
#[derive(Clone)]
pub struct NimiqLeg {
    key: Arc<dyn EnclaveKey>,
    network_id: u8,
}

impl NimiqLeg {
    /// A testnet Nimiq leg over the given enclave key.
    pub fn new(key: Arc<dyn EnclaveKey>) -> Self {
        NimiqLeg {
            key,
            network_id: TESTNET_NETWORK_ID,
        }
    }

    /// This leg's funding address (derived from the enclave public key).
    pub fn address(&self) -> Result<Address, LegBuildError> {
        Ok(Address::from_public_key(&self.public_key()?))
    }

    fn public_key(&self) -> Result<[u8; PUBLIC_KEY_LEN], LegBuildError> {
        self.key
            .public_key()
            .try_into()
            .map_err(|_| LegBuildError::BadKeyMaterial)
    }
}

impl LegBuilder for NimiqLeg {
    fn leg_id(&self) -> SwapLegId {
        SwapLegId::Nim
    }

    fn build_funding(&self, terms: &FundingTerms) -> Result<Vec<u8>, LegBuildError> {
        let public_key = self.public_key()?;
        let funder = Address::from_public_key(&public_key);
        let recipient: [u8; ADDRESS_LEN] =
            terms
                .recipient
                .as_slice()
                .try_into()
                .map_err(|_| LegBuildError::BadField {
                    reason: "NIM recipient must be 20 raw address bytes",
                })?;
        let creation = HtlcCreation {
            funder,
            data: HtlcCreationData {
                htlc_sender: funder, // refund goes back to the funder
                htlc_recipient: Address::from_bytes(recipient),
                hash_algorithm: HashAlgorithm::Sha256, // SHA-256 for cross-chain (BTC-compatible)
                hash_root: terms.hashlock,
                hash_count: 1,
                timeout: terms.timeout,
            },
            value: terms.amount,
            fee: 0,
            validity_start_height: terms.validity_start_height,
            network_id: self.network_id,
        };
        // Sign the creation content with the enclave key; the seed never leaves it.
        let content = creation.serialize_content();
        let signature: [u8; SIGNATURE_LEN] = self
            .key
            .sign_content(content)
            .try_into()
            .map_err(|_| LegBuildError::BadKeyMaterial)?;
        let proof = signature_proof_single_sig(&public_key, &signature);
        Ok(creation.serialize_wire(&proof))
    }

    fn build_claim(
        &self,
        _funded_tx: &[u8],
        _preimage: [u8; 32],
    ) -> Result<Vec<u8>, LegBuildError> {
        // The claim is an HTLC redeem (RegularTransfer) tx — its serializeContent is byte-exact
        // (F1) but the resolve PROOF is gated against core-rs-albatross / a live testnet redeem.
        Err(LegBuildError::RedeemProofPending)
    }

    fn build_refund(&self, _funded_tx: &[u8]) -> Result<Vec<u8>, LegBuildError> {
        Err(LegBuildError::RedeemProofPending)
    }
}

/// The Bitcoin leg — a documented **gated stub**. The real P2WSH-HTLC signer + a BTC node and
/// real funds are **`needs:owner`**; the loop ships the seam so the swap engine compiles against
/// a real second leg, and Andjroo drops in the implementation behind this same trait.
#[derive(Debug, Clone, Copy, Default)]
pub struct BitcoinLeg;

impl BitcoinLeg {
    const NEEDS: &'static str =
        "a Bitcoin node/RPC, real funds, and a P2WSH-HTLC signer/watcher (mainnet is Andjroo-only)";

    /// A new gated Bitcoin leg stub.
    pub fn new() -> Self {
        BitcoinLeg
    }
}

impl LegBuilder for BitcoinLeg {
    fn leg_id(&self) -> SwapLegId {
        SwapLegId::Counterparty
    }
    fn build_funding(&self, _terms: &FundingTerms) -> Result<Vec<u8>, LegBuildError> {
        Err(LegBuildError::Gated {
            chain: "bitcoin",
            needs: Self::NEEDS,
        })
    }
    fn build_claim(
        &self,
        _funded_tx: &[u8],
        _preimage: [u8; 32],
    ) -> Result<Vec<u8>, LegBuildError> {
        Err(LegBuildError::Gated {
            chain: "bitcoin",
            needs: Self::NEEDS,
        })
    }
    fn build_refund(&self, _funded_tx: &[u8]) -> Result<Vec<u8>, LegBuildError> {
        Err(LegBuildError::Gated {
            chain: "bitcoin",
            needs: Self::NEEDS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nimiq::htlc::{HtlcCreation, HtlcCreationData};
    use crate::nimiq::signer::InMemoryEnclaveKey;

    fn nim_leg() -> NimiqLeg {
        NimiqLeg::new(Arc::new(InMemoryEnclaveKey::from_secret(&[3u8; 32])))
    }

    #[test]
    fn nimiq_leg_builds_a_signed_248_byte_funding_tx() {
        let leg = nim_leg();
        let funder = leg.address().unwrap();
        let recipient = Address::from_bytes([0x56; ADDRESS_LEN]);
        let terms = FundingTerms {
            hashlock: [0xDD; 32],
            amount: 100_000,
            timeout: 12_345,
            recipient: recipient.as_bytes().to_vec(),
            validity_start_height: 100,
        };
        let wire = leg.build_funding(&terms).unwrap();
        // A signed HTLC funding tx is the 248-byte wire @nimiq/core's validator accepts.
        assert_eq!(wire.len(), 248);
        assert_eq!(wire[0], 0x01); // Extended format

        // The unsigned wire matches the byte-exact serializer for the same params (the signature
        // is the only difference) — i.e. the leg fed `nimiq::htlc` the right contract.
        let expected_unsigned = HtlcCreation {
            funder,
            data: HtlcCreationData {
                htlc_sender: funder,
                htlc_recipient: recipient,
                hash_algorithm: HashAlgorithm::Sha256,
                hash_root: [0xDD; 32],
                hash_count: 1,
                timeout: 12_345,
            },
            value: 100_000,
            fee: 0,
            validity_start_height: 100,
            network_id: TESTNET_NETWORK_ID,
        }
        .serialize_wire(&[]);
        // The signed wire shares the unsigned prefix up to the (empty vs 98-byte) proof tail.
        let prefix = expected_unsigned.len() - 1; // drop the unsigned proof-length byte (0x00)
        assert_eq!(&wire[..prefix], &expected_unsigned[..prefix]);
    }

    #[test]
    fn nimiq_claim_and_refund_are_gated_on_the_redeem_proof() {
        let leg = nim_leg();
        assert_eq!(
            leg.build_claim(&[0u8; 248], [0u8; 32]),
            Err(LegBuildError::RedeemProofPending)
        );
        assert_eq!(
            leg.build_refund(&[0u8; 248]),
            Err(LegBuildError::RedeemProofPending)
        );
    }

    #[test]
    fn bitcoin_leg_is_a_clear_gated_stub() {
        let leg = BitcoinLeg::new();
        assert_eq!(leg.leg_id(), SwapLegId::Counterparty);
        let terms = FundingTerms {
            hashlock: [0; 32],
            amount: 1,
            timeout: 1,
            recipient: vec![],
            validity_start_height: 0,
        };
        assert!(matches!(
            leg.build_funding(&terms),
            Err(LegBuildError::Gated {
                chain: "bitcoin",
                ..
            })
        ));
        assert!(matches!(
            leg.build_claim(&[], [0; 32]),
            Err(LegBuildError::Gated { .. })
        ));
    }

    #[test]
    fn nimiq_leg_rejects_a_bad_recipient_length() {
        let leg = nim_leg();
        let terms = FundingTerms {
            hashlock: [0; 32],
            amount: 1,
            timeout: 1,
            recipient: vec![0u8; 19], // not 20 bytes
            validity_start_height: 0,
        };
        assert!(matches!(
            leg.build_funding(&terms),
            Err(LegBuildError::BadField { .. })
        ));
    }
}
