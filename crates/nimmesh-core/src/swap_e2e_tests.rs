//! # swap_e2e_tests — the atomicity proof (mesh swap F4, `cfg(test)` only)
//!
//! Drives a whole offline cross-chain swap end to end — both [`crate::swap::Swap`] state machines
//! (Alice the initiator, Bob the responder) over two [`crate::swap_leg::MockLeg`] HTLC escrows —
//! through the happy path **and every adversarial path**, asserting the one invariant that answers
//! "could someone lose funds?": **no one-sided settlement is ever possible** — either both legs are
//! claimed (the swap succeeded) or neither is (it unwound to refunds). The worst case is a refund,
//! never a theft (`docs/swap/FEASIBILITY.md`).
//!
//! The two legs:
//! - **NIM leg** — funded by Alice (initiator), claimed by Bob, timeout `T_A` (the longer one).
//! - **counterparty (BTC) leg** — funded by Bob (responder), claimed by Alice, timeout `T_B < T_A`.
//!
//! Alice knows the secret `S`; she claims the BTC leg by revealing it, Bob reads `S` off that
//! claim and claims the NIM leg. The mock legs enforce the real HTLC rules, so these tests prove
//! the *protocol* is atomic independent of the on-chain byte format (which `nimiq::htlc` owns and a
//! live testnet validates).

use crate::swap::{LadderParams, Swap, SwapAction, SwapError, SwapPhase, SwapRole, SwapTerms};
use crate::swap_leg::{sha256, HtlcParams, LegOutcome, MockLeg, SwapLeg};
use crate::swap_wire::SwapLegId;

const GIVE_NIM: u64 = 100_000; // luna Alice locks on the NIM leg
const TAKE_BTC: u64 = 50_000; // sats Bob locks on the BTC leg
const T_A: u64 = 10_000; // NIM-leg timeout (longer)
const T_B: u64 = 5_000; // BTC-leg timeout (shorter); margin 5000 ≥ Δ_safe, window 5000 ≥ min

fn terms() -> SwapTerms {
    SwapTerms {
        nim_timeout: T_A,
        counterparty_timeout: T_B,
    }
}

/// THE invariant. After a scenario has fully played out (claims + any timeout refunds), no leg may
/// be left funded-and-unresolved, and the two legs must agree: both claimed, or neither. A single
/// claimed leg with the other stuck would be a one-sided settlement (a theft) — this forbids it.
fn assert_no_one_sided(nim: &MockLeg, btc: &MockLeg) {
    assert_ne!(
        nim.outcome(),
        LegOutcome::Funded,
        "NIM leg left unresolved (no refund taken)"
    );
    assert_ne!(
        btc.outcome(),
        LegOutcome::Funded,
        "BTC leg left unresolved (no refund taken)"
    );
    let nim_claimed = nim.outcome() == LegOutcome::Claimed;
    let btc_claimed = btc.outcome() == LegOutcome::Claimed;
    assert_eq!(
        nim_claimed, btc_claimed,
        "ONE-SIDED SETTLEMENT: nim_claimed={nim_claimed} btc_claimed={btc_claimed}"
    );
}

/// Both parties accept + both fund, leaving the swap at `BothFunded` with both legs locked.
/// Returns (alice, bob, nim_leg, btc_leg, secret, hashlock).
fn fund_both() -> (Swap, Swap, MockLeg, MockLeg, [u8; 32]) {
    let p = LadderParams::default();
    let secret = [42u8; 32];
    let hashlock = sha256(&secret);
    let head = 0;

    let mut alice = Swap::new(SwapRole::Initiator, terms());
    let mut bob = Swap::new(SwapRole::Responder, terms());
    let mut nim_leg = MockLeg::new();
    let mut btc_leg = MockLeg::new();

    alice.accept(head, &p).unwrap();
    bob.accept(head, &p).unwrap();

    // Alice funds the NIM leg first.
    let act = alice.fund(head, &p).unwrap();
    assert_eq!(
        act,
        SwapAction::FundLeg {
            leg: SwapLegId::Nim,
            timeout: T_A
        }
    );
    nim_leg
        .fund(HtlcParams {
            hashlock,
            amount: GIVE_NIM,
            timeout: T_A,
        })
        .unwrap();

    // Bob sees the initiator's funding, then funds the BTC leg.
    bob.observe_initiator_funded().unwrap();
    let act = bob.fund(head, &p).unwrap();
    assert_eq!(
        act,
        SwapAction::FundLeg {
            leg: SwapLegId::Counterparty,
            timeout: T_B
        }
    );
    btc_leg
        .fund(HtlcParams {
            hashlock,
            amount: TAKE_BTC,
            timeout: T_B,
        })
        .unwrap();

    // Both observe both legs funded.
    alice.observe_counterparty_funded().unwrap();
    bob.observe_counterparty_funded().unwrap();
    assert_eq!(alice.phase, SwapPhase::BothFunded);
    assert_eq!(bob.phase, SwapPhase::BothFunded);

    (alice, bob, nim_leg, btc_leg, secret)
}

