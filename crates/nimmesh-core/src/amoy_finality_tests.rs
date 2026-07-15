//! # amoy_finality_tests — the finalized fast-path on the LIVE-eligible Amoy USDC verifier
//! (ADR-0003 addendum, 2026-07-15). A sibling of [`super::tests`] (shared fixtures) kept in its
//! own file for the 800-line guard. All offline against [`MockAmoy`]: the `finalized` tag rides a
//! delegating wrapper so the shared fixture stays untouched — a fake that does not serve the tag
//! (the trait default's error) proves the depth-count fallback, a lying primary is capped by the
//! secondary's tag, and a secondary that cannot vouch blocks the fast path entirely.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use super::tests::{counterparty_expect, hashlock, new_swap_log, MockAmoy, ALICE_EVM_CLAIM, NOW_S};
use super::*;
use crate::amoy_swap_verifier::AmoyHtlcSwapVerifier;
use crate::evm::keccak256;
use crate::polygon_gateway::{EvmLog, EvmReceipt, EvmRpcError};
use crate::swap_funding_verify::{
    require_funded, ConfirmationPolicy, FundingObservation, FundingVerifier,
    FINALIZED_CONFIRMATIONS,
};
use crate::swap_intent::Asset;
use crate::swap_wire::SwapLegId;

/// [`MockAmoy`] plus a programmable `finalized` tag (`None` = the endpoint does not serve it —
/// the trait default's error, i.e. the depth-count fallback). Pure delegation otherwise.
struct FinalizedAmoy {
    inner: Arc<MockAmoy>,
    finalized: Option<u64>,
}

