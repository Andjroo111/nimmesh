//! # swap_claim_tests — the run-4 regression suite (2026-07-19 soak, one-sided responder loss)
//!
//! Run 4's chain of events, now pinned red→green: the initiator's `withdraw(S)` mined (S public,
//! its USDC leg claimed), the responder's `claim_nim` failed on ONE transient `rpc http 429`, and
//! the old driver settled anyway — the coordinator read `Settled`, was reaped, and nothing ever
//! retried the still-claimable NIM HTLC. These tests prove the ladder that replaces it:
//! broadcast-failure → stay `Revealed` + tick-retry; broadcast-success → still `Revealed` until
//! the claim is CHAIN-CONFIRMED on the (sim) ledger; never-lands → the honest `Lost` terminal at
//! the timelock, never a silent `Settled`. Node-level halves ride the real `MeshNode` worker
//! loops over the mock ether (the ADR-0005 harness); session-level halves are clock-free.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::swap::{LadderParams, SwapPhase};
use crate::swap_funding_verify::{
    ClaimObservation, FundingObservation, FundingVerifier, HtlcExpectation,
};
use crate::swap_session::SwapSession;
use crate::swap_signer::{sim_tx_id, MockSigner, SwapSigner};
use crate::swap_wire::{SwapLegId, SWAP_ID_LEN};
use crate::test_support::{new_initiator_signed, participant_fixtures, wait_until, SETTLE};

/// A funding-accepting verifier whose CLAIM observation the test mutates — the deterministic
/// stand-in for a chain whose view of our broadcast NIM claim evolves (mempool → buried), or
/// that cannot be consulted at all (the 429 world). Funding reads accept-all (the sim default),
/// exactly like `AcceptAllVerifier`; only the claim watch is under test here.
#[derive(Clone)]
struct ClaimChain {
    claim: Arc<Mutex<ClaimObservation>>,
    consulted: Arc<Mutex<Vec<[u8; 32]>>>,
}

impl ClaimChain {
    fn new(initial: ClaimObservation) -> Self {
        ClaimChain {
            claim: Arc::new(Mutex::new(initial)),
            consulted: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn set_claim(&self, obs: ClaimObservation) {
        *self.claim.lock().unwrap() = obs;
    }
    fn consulted_txs(&self) -> Vec<[u8; 32]> {
        self.consulted.lock().unwrap().clone()
    }
}

impl FundingVerifier for ClaimChain {
    fn observe(&self, _expect: &HtlcExpectation) -> FundingObservation {
        FundingObservation::Found {
            amount: u64::MAX,
            timeout: u64::MAX,
            confirmations: u32::MAX,
        }
    }
    fn observe_claim(&self, _leg: SwapLegId, tx_id: &[u8; 32]) -> ClaimObservation {
        self.consulted.lock().unwrap().push(*tx_id);
        *self.claim.lock().unwrap()
    }
}

/// A `MockSigner` whose CLAIM builds fail while `fail` is set — run 4's `claim_nim: head fetch
/// failed: rpc http 429`, switchable so the test controls exactly when the "RPC" recovers.
/// Funding builds always succeed (run 4's did).
#[derive(Clone)]
struct FlakyClaimSigner {
    fail: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
}

impl FlakyClaimSigner {
    fn new(failing: bool) -> Self {
        FlakyClaimSigner {
            fail: Arc::new(AtomicBool::new(failing)),
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn set_failing(&self, failing: bool) {
        self.fail.store(failing, Ordering::SeqCst);
    }
    fn claim_attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl SwapSigner for FlakyClaimSigner {
    fn build_funding(
        &self,
        ctx: &crate::swap_coordinator::SwapContext,
        leg: SwapLegId,
    ) -> Option<(Vec<u8>, [u8; 32])> {
        MockSigner.build_funding(ctx, leg)
    }
    fn build_claim(
        &self,
        ctx: &crate::swap_coordinator::SwapContext,
        secret: [u8; 32],
    ) -> Option<(Vec<u8>, [u8; 32])> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return None; // "claim_nim: head fetch failed: rpc http 429"
        }
        MockSigner.build_claim(ctx, secret)
    }
}

// --- the two run-4 regression tests (node-level, RED on main) --------------------------------------

#[test]
fn a_responder_whose_claim_signer_fails_transiently_retries_and_settles_only_on_chain_confirmation()
{
    // Run 4, replayed over the real node loops — but this time the 429 is survivable. The
    // responder reaches `Revealed`, its claim build fails (transiently), and it must STAY
    // un-Settled (on main it silently settled right here — RED) while the ticks re-attempt.
    // Once the signer recovers, a successful BROADCAST is still not settlement: the swap
    // settles only after the sim ledger reports the claim buried to the NIM depth.
    use crate::mock_radio::MeshHarness;

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let chain = ClaimChain::new(ClaimObservation::Unavailable);
    let signer = FlakyClaimSigner::new(true); // the 429 storm is on
    let session = SwapSession::new(bob_id, LadderParams::default())
        .with_funding_verifier(Box::new(chain.clone()));
    let bob = h.add_session_participant("bob", &[2], session, Box::new(signer.clone()));
    h.connect("alice", "bob");

    let (coordinator, propose) =
        new_initiator_signed(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    // The swap runs to the reveal; bob's claim build fails → he must HOLD at Revealed.
    // (RED on main: the old driver settled bob despite the failed claim, so `Revealed`
    // never appears in his mirror and this wait times out on `Settled`.)
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Revealed),
            SETTLE
        ),
        "bob must hold at Revealed while his claim cannot broadcast (main settles here)"
    );
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "alice (initiator) settles her side as before"
    );