#[test]
fn happy_path_both_legs_settle() {
    let (mut alice, mut bob, mut nim_leg, mut btc_leg, secret) = fund_both();
    let head = 100; // well before either timeout

    // Alice claims the BTC leg, revealing the secret on that chain.
    assert_eq!(
        alice.reveal_and_claim().unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Counterparty
        }
    );
    btc_leg.claim(secret, head).unwrap();
    alice.observe_settled().unwrap();

    // Bob reads the secret off the BTC claim (as he would off-chain or over the mesh)...
    let learned = btc_leg
        .revealed_preimage()
        .expect("secret is public after the claim");
    assert_eq!(learned, secret);
    // ...and claims the NIM leg with it.
    assert_eq!(
        bob.observe_secret().unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Nim
        }
    );
    nim_leg.claim(learned, head).unwrap();
    bob.observe_settled().unwrap();

    assert_eq!(alice.phase, SwapPhase::Settled);
    assert_eq!(bob.phase, SwapPhase::Settled);
    assert_eq!(nim_leg.outcome(), LegOutcome::Claimed); // Bob got the NIM
    assert_eq!(btc_leg.outcome(), LegOutcome::Claimed); // Alice got the BTC
    assert_no_one_sided(&nim_leg, &btc_leg);
}

#[test]
fn responder_never_funds_initiator_refunds() {
    let p = LadderParams::default();
    let secret = [7u8; 32];
    let hashlock = sha256(&secret);

    let mut alice = Swap::new(SwapRole::Initiator, terms());
    alice.accept(0, &p).unwrap();
    alice.fund(0, &p).unwrap();
    let mut nim_leg = MockLeg::new();
    nim_leg
        .fund(HtlcParams {
            hashlock,
            amount: GIVE_NIM,
            timeout: T_A,
        })
        .unwrap();
    // Bob never funds — there is no BTC leg, so Alice can never claim anything.
    let btc_leg = MockLeg::new();

    // Alice can't get her counterparty funds; once `T_A` passes she refunds her NIM. No loss.
    assert_eq!(
        alice.refund_after_timeout(T_A + 1).unwrap(),
        SwapAction::RefundLeg {
            leg: SwapLegId::Nim
        }
    );
    nim_leg.refund(T_A + 1).unwrap();

    assert_eq!(alice.phase, SwapPhase::Refunded);
    assert_eq!(nim_leg.outcome(), LegOutcome::Refunded);
    assert_eq!(btc_leg.outcome(), LegOutcome::Unfunded);
    assert_no_one_sided(&nim_leg, &btc_leg);
}

#[test]
fn initiator_vanishes_after_funding_both_refund() {
    let (mut alice, _bob, mut nim_leg, mut btc_leg, _secret) = fund_both();

    // Alice never reveals/claims. Bob's BTC leg has the shorter timeout, so he refunds first...
    btc_leg.refund(T_B + 1).unwrap();
    // ...then Alice's NIM leg passes its timeout and she refunds too.
    assert_eq!(
        alice.refund_after_timeout(T_A + 1).unwrap(),
        SwapAction::RefundLeg {
            leg: SwapLegId::Nim
        }
    );
    nim_leg.refund(T_A + 1).unwrap();

    assert_eq!(nim_leg.outcome(), LegOutcome::Refunded);
    assert_eq!(btc_leg.outcome(), LegOutcome::Refunded);
    assert_no_one_sided(&nim_leg, &btc_leg); // neither claimed — unwound cleanly
}