impl AmoyChain for FinalizedAmoy {
    fn gas_price(&self) -> Result<u64, EvmRpcError> {
        self.inner.gas_price()
    }
    fn transaction_count(&self, address: &EvmAddress) -> Result<u64, EvmRpcError> {
        self.inner.transaction_count(address)
    }
    fn balance(&self, address: &EvmAddress) -> Result<u128, EvmRpcError> {
        self.inner.balance(address)
    }
    fn send_raw(&self, raw: &[u8]) -> Result<String, EvmRpcError> {
        self.inner.send_raw(raw)
    }
    fn receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<EvmReceipt>, EvmRpcError> {
        self.inner.receipt(tx_hash)
    }
    fn call(&self, to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, EvmRpcError> {
        self.inner.call(to, data)
    }
    fn new_swap_logs_to(
        &self,
        htlc: &EvmAddress,
        recipient: &EvmAddress,
        from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError> {
        self.inner.new_swap_logs_to(htlc, recipient, from_block)
    }
    fn head(&self) -> Result<u64, EvmRpcError> {
        self.inner.head()
    }
    fn finalized_head(&self) -> Result<u64, EvmRpcError> {
        self.finalized
            .ok_or_else(|| EvmRpcError::Transport("fake: finalized tag unserved".to_string()))
    }
}

const RAW_FUNDING: [u8; 4] = [0xF0, 0xF1, 0xF2, 0xF3];
const ESCROW_BLOCK: u64 = 100;
const HEAD: u64 = 104; // raw depth 104 - 100 + 1 = 5 — BELOW the mainnet USDC floor of 8

/// A primary Amoy fake with the FundingProof-named tx mined at [`ESCROW_BLOCK`] and the matching
/// live escrow log visible — the shape of the moment a real swap awaits USDC burial.
fn primary(finalized: Option<u64>) -> Arc<FinalizedAmoy> {
    let chain = Arc::new(MockAmoy {
        head: AtomicU64::new(HEAD),
        ..MockAmoy::default()
    });
    chain.add_receipt(keccak256(&RAW_FUNDING), true, ESCROW_BLOCK);
    chain.logs.lock().unwrap().push(new_swap_log(
        [0x9F; 32],
        1_000_000,
        hashlock(),
        NOW_S + 5_000,
        ESCROW_BLOCK,
    ));
    chain.swap_states.lock().unwrap().insert([0x9F; 32], 1); // Live
    Arc::new(FinalizedAmoy {
        inner: chain,
        finalized,
    })
}

/// A secondary endpoint that agrees on head and re-reads the escrow Live (the M5 cross-read
/// happy shape), with its own `finalized` answer.
fn agreeing_secondary(finalized: Option<u64>) -> Arc<FinalizedAmoy> {
    let chain = Arc::new(MockAmoy {
        head: AtomicU64::new(HEAD),
        ..MockAmoy::default()
    });
    chain.swap_states.lock().unwrap().insert([0x9F; 32], 1); // Live on re-read
    Arc::new(FinalizedAmoy {
        inner: chain,
        finalized,
    })
}

fn verifier(
    primary: Arc<FinalizedAmoy>,
    secondary: Option<Arc<FinalizedAmoy>>,
) -> AmoyHtlcSwapVerifier {
    let mut v = AmoyHtlcSwapVerifier::new(
        primary,
        super::tests::HTLC,
        ALICE_EVM_CLAIM,
        Arc::new(PolygonFundingStore::new()),
    )
    .with_clock(Box::new(|| NOW_S));
    if let Some(sec) = secondary {
        v = v.with_secondary(sec);
    }
    v.note_funding_wire(SwapLegId::Counterparty, &RAW_FUNDING);
    v
}

fn confirmations_of(obs: FundingObservation) -> u32 {
    match obs {
        FundingObservation::Found { confirmations, .. } => confirmations,
        other => panic!("expected Found, got {other:?}"),
    }
}

#[test]
fn a_finalized_escrow_clears_the_mainnet_usdc_floor_at_raw_depth_5() {
    // The escrow is only 5 blocks deep (below the mainnet fallback floor of 8), but the
    // `finalized` tag has passed its inclusion block → deterministic finality reports
    // FINALIZED_CONFIRMATIONS and the gate clears at once. This is the ~2-3 min USDC
    // depth-wait the fast-finality profile removes.
    let v = verifier(primary(Some(ESCROW_BLOCK + 1)), None);
    let obs = v.observe(&counterparty_expect());
    assert_eq!(confirmations_of(obs.clone()), FINALIZED_CONFIRMATIONS);
    let need = ConfirmationPolicy::mainnet_defaults().required(Asset::Usdc);
    assert!(require_funded(&obs, &counterparty_expect(), need).is_ok());
    // Finality outranks any count — even the paranoid profile's 64-deep floor clears.
    let paranoid = ConfirmationPolicy::mainnet_paranoid().required(Asset::Usdc);
    assert!(require_funded(&obs, &counterparty_expect(), paranoid).is_ok());
}

#[test]
fn a_missing_finalized_tag_keeps_the_depth_count_and_the_floor_refuses() {
    // The endpoint does not serve the tag: the observation is EXACTLY the pre-finality raw
    // depth (5), which the mainnet floor (8) still refuses — the fallback is only ever
    // slower, never weaker.
    let v = verifier(primary(None), None);
    let obs = v.observe(&counterparty_expect());
    assert_eq!(confirmations_of(obs.clone()), 5);
    let need = ConfirmationPolicy::mainnet_defaults().required(Asset::Usdc);
    assert!(require_funded(&obs, &counterparty_expect(), need).is_err());
}

#[test]
fn an_escrow_above_the_finalized_height_keeps_depth_counting() {
    // Tag served, but finality has not reached the inclusion block yet → no fast path.
    let v = verifier(primary(Some(ESCROW_BLOCK - 1)), None);
    assert_eq!(confirmations_of(v.observe(&counterparty_expect())), 5);
}

#[test]
fn a_lying_primary_finalized_claim_is_capped_by_the_secondary() {
    // M5 lying-RPC posture on the finalized path: the primary claims finality far past the
    // escrow (1000); the independent secondary's tag is still below it (99) → the min wins,
    // the escrow is NOT finalized, and the raw depth (5) does not clear the floor (8) — a
    // single lying/MITM'd endpoint cannot fake finality into an authorization.
    let v = verifier(
        primary(Some(1_000)),
        Some(agreeing_secondary(Some(ESCROW_BLOCK - 1))),
    );
    let obs = v.observe(&counterparty_expect());
    assert_eq!(confirmations_of(obs.clone()), 5);
    let need = ConfirmationPolicy::mainnet_defaults().required(Asset::Usdc);
    assert!(require_funded(&obs, &counterparty_expect(), need).is_err());
}

#[test]
fn a_secondary_that_cannot_vouch_blocks_the_finality_fast_path() {
    // The wired secondary agrees on head + re-reads the escrow Live, but does NOT serve the
    // finalized tag → the primary's claim must not authorize alone; depth counting holds.
    let v = verifier(
        primary(Some(ESCROW_BLOCK + 1)),
        Some(agreeing_secondary(None)),
    );
    assert_eq!(confirmations_of(v.observe(&counterparty_expect())), 5);
}

#[test]
fn both_endpoints_vouching_finalizes_and_still_requires_the_live_cross_read() {
    // Happy cross-read: both tags at/past the inclusion block → finalized.
    let v = verifier(
        primary(Some(ESCROW_BLOCK + 2)),
        Some(agreeing_secondary(Some(ESCROW_BLOCK))),
    );
    assert_eq!(
        confirmations_of(v.observe(&counterparty_expect())),
        FINALIZED_CONFIRMATIONS
    );
    // But finality NEVER bypasses the M5 live re-read: a secondary that re-reads the escrow
    // as not-Live fails the whole observation closed, tags notwithstanding.
    let sec_notlive = Arc::new(FinalizedAmoy {
        inner: Arc::new(MockAmoy {
            head: AtomicU64::new(HEAD),
            ..MockAmoy::default()
        }),
        finalized: Some(ESCROW_BLOCK + 2),
    });
    let v2 = verifier(primary(Some(ESCROW_BLOCK + 2)), Some(sec_notlive));
    assert_eq!(
        v2.observe(&counterparty_expect()),
        FundingObservation::Absent
    );
}
