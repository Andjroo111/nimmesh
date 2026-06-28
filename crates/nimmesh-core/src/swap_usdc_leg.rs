//! # swap_usdc_leg — the USDC-on-Polygon counterparty leg model (P2, behind `polygon-leg`)
//!
//! A faithful in-memory model of the Polygon **HTLC contract** that escrows USDC, implementing the
//! chain-agnostic [`crate::swap_leg::SwapLeg`] trait so it plugs into the swap engine exactly like
//! [`crate::swap_leg::MockLeg`]. It proves a NIM⇄USDC swap is atomic end to end in SIM
//! (`swap_usdc_e2e_tests`), the same way `MockLeg` proves it for the NIM/BTC legs — **no EVM
//! tx-building, no RPC, no funds** (those are P3/P4, gated). This models the *contract's behaviour*.
//!
//! ## The on-chain shape this mirrors
//!
//! A standard Solidity hashed-timelock contract, with ONE deliberate cross-chain choice: it hashes
//! the preimage with the **SHA-256 precompile** (address `0x02`), NOT `keccak256`, so the lock
//! `H = SHA-256(S)` is byte-identical to the NIM and BTC legs and **one secret `S` unlocks all
//! three**. Shape (USDC is an ERC-20 the contract escrows via `transferFrom` after an `approve`):
//!
//! ```solidity
//! // newSwap pulls `amount` micro-USDC from msg.sender into escrow under a fresh swapId.
//! function newSwap(address receiver, uint256 amount, bytes32 hashlock, uint256 timelock) returns (bytes32 swapId)
//! function withdraw(bytes32 swapId, bytes32 secret)  // require(sha256(secret) == hashlock && block.timestamp < timelock)
//! function refund(bytes32 swapId)                     // require(block.timestamp >= timelock)
//! ```
//!
//! `swapId` is derived ON-CHAIN (never trusted from the caller) as
//! `keccak256(abi.encodePacked(sender, receiver, amount, hashlock, timelock))` — so two swaps with
//! identical parameters map to the same slot and the contract rejects the second while the first is
//! live (the same anti-replay a real HTLC contract enforces). Keccak is used ONLY for this id +
//! addressing (see [`crate::evm`]); the hashlock itself stays SHA-256.
//!
//! No key material here; the recipient/sender are public EVM addresses and the preimage only becomes
//! visible after `withdraw` spends it on-chain (i.e. once it is public anyway).

use crate::evm::keccak256;
use crate::swap_leg::{sha256, HtlcParams, LegError, LegOutcome, SwapLeg, HASH_LEN};

/// USDC has **6 decimals** on Polygon, so amounts are in micro-USDC (1 USDC = 1_000_000). The leg
/// carries them as the same `u64` base unit every [`SwapLeg`] uses; `1.50 USDC` = `1_500_000`.
pub const MICRO_USDC: u64 = 1_000_000;

/// An EVM 20-byte address (e.g. from [`crate::evm::evm_address`]). The funder and recipient of the
/// USDC escrow are EVM accounts, not Nimiq/Bitcoin keys.
pub type EvmAddress = [u8; 20];

/// A 32-byte EVM swap id — `keccak256(abi.encodePacked(sender, receiver, amount, hashlock, timelock))`.
pub type UsdcSwapId = [u8; 32];

/// Left-pad a `u64` into a 32-byte big-endian EVM `uint256` word (the ABI encoding of `amount` /
/// `timelock` inside `abi.encodePacked`).
fn u256_be(v: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&v.to_be_bytes());
    out
}

/// The contract's on-chain `swapId` derivation: `keccak256(abi.encodePacked(sender, receiver,
/// amount, hashlock, timelock))`. `abi.encodePacked` concatenates with NO padding between fields —
/// `address` is 20 bytes, `uint256` is a 32-byte big-endian word, `bytes32` is 32 bytes — so the
/// pre-image is `20 + 20 + 32 + 32 + 32 = 136` bytes. Deterministic + bound to every parameter, so
/// changing any field yields a different id (and the contract can't be tricked into a slot collision).
pub fn usdc_swap_id(
    sender: &EvmAddress,
    receiver: &EvmAddress,
    amount: u64,
    hashlock: &[u8; HASH_LEN],
    timelock: u64,
) -> UsdcSwapId {
    let mut packed = Vec::with_capacity(136);
    packed.extend_from_slice(sender);
    packed.extend_from_slice(receiver);
    packed.extend_from_slice(&u256_be(amount));
    packed.extend_from_slice(hashlock);
    packed.extend_from_slice(&u256_be(timelock));
    keccak256(&packed)
}

/// The live escrow entry the contract stores under a `swapId` while a swap is unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UsdcSwapState {
    swap_id: UsdcSwapId,
    params: HtlcParams,
}