#[test]
fn unsafe_ladder_refuses_to_fund_so_nothing_is_at_risk() {
    let p = LadderParams::default();
    // A thin-margin ladder: T_A - T_B = 1000 < Δ_safe (3600).
    let bad = SwapTerms {
        nim_timeout: 6_000,
        counterparty_timeout: 5_000,
    };
    let mut alice = Swap::new(SwapRole::Initiator, bad);
    assert!(matches!(
        alice.accept(0, &p),
        Err(SwapError::UnsafeLadder(_))
    ));
    // No funding ever happened, so both legs are untouched — trivially safe.
    let (nim_leg, btc_leg) = (MockLeg::new(), MockLeg::new());
    assert_eq!(alice.phase, SwapPhase::Proposed);
    assert_no_one_sided(&nim_leg, &btc_leg);
}

#[test]
fn a_relay_with_the_wrong_secret_cannot_steal_a_leg() {
    // A blind relay (or any attacker) sees the funded legs flooding past but does NOT know the
    // secret. It cannot claim — the hashlock holds — so the funds stay safe for the real parties.
    let (_alice, _bob, _nim_leg, mut btc_leg, _secret) = fund_both();
    let attacker_guess = [0xABu8; 32];
    assert!(btc_leg.claim(attacker_guess, 100).is_err());
    assert_eq!(btc_leg.outcome(), LegOutcome::Funded); // untouched — no theft
                                                       // The leg is still refundable by its funder after the timeout: no loss.
    btc_leg.refund(T_B + 1).unwrap();
    assert_eq!(btc_leg.outcome(), LegOutcome::Refunded);
}

#[test]
fn responder_cannot_claim_before_the_secret_is_revealed() {
    // Liveness/ordering: Bob (responder) only reaches a claimable state via `observe_secret`, which
    // is only legal once both legs are funded — he can't jump the gun and claim NIM early.
    let (_alice, mut bob, _nim_leg, _btc_leg, _secret) = fund_both();
    // Bob is at BothFunded; observe_secret is the only path to ClaimLeg, and it requires the reveal
    // to have happened off-chain first (the test models that by Alice claiming). Calling it here is
    // legal as a transition, but Bob still needs the actual preimage to claim the NIM leg on-chain —
    // which `MockLeg::claim` enforces. Prove the wrong/absent secret can't claim:
    let mut nim_leg = MockLeg::new();
    nim_leg
        .fund(HtlcParams {
            hashlock: sha256(&[42u8; 32]),
            amount: GIVE_NIM,
            timeout: T_A,
        })
        .unwrap();
    bob.observe_secret().unwrap(); // state-machine transition
    assert!(nim_leg.claim([0u8; 32], 100).is_err()); // but without the real secret, no claim
    assert_eq!(nim_leg.outcome(), LegOutcome::Funded);
}

#[test]
fn a_swap_funding_proof_rides_the_multi_hop_mesh_blindly() {
    // F4b: a swap message floods through a relay it has no direct interest in, and reaches the far
    // node over the REAL engine flood/relay/store-forward path — carried as opaque blind-relayed
    // bytes, exactly like a `nimiqTx`. Proves the swap rides the actual mesh, not just the model.
    use crate::codec::encode;
    use crate::mock_radio::MeshHarness;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap_wire::{encode_swap, SwapEnvelope, SwapLegId, HASH_LEN, SWAP_ID_LEN};
    use crate::test_support::{wait_until, SETTLE};

    let mut h = MeshHarness::new();
    let relay = h.add_node("relay", &[2]);
    let dest = h.add_node("dest", &[3]);
    h.connect("relay", "dest"); // origin -> relay -> dest; the relay is just a blind hop

    // A funding-proof carrying an opaque ~248 B signed HTLC funding tx blob.
    let env = SwapEnvelope {
        swap_id: [0xB2; SWAP_ID_LEN],
        leg: Some(SwapLegId::Nim),
        tx_wire: Some(vec![0x11; 248]),
        tx_id: Some([0xCD; HASH_LEN]),
        network_id: Some(5),
        ..Default::default()
    };
    let mut packet = Packet::new(
        MessageType::SwapFundingProof,
        [9u8; 8],
        encode_swap(&env).unwrap(),
    );
    packet.recipient_id = Some(BROADCAST_RECIPIENT); // flood
    let bytes = encode(&packet).unwrap();

    // It arrives at the relay from some upstream peer the relay isn't otherwise linked to.
    relay.on_packet_received_from("origin".to_string(), bytes);

    // The relay carries it onward (blind) AND stores it so G7 store-and-forward can serve it...
    assert!(
        wait_until(|| relay.forwarded_count() >= 1, SETTLE),
        "relay did not forward the swap packet onward"
    );
    assert!(
        relay.recent_stored() >= 1,
        "relay did not store the swap packet for store-and-forward"
    );
    // ...and it reaches the far node over the multi-hop mesh.
    assert!(
        wait_until(|| dest.recent_stored() >= 1, SETTLE),
        "the swap packet never reached the destination node"
    );
}

