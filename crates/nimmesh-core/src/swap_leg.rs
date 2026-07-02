//! # swap_leg — the chain-agnostic swap-leg seam + a faithful mock HTLC chain (mesh swap F4)
//!
//! One side of a swap on one chain, behind a single [`SwapLeg`] trait — exactly the pluggable
//! pattern the signer uses for `KeyOrigin`. The swap state machine ([`crate::swap`]) and the mesh
//! never learn which chains are involved; they call `fund` / `claim` / `refund` on a `SwapLeg`.
//!
//! - `NimiqLeg` (built later on [`crate::nimiq::htlc`]) drives the native-HTLC NIM side.
//! - `BitcoinLeg` is a gated stub (F5): real P2WSH-HTLC signing + a BTC watcher → `needs:owner`.
//! - [`MockLeg`] is a faithful in-memory HTLC escrow used by the F4 end-to-end proof. It enforces
//!   the **real** HTLC rules — claim needs `SHA-256(preimage) == hashlock` and `head ≤ timeout`;
//!   refund needs `head > timeout` — and **reveals the preimage once claimed** (as an on-chain
//!   claim does), so the e2e can prove the atomicity invariant: *no one-sided settlement*.
//!
//! No key material here; the hashlock + preimage are the only secrets, and the preimage only
//! becomes visible after it has been spent into a claim (i.e. once it is public anyway).

use sha2::{Digest, Sha256};

/// The hashlock / preimage length in bytes (SHA-256).
pub const HASH_LEN: usize = 32;

/// `H = SHA-256(preimage)` — the cross-chain hashlock (Bitcoin's HTLC hashes with SHA-256, so
/// the NIM and BTC legs must share a SHA-256 lock).
pub fn sha256(data: &[u8]) -> [u8; HASH_LEN] {
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut arr = [0u8; HASH_LEN];
    arr.copy_from_slice(&out);
    arr
}

/// The disposition of a single leg's escrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LegOutcome {
    /// Nothing locked yet.
    #[default]
    Unfunded,
    /// Funds are locked in the HTLC, unresolved.
    Funded,
    /// The recipient claimed with the preimage (the preimage is now public).
    Claimed,
    /// The funder reclaimed via the timeout path.
    Refunded,
}

/// The public parameters of one HTLC leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtlcParams {
    /// `H` — the shared hashlock.
    pub hashlock: [u8; HASH_LEN],
    /// Amount locked, in the chain's base unit.
    pub amount: u64,
    /// The HTLC timeout, a block height on this leg's chain.
    pub timeout: u64,
}

/// An illegal leg operation (the mock enforces the same rules a real validator would).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegError {
    /// Tried to fund a leg that is already funded/resolved.
    NotUnfunded,
    /// Tried to claim/refund a leg that is not in the `Funded` state.
    NotFunded,
    /// Claim presented a preimage whose hash does not match the hashlock.
    WrongPreimage,
    /// Claim attempted at/after the timeout (the claim window has closed).
    ClaimWindowClosed {
        /// The leg timeout.
        timeout: u64,
        /// The head the claim was attempted at.
        head: u64,
    },
    /// Refund attempted before the timeout elapsed.
    TimeoutNotReached {
        /// The leg timeout.
        timeout: u64,
        /// The head the refund was attempted at.
        head: u64,
    },
}

/// One side of a swap on one chain. `NimiqLeg`/`BitcoinLeg`/[`MockLeg`] implement it; the swap
/// engine stays chain-agnostic.
pub trait SwapLeg {
    /// Lock `params.amount` into a fresh HTLC. Idempotency / double-fund is rejected.
    fn fund(&mut self, params: HtlcParams) -> Result<(), LegError>;
    /// Claim the funded HTLC by revealing `preimage` (recipient path; before the timeout).
    fn claim(&mut self, preimage: [u8; HASH_LEN], head: u64) -> Result<(), LegError>;
    /// Reclaim the funded HTLC via the timeout path (funder path; after the timeout).
    fn refund(&mut self, head: u64) -> Result<(), LegError>;
    /// The current disposition of this leg.
    fn outcome(&self) -> LegOutcome;
    /// The preimage, **only** once the leg has been claimed (as it would be visible on-chain).
    fn revealed_preimage(&self) -> Option<[u8; HASH_LEN]>;
}