/// One USDC-on-Polygon HTLC leg — a faithful model of the escrow contract for one swap. Created with
/// the funder's + recipient's EVM addresses; `fund`/`claim`/`refund` mirror `newSwap`/`withdraw`/
/// `refund`. Enforces the SAME rules a real validator would (SHA-256 hashlock, claim-before-timeout,
/// refund-after-timeout) and reveals the preimage on claim — so the e2e can prove atomicity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsdcLeg {
    /// The funder's EVM address (`newSwap` caller), whose USDC is escrowed.
    sender: EvmAddress,
    /// The recipient's EVM address (`withdraw` caller), who claims with the secret.
    receiver: EvmAddress,
    state: Option<UsdcSwapState>,
    outcome: LegOutcome,
    revealed: Option<[u8; HASH_LEN]>,
}

impl UsdcLeg {
    /// A fresh, unfunded USDC leg between two EVM accounts (`sender` funds, `receiver` claims).
    pub fn new(sender: EvmAddress, receiver: EvmAddress) -> Self {
        UsdcLeg {
            sender,
            receiver,
            state: None,
            outcome: LegOutcome::Unfunded,
            revealed: None,
        }
    }

    /// The on-chain `swapId` of the escrowed swap, once funded.
    pub fn swap_id(&self) -> Option<UsdcSwapId> {
        self.state.map(|s| s.swap_id)
    }

    /// The escrowed amount in micro-USDC, if funded.
    pub fn amount(&self) -> Option<u64> {
        self.state.map(|s| s.params.amount)
    }

    /// The funder's EVM address.
    pub fn sender(&self) -> EvmAddress {
        self.sender
    }

    /// The recipient's EVM address.
    pub fn receiver(&self) -> EvmAddress {
        self.receiver
    }
}

impl SwapLeg for UsdcLeg {
    fn fund(&mut self, params: HtlcParams) -> Result<(), LegError> {
        if self.outcome != LegOutcome::Unfunded {
            return Err(LegError::NotUnfunded);
        }
        let swap_id = usdc_swap_id(
            &self.sender,
            &self.receiver,
            params.amount,
            &params.hashlock,
            params.timeout,
        );
        self.state = Some(UsdcSwapState { swap_id, params });
        self.outcome = LegOutcome::Funded;
        Ok(())
    }

    fn claim(&mut self, preimage: [u8; HASH_LEN], head: u64) -> Result<(), LegError> {
        let params = match (self.outcome, self.state) {
            (LegOutcome::Funded, Some(s)) => s.params,
            _ => return Err(LegError::NotFunded),
        };
        // The contract verifies the hashlock with the SHA-256 precompile (0x02), NOT keccak — this is
        // what makes `H` shared across the NIM/BTC/USDC legs so one secret unlocks all three.
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
        // `withdraw` spends the secret into the Polygon chain — it is now public, so the NIM funder
        // can read it off the claim and settle the other leg.
        self.revealed = Some(preimage);
        Ok(())
    }