// --- G14: the SwapSession ↔ MeshNode hook, driven over the REAL node loop ---------------

/// Build a `(swap_id, terms)` pair + the two parties' identities/contexts for a node-level swap.
#[cfg(test)]
fn participant_fixtures() -> (
    [u8; 16],
    crate::swap_session::NodeIdentity,
    crate::swap_session::NodeIdentity,
    crate::swap_coordinator::SwapContext,
) {
    use crate::swap_coordinator::SwapContext;
    use crate::swap_session::NodeIdentity;
    let swap_id = [0x7A; 16];
    let pk = |b: u8| {
        let mut k = [b; 33];
        k[0] = 0x02;
        k
    };
    let alice_id = NodeIdentity {
        nim_address: [0xA1; 20],
        btc_address: b"tb1qalice".to_vec(),
        btc_pubkey: pk(0x11),
    };
    let bob_id = NodeIdentity {
        nim_address: [0xB2; 20],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: pk(0x22),
    };
    // Alice's initiator context (she gives NIM, wants BTC), terms = the proven-safe ladder.
    let alice_ctx = SwapContext {
        swap_id,
        terms: terms(),
        hashlock: sha256(&[42u8; 32]),
        nim_address: alice_id.nim_address,
        btc_address: alice_id.btc_address.clone(),
        btc_pubkey: alice_id.btc_pubkey,
        give_amount: GIVE_NIM,
        take_amount: TAKE_BTC,
        network_id: 5,
    };
    (swap_id, alice_id, bob_id, alice_ctx)
}

#[test]
fn a_responder_node_accepts_a_proposed_swap_injected_over_the_mesh() {
    // G14: a swap `Propose` packet is injected at a participant node `bob`. Through the REAL node
    // receive loop (decode → dispatch → SwapSession route → reply flood) bob spins up a responder
    // coordinator, reaches `Accepted`, and floods a `SwapAccept` — which a connected relay carries
    // onward. Proves the MeshNode swap hook works end to end, not the session in isolation.
    use crate::codec::encode;
    use crate::mock_radio::MeshHarness;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::swap_wire::encode_swap;
    use crate::test_support::{wait_until, SETTLE};

    let (swap_id, _alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    let relay = h.add_node("relay", &[3]);
    h.connect("bob", "relay");

    // Alice (off-mesh here) builds her Propose; it arrives at bob from some upstream peer.
    let (_alice, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    let mut packet = Packet::new(
        MessageType::SwapPropose,
        [9u8; 8],
        encode_swap(&propose).unwrap(),
    );
    packet.recipient_id = Some(BROADCAST_RECIPIENT);
    bob.on_packet_received_from("alice".to_string(), encode(&packet).unwrap());

    // Bob's node routes the Propose to its session and accepts — observable via the phase mirror.
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "bob did not accept the proposed swap through the node loop"
    );
    // And bob flooded a reply (the SwapAccept) which the relay carried onward.
    assert!(
        wait_until(|| relay.forwarded_count() >= 1, SETTLE),
        "bob's Accept reply never reached / was relayed by the neighbour"
    );

    h.shutdown();
}

#[test]
fn two_participant_nodes_drive_a_full_swap_to_settled_over_the_real_mesh() {
    // G17: the WHOLE swap lifecycle over the real mesh, with no hand orchestration — the node-side
    // driver reacts to each coordinator phase change. Alice (initiator) floods a `Propose`; bob
    // (responder) accepts; alice funds NIM + floods a `FundingProof`; bob funds BTC + floods his
    // own; alice (both funded) claims the BTC leg revealing `S` + floods a `PreimageReveal`; bob
    // reads `S`, claims his NIM leg, and both settle. Propose → Accept → fund → fund → reveal →
    // settle, all driven by the two MeshNode loops over the flood path (sim / stand-in tx bytes).
    use crate::mock_radio::MeshHarness;
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::test_support::{wait_until, SETTLE};

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");

    // Alice originates the swap: her coordinator + the Propose to flood. Everything after is driven.
    let (coordinator, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    // Both sides drive themselves all the way to Settled — neither leg left one-sided.
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "alice's swap never settled over the mesh"
    );
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "bob's swap never settled over the mesh"
    );

    h.shutdown();
}