    // Maintenance beats while the "RPC" stays down: the tick RE-ATTEMPTS the claim (the fix's
    // retry half) and the phase never moves — un-Settled, un-reaped, funds-path intact.
    let before = signer.claim_attempts();
    for _ in 0..3 {
        bob.poll_beacon();
        bob.fence();
    }
    assert!(
        signer.claim_attempts() > before,
        "the tick cadence must re-attempt the failed claim broadcast"
    );
    assert_eq!(
        bob.swap_phase(swap_id),
        Some(SwapPhase::Revealed),
        "a swap whose claim never broadcast must never read Settled"
    );

    // The RPC recovers: the claim broadcasts — but the chain still shows it unburied
    // (mempool). Broadcast alone is NOT settlement.
    chain.set_claim(ClaimObservation::Included { confirmations: 0 });
    signer.set_failing(false);
    bob.poll_beacon();
    bob.fence();
    assert_eq!(
        bob.swap_phase(swap_id),
        Some(SwapPhase::Revealed),
        "a broadcast-but-unconfirmed claim must not settle"
    );
    // The confirmation watch consulted the EXACT tx the signer broadcast.
    assert!(
        chain.consulted_txs().contains(&sim_tx_id(swap_id, 0xF3)),
        "the claim watch must consult the broadcast claim tx"
    );

    // The sim ledger buries the claim to depth → the next beat settles. Chain truth, at last.
    chain.set_claim(ClaimObservation::Included {
        confirmations: u32::MAX,
    });
    bob.poll_beacon();
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "the responder settles once — and only once — the claim is chain-confirmed"
    );
    h.shutdown();
}