    fn refund(&mut self, head: u64) -> Result<(), LegError> {
        let params = match (self.outcome, self.state) {
            (LegOutcome::Funded, Some(s)) => s.params,
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

/// Build the USDC counterparty leg for a MATCHED NIM⇄USDC intent pair (P7): the escrow's `sender`
/// (funder) is the USDC-giver's `evm_address`, its `receiver` (claimant) is the NIM-giver's
/// `evm_address` — the two payout addresses carried in the discovery intents ([`crate::swap_intent`]).
/// The caller has already confirmed the pair matches (`SwapIntent::would_initiate_against`) and that
/// the counterparty leg is USDC (`swap_leg_select::counterparty_leg_for`).
pub fn usdc_leg_for_pair(
    nim_giver: &crate::swap_intent::SwapIntent,
    usdc_giver: &crate::swap_intent::SwapIntent,
) -> UsdcLeg {
    UsdcLeg::new(usdc_giver.evm_address, nim_giver.evm_address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::keccak256;

    // Two distinct EVM accounts for the funder + recipient (derived addresses don't matter to the
    // model; any 20-byte value is a valid EVM address).
    const ALICE: EvmAddress = [0xA1; 20];
    const BOB: EvmAddress = [0xB2; 20];

    fn params(hashlock: [u8; 32], timeout: u64) -> HtlcParams {
        HtlcParams {
            hashlock,
            amount: 25 * MICRO_USDC, // 25.000000 USDC
            timeout,
        }
    }

    #[test]
    fn fund_claim_reveals_the_preimage() {
        let secret = [9u8; 32];
        let mut leg = UsdcLeg::new(ALICE, BOB);
        leg.fund(params(sha256(&secret), 5_000)).unwrap();
        assert_eq!(leg.outcome(), LegOutcome::Funded);
        assert_eq!(leg.amount(), Some(25 * MICRO_USDC));
        assert!(leg.swap_id().is_some()); // contract assigned a slot
        assert_eq!(leg.revealed_preimage(), None);
        leg.claim(secret, 1_000).unwrap();
        assert_eq!(leg.outcome(), LegOutcome::Claimed);
        // The secret is now public on Polygon (as it would be after `withdraw`).
        assert_eq!(leg.revealed_preimage(), Some(secret));
    }

    #[test]
    fn claim_rejects_wrong_preimage() {
        let mut leg = UsdcLeg::new(ALICE, BOB);
        leg.fund(params(sha256(&[1u8; 32]), 5_000)).unwrap();
        assert_eq!(leg.claim([2u8; 32], 100), Err(LegError::WrongPreimage));
        assert_eq!(leg.outcome(), LegOutcome::Funded); // unchanged — no theft
        assert_eq!(leg.revealed_preimage(), None);
    }

    #[test]
    fn claim_rejected_after_timeout_refund_only_after() {
        let secret = [7u8; 32];
        let mut leg = UsdcLeg::new(ALICE, BOB);
        leg.fund(params(sha256(&secret), 5_000)).unwrap();
        // Past the timeout, the recipient can no longer `withdraw`...
        assert!(matches!(
            leg.claim(secret, 5_001),
            Err(LegError::ClaimWindowClosed { .. })
        ));
        // ...and before it, the funder cannot `refund`...
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
        let mut leg = UsdcLeg::new(ALICE, BOB);
        leg.fund(params([0u8; 32], 1)).unwrap();
        assert_eq!(leg.fund(params([0u8; 32], 1)), Err(LegError::NotUnfunded));
    }

    #[test]
    fn swap_id_is_deterministic_and_bound_to_every_param() {
        let hashlock = sha256(&[3u8; 32]);
        let id = usdc_swap_id(&ALICE, &BOB, 25 * MICRO_USDC, &hashlock, 5_000);
        // Deterministic: same inputs → same id.
        assert_eq!(
            id,
            usdc_swap_id(&ALICE, &BOB, 25 * MICRO_USDC, &hashlock, 5_000)
        );
        // Bound to every field: changing any one yields a different slot (no collision/replay).
        assert_ne!(
            id,
            usdc_swap_id(&BOB, &BOB, 25 * MICRO_USDC, &hashlock, 5_000)
        );
        assert_ne!(
            id,
            usdc_swap_id(&ALICE, &ALICE, 25 * MICRO_USDC, &hashlock, 5_000)
        );
        assert_ne!(
            id,
            usdc_swap_id(&ALICE, &BOB, 25 * MICRO_USDC + 1, &hashlock, 5_000)
        );
        assert_ne!(
            id,
            usdc_swap_id(&ALICE, &BOB, 25 * MICRO_USDC, &sha256(&[4u8; 32]), 5_000)
        );
        assert_ne!(
            id,
            usdc_swap_id(&ALICE, &BOB, 25 * MICRO_USDC, &hashlock, 5_001)
        );
        // And the funded leg reports exactly that derivation.
        let mut leg = UsdcLeg::new(ALICE, BOB);
        leg.fund(params(hashlock, 5_000)).unwrap();
        assert_eq!(leg.swap_id(), Some(id));
    }

    #[test]
    fn hashlock_is_sha256_not_keccak_so_the_lock_is_cross_chain() {
        // THE cross-chain choice. The contract verifies with the SHA-256 precompile, so a secret
        // whose SHA-256 matches claims — and a lock built from the EVM's native keccak256(secret)
        // would NOT be claimable by that secret. This is why H is shared with the NIM/BTC legs.
        let secret = [5u8; 32];
        let sha_lock = sha256(&secret);
        let keccak_lock = keccak256(&secret);
        assert_ne!(sha_lock, keccak_lock); // the two hashes genuinely differ

        // Locked with SHA-256 (our cross-chain lock): the secret claims.
        let mut ok = UsdcLeg::new(ALICE, BOB);
        ok.fund(params(sha_lock, 5_000)).unwrap();
        ok.claim(secret, 100).unwrap();
        assert_eq!(ok.outcome(), LegOutcome::Claimed);

        // Locked with keccak256 (a vanilla EVM HTLC): the SAME secret can't claim — proving the
        // contract MUST hash with the 0x02 precompile for the cross-chain swap to work.
        let mut bad = UsdcLeg::new(ALICE, BOB);
        bad.fund(params(keccak_lock, 5_000)).unwrap();
        assert_eq!(bad.claim(secret, 100), Err(LegError::WrongPreimage));
        assert_eq!(bad.outcome(), LegOutcome::Funded); // safe — refundable by its funder later
    }
}