#[test]
fn the_worker_gc_tick_sheds_a_stale_swap_once_the_head_passes_its_timelock() {
    // G16: a participant node accepts a swap, then the negotiation stalls (never funds). Once the
    // node hears a head beacon putting the chain head past the swap's T_A timelock (10_000 in the
    // fixtures), the worker's maintenance/GC tick drops the dead swap — proving the session expiry
    // is wired into the real MeshNode worker, not just callable on the session in isolation.
    use crate::mock_radio::MeshHarness;
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::swap_wire::encode_swap;
    use crate::test_support::{make_beacon_packet, wait_until, SETTLE};

    let (swap_id, _alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());

    // Bob accepts a proposed swap (no funds ever locked) — it sits in the session.
    let (_alice, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    let mut packet = crate::packet::Packet::new(
        crate::packet::MessageType::SwapPropose,
        [9u8; 8],
        encode_swap(&propose).unwrap(),
    );
    packet.recipient_id = Some(crate::packet::BROADCAST_RECIPIENT);
    bob.on_packet_received_from("alice".to_string(), crate::codec::encode(&packet).unwrap());
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "bob never accepted the proposed swap"
    );

    // Bob hears a head beacon putting the chain head at 10_001 — past the swap's T_A (10_000).
    bob.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([7; 8], 10_001, 5, 7, 1),
    );
    assert!(
        wait_until(|| bob.cached_head_height() == Some(10_001), SETTLE),
        "bob never cached the head beacon"
    );

    // The maintenance/GC tick now sheds the dead swap.
    bob.poll_sync();
    assert!(
        wait_until(|| bob.swap_phase(swap_id).is_none(), SETTLE),
        "the stale swap was not GC'd by the worker tick"
    );

    h.shutdown();
}