#[test]
fn a_responder_whose_claim_never_lands_surfaces_an_honest_loss_at_the_timelock() {
    // Run 4's true fate, named honestly: the claim NEVER lands (429 forever), the timelock
    // forecloses, and the responder must surface `Lost` — never the silent `Settled` main
    // produced (RED there), and never a fictitious `Refunded` (its USDC left with `S`).
    use crate::mock_radio::MeshHarness;
    use crate::test_support::make_beacon_packet;

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let chain = ClaimChain::new(ClaimObservation::Unavailable); // the chain never answers
    let signer = FlakyClaimSigner::new(true); // ... and the claim never broadcasts
    let session = SwapSession::new(bob_id, LadderParams::default())
        .with_funding_verifier(Box::new(chain.clone()));
    let bob = h.add_session_participant("bob", &[2], session, Box::new(signer.clone()));
    h.connect("alice", "bob");

    let (coordinator, propose) =
        new_initiator_signed(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Revealed),
            SETTLE
        ),
        "bob must hold at Revealed while his claim cannot broadcast (main settles here)"
    );
    // The head passes T_A (10_000 in the fixtures): the initiator can now refund the NIM HTLC,
    // so bob's inbound claim window is CLOSED. The next maintenance tick forecloses honestly.
    bob.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([7; 8], 10_001, 5, 7, 1),
    );
    assert!(
        wait_until(|| bob.cached_head_height() == Some(10_001), SETTLE),
        "bob never cached the head beacon"
    );
    bob.poll_beacon();
    assert!(
        wait_until(|| bob.swap_phase(swap_id) == Some(SwapPhase::Lost), SETTLE),
        "an unconfirmed claim past the timelock must surface the honest Lost terminal"
    );
    assert!(
        signer.claim_attempts() > 0,
        "the loss must come after real claim attempts, not instead of them"
    );

    // The loss was SURFACED (one mirrored tick), then the terminal swap is reaped as usual.
    bob.poll_beacon();
    assert!(
        wait_until(|| bob.swap_phase(swap_id).is_none(), SETTLE),
        "the Lost terminal is reaped on the following tick"
    );
    h.shutdown();
}

// --- session-level halves (clock-free, ADR-0005) ---------------------------------------------------

/// A responder session driven to `Revealed` by real messages, wired to `chain`, with the swap's
/// deterministic sim claim tx already BROADCAST-recorded. Returns `(session, swap_id)`.
fn revealed_responder_with(chain: ClaimChain) -> (SwapSession, [u8; SWAP_ID_LEN]) {
    use crate::swap_wire::{encode_swap, SwapKind};
    let (swap_id, _alice_id, bob_id, alice_ctx) = participant_fixtures();
    let secret = [42u8; 32];
    let (_alice, propose) = new_initiator_signed(alice_ctx, secret, LadderParams::default());
    let mut bob =
        SwapSession::new(bob_id, LadderParams::default()).with_funding_verifier(Box::new(chain));
    bob.on_message(SwapKind::Propose, &encode_swap(&propose).unwrap(), 0)
        .unwrap();
    // The initiator's NIM funding proof arrives; the accept-all funding read advances bob, who
    // then funds his own leg; the reveal hands him S.
    let nim_fp =
        crate::swap_messages::tx_envelope(swap_id, SwapLegId::Nim, vec![0x11; 248], [0xC1; 32]);
    bob.on_message(SwapKind::FundingProof, &encode_swap(&nim_fp).unwrap(), 0)
        .unwrap();
    {
        let c = bob.coordinator(&swap_id).unwrap();
        c.fund(0, vec![0x22; 120], [0xF2; 32]).unwrap();
        let reveal = crate::swap_messages::tx_envelope(
            swap_id,
            SwapLegId::Counterparty,
            secret.to_vec(),
            [0xF3; 32],
        );
        c.recv_reveal(&reveal, secret).unwrap();
        assert_eq!(c.phase(), SwapPhase::Revealed);
        c.note_claim_broadcast(sim_tx_id(swap_id, 0xF3));
    }
    (bob, swap_id)
}