/// A faithful in-memory HTLC escrow for the F4 end-to-end proof. Enforces the real claim/refund
/// rules and reveals the preimage on claim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MockLeg {
    params: Option<HtlcParams>,
    outcome: LegOutcome,
    revealed: Option<[u8; HASH_LEN]>,
}

impl MockLeg {
    /// A fresh, unfunded mock leg.
    pub fn new() -> Self {
        MockLeg::default()
    }

    /// The locked amount, if funded.
    pub fn amount(&self) -> Option<u64> {
        self.params.map(|p| p.amount)
    }
}

impl SwapLeg for MockLeg {
    fn fund(&mut self, params: HtlcParams) -> Result<(), LegError> {
        if self.outcome != LegOutcome::Unfunded {
            return Err(LegError::NotUnfunded);
        }
        self.params = Some(params);
        self.outcome = LegOutcome::Funded;
        Ok(())
    }

    fn claim(&mut self, preimage: [u8; HASH_LEN], head: u64) -> Result<(), LegError> {
        let params = match (self.outcome, self.params) {
            (LegOutcome::Funded, Some(p)) => p,
            _ => return Err(LegError::NotFunded),
        };
        if sha256(&preimage) != params.hashlock {
            return Err(LegError::WrongPreimage);
        }
        if head > params.timeout {
            return Err(LegError::ClaimWindowClosed {
                timeout: params.timeout,
                head,
            });
        }
        self.outcome = LegOutcome::Claimed;
        // Claiming spends the preimage into the chain — it is now public.
        self.revealed = Some(preimage);
        Ok(())
    }

    fn refund(&mut self, head: u64) -> Result<(), LegError> {
        let params = match (self.outcome, self.params) {
            (LegOutcome::Funded, Some(p)) => p,
            _ => return Err(LegError::NotFunded),
        };
        if head <= params.timeout {
            return Err(LegError::TimeoutNotReached {
                timeout: params.timeout,
                head,
            });
        }
        self.outcome = LegOutcome::Refunded;
        Ok(())
    }

    fn outcome(&self) -> LegOutcome {
        self.outcome
    }

    fn revealed_preimage(&self) -> Option<[u8; HASH_LEN]> {
        self.revealed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(hashlock: [u8; 32], timeout: u64) -> HtlcParams {
        HtlcParams {
            hashlock,
            amount: 100_000,
            timeout,
        }
    }

    #[test]
    fn fund_claim_reveals_the_preimage() {
        let secret = [9u8; 32];
        let mut leg = MockLeg::new();
        leg.fund(params(sha256(&secret), 5_000)).unwrap();
        assert_eq!(leg.outcome(), LegOutcome::Funded);
        assert_eq!(leg.revealed_preimage(), None);
        leg.claim(secret, 1_000).unwrap();
        assert_eq!(leg.outcome(), LegOutcome::Claimed);
        // The preimage is now public (as it would be on-chain) — this is how the counterparty
        // learns the secret.
        assert_eq!(leg.revealed_preimage(), Some(secret));
    }

    #[test]
    fn claim_rejects_wrong_preimage() {
        let mut leg = MockLeg::new();
        leg.fund(params(sha256(&[1u8; 32]), 5_000)).unwrap();
        assert_eq!(leg.claim([2u8; 32], 100), Err(LegError::WrongPreimage));
        assert_eq!(leg.outcome(), LegOutcome::Funded); // unchanged — no theft
        assert_eq!(leg.revealed_preimage(), None);
    }

    #[test]
    fn claim_rejected_after_timeout_refund_only_after() {
        let secret = [7u8; 32];
        let mut leg = MockLeg::new();
        leg.fund(params(sha256(&secret), 5_000)).unwrap();
        // Past the timeout, the recipient can no longer claim...
        assert!(matches!(
            leg.claim(secret, 5_001),
            Err(LegError::ClaimWindowClosed { .. })
        ));
        // ...and before it, the funder cannot refund...
        assert!(matches!(
            leg.refund(4_999),
            Err(LegError::TimeoutNotReached { .. })
        ));
        // ...but after it, refund works.
        leg.refund(5_001).unwrap();
        assert_eq!(leg.outcome(), LegOutcome::Refunded);
    }

    #[test]
    fn double_fund_rejected() {
        let mut leg = MockLeg::new();
        leg.fund(params([0u8; 32], 1)).unwrap();
        assert_eq!(leg.fund(params([0u8; 32], 1)), Err(LegError::NotUnfunded));
    }
}