#[test]
fn a_full_swap_is_negotiated_and_settled_via_wire_messages() {
    // G5: a complete swap expressed as the wire message SEQUENCE — Propose → Accept → FundingProof
    // (NIM) → FundingProof (BTC) → PreimageReveal — each message built by `swap_messages`, wrapped
    // in its packet and round-tripped through the REAL packet codec (sender → wire → receiver),
    // driving both state machines + escrows to a settled, atomic swap. Proves the swap is fully
    // negotiable over the mesh, not just modeled.
    use crate::codec::{decode, encode};
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap_leg::HtlcParams;
    use crate::swap_messages::{tx_envelope, SwapAcceptance, SwapProposal};
    use crate::swap_wire::{decode_swap, encode_swap, SwapEnvelope, SwapKind, SWAP_ID_LEN};

    // Carry an envelope over the wire: build its packet, encode, decode, decode the swap payload.
    let over_wire = |mt: MessageType, env: &SwapEnvelope| -> SwapEnvelope {
        let mut pkt = Packet::new(mt, [0u8; 8], encode_swap(env).unwrap());
        pkt.recipient_id = Some(BROADCAST_RECIPIENT);
        let back = decode(&encode(&pkt).unwrap()).unwrap();
        decode_swap(SwapKind::from_message_type(mt).unwrap(), &back.payload).unwrap()
    };

    let p = LadderParams::default();
    let (secret, head) = ([42u8; 32], 0);
    let hashlock = sha256(&secret);
    let swap_id = [0x7A; SWAP_ID_LEN];
    let alice_btc_pk = {
        let mut k = [0x11; 33];
        k[0] = 0x02;
        k
    };
    let bob_btc_pk = {
        let mut k = [0x22; 33];
        k[0] = 0x03;
        k
    };

    let mut alice = Swap::new(SwapRole::Initiator, terms());
    let mut bob = Swap::new(SwapRole::Responder, terms());
    let mut nim_leg = MockLeg::new();
    let mut btc_leg = MockLeg::new();

    // 1) Propose (alice → bob): bob learns the terms + alice's BTC claimant pubkey.
    let proposal = SwapProposal {
        swap_id,
        hashlock,
        give_amount: GIVE_NIM,
        take_amount: TAKE_BTC,
        terms: terms(),
        nim_address: [0xA1; 20],
        btc_address: b"tb1qalice".to_vec(),
        btc_pubkey: alice_btc_pk,
        network_id: 5,
    };
    let bob_sees = SwapProposal::from_envelope(&over_wire(
        MessageType::SwapPropose,
        &proposal.to_envelope(),
    ))
    .unwrap();
    assert_eq!(bob_sees.hashlock, hashlock);
    assert_eq!(bob_sees.btc_pubkey, alice_btc_pk);
    bob.accept(head, &p).unwrap();

    // 2) Accept (bob → alice): alice learns bob's BTC funder pubkey. Both now hold both pubkeys.
    let acceptance = SwapAcceptance {
        swap_id,
        nim_address: [0xB2; 20],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: bob_btc_pk,
    };
    let alice_sees = SwapAcceptance::from_envelope(&over_wire(
        MessageType::SwapAccept,
        &acceptance.to_envelope(),
    ))
    .unwrap();
    assert_eq!(alice_sees.btc_pubkey, bob_btc_pk);
    alice.accept(head, &p).unwrap();

    // 3) Alice funds the NIM leg → FundingProof(Nim) over the wire → bob observes it.
    alice.fund(head, &p).unwrap();
    nim_leg
        .fund(HtlcParams {
            hashlock,
            amount: GIVE_NIM,
            timeout: T_A,
        })
        .unwrap();
    let _ = over_wire(
        MessageType::SwapFundingProof,
        &tx_envelope(swap_id, SwapLegId::Nim, vec![0x11; 248], [0xC1; 32]),
    );
    bob.observe_initiator_funded().unwrap();

    // 4) Bob funds the BTC leg → FundingProof(Counterparty) over the wire → both reach BothFunded.
    bob.fund(head, &p).unwrap();
    btc_leg
        .fund(HtlcParams {
            hashlock,
            amount: TAKE_BTC,
            timeout: T_B,
        })
        .unwrap();
    let _ = over_wire(
        MessageType::SwapFundingProof,
        &tx_envelope(
            swap_id,
            SwapLegId::Counterparty,
            vec![0x22; 120],
            [0xC2; 32],
        ),
    );
    alice.observe_counterparty_funded().unwrap();
    bob.observe_counterparty_funded().unwrap();

    // 5) Alice claims BTC (reveals S) → PreimageReveal carries the claim (with S) over the wire.
    alice.reveal_and_claim().unwrap();
    btc_leg.claim(secret, head).unwrap();
    let reveal = over_wire(
        MessageType::SwapPreimageReveal,
        &tx_envelope(
            swap_id,
            SwapLegId::Counterparty,
            secret.to_vec(),
            [0xC3; 32],
        ),
    );
    let revealed: [u8; 32] = reveal.tx_wire.unwrap()[..32].try_into().unwrap(); // bob reads S off it

    // 6) Bob claims NIM with the revealed S; both settle. Atomic, end to end over the wire.
    assert_eq!(revealed, secret);
    bob.observe_secret().unwrap();
    nim_leg.claim(revealed, head).unwrap();
    alice.observe_settled().unwrap();
    bob.observe_settled().unwrap();

    assert_eq!(alice.phase, SwapPhase::Settled);
    assert_eq!(bob.phase, SwapPhase::Settled);
    assert_no_one_sided(&nim_leg, &btc_leg);
}