#[test]
fn confirm_claim_is_fail_closed_on_unavailable_rearms_on_notfound_and_settles_on_depth() {
    // The three-valued claim watch, exhaustively: Unavailable (the 429) changes NOTHING —
    // neither settle nor re-broadcast; NotFound clears the broadcast so the tick re-claims;
    // Included below the NIM depth (testnet 2) keeps waiting; Included at depth settles.
    let chain = ClaimChain::new(ClaimObservation::Unavailable);
    let (mut bob, swap_id) = revealed_responder_with(chain.clone());

    assert!(bob.confirm_pending_claims().is_empty()); // 429 → fail-closed
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Revealed
    );
    assert!(bob
        .coordinator(&swap_id)
        .unwrap()
        .claim_broadcast()
        .is_some());

    chain.set_claim(ClaimObservation::NotFound); // evicted/never landed → re-arm
    assert!(bob.confirm_pending_claims().is_empty());
    assert!(bob
        .coordinator(&swap_id)
        .unwrap()
        .claim_broadcast()
        .is_none());
    assert_eq!(bob.claims_awaiting_broadcast(), vec![swap_id]);

    bob.coordinator(&swap_id)
        .unwrap()
        .note_claim_broadcast(sim_tx_id(swap_id, 0xF3)); // the tick re-claimed
    chain.set_claim(ClaimObservation::Included { confirmations: 1 }); // 1 < the NIM depth (2)
    assert!(bob.confirm_pending_claims().is_empty());
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Revealed
    );

    chain.set_claim(ClaimObservation::Included { confirmations: 2 }); // buried to depth
    assert_eq!(bob.confirm_pending_claims(), vec![swap_id]);
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Settled
    );
}

#[test]
fn the_session_tick_never_fake_refunds_a_revealed_responder_and_forfeits_past_t_a() {
    // The GC tick's two run-4 hazards, pinned: (1) the blanket refund sweep must NOT flip a
    // Revealed responder to `Refunded` once T_B (5_000) passes — its USDC already left with the
    // public S; (2) past T_A (10_000) the unconfirmed claim forecloses to `Lost`, which stays
    // mirrored for one tick and is reaped on the next.
    let chain = ClaimChain::new(ClaimObservation::Unavailable);
    let (mut bob, swap_id) = revealed_responder_with(chain);

    bob.tick(6_000); // past T_B: the old sweep would have stamped this "Refunded" (no loss!)
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Revealed,
        "a Revealed responder has nothing to refund — its exit is claim or honest Lost"
    );

    bob.tick(10_001); // past T_A, claim still unconfirmed → the honest loss, surfaced
    assert_eq!(bob.coordinator(&swap_id).unwrap().phase(), SwapPhase::Lost);

    bob.tick(10_002); // ... and reaped as terminal on the following tick
    assert!(bob.coordinator(&swap_id).is_none());
}

#[test]
fn the_forfeiture_runs_a_last_chance_confirmation_so_a_buried_claim_settles_not_lost() {
    // The T_A boundary: the chain shows the claim buried in the very tick the head crosses T_A.
    // The tick's forfeiture must consult ONE more time and settle — an actually-paid responder
    // must never be mislabeled Lost.
    let chain = ClaimChain::new(ClaimObservation::Included {
        confirmations: u32::MAX,
    });
    let (mut bob, swap_id) = revealed_responder_with(chain);
    bob.tick(10_001);
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Settled
    );
}

#[test]
fn a_pending_claim_survives_snapshot_restore() {
    // The never-strand discipline across a restart: a responder that crashed AFTER broadcasting
    // its claim must come back WATCHING that exact tx — not blind-rebroadcasting, and above all
    // not forgetting a claim ever went out.
    let chain = ClaimChain::new(ClaimObservation::Unavailable);
    let (bob, swap_id) = revealed_responder_with(chain);
    let (_, _, bob_id, _) = participant_fixtures();
    let bytes = bob.encode_snapshot();
    drop(bob); // the crash

    let mut restored = SwapSession::restore_bytes(bob_id, LadderParams::default(), &bytes).unwrap();
    let c = restored.coordinator(&swap_id).unwrap();
    assert_eq!(c.phase(), SwapPhase::Revealed);
    assert_eq!(
        c.claim_broadcast(),
        Some(sim_tx_id(swap_id, 0xF3)),
        "the pending claim tx must ride the recovery bytes"
    );
    assert!(restored.has_claim_work());
}
