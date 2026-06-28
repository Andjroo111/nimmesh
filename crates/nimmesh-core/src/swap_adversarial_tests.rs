//! # swap_adversarial_tests — a swap survives a hostile relay (G27, cfg(test))
//!
//! Raises the protocol-level "a relay with the wrong secret cannot steal a leg" (see
//! [`crate::swap_e2e_tests`]) to the real [`crate::node::MeshNode`] loop: a relay that carries every
//! flooded swap packet still cannot (a) learn the swap or extract `S` — it holds no `SwapSession`, so
//! the participant path is skipped and it never reaches a phase; (b) forge a settlement — a reveal
//! with the wrong secret is rejected by the responder's coordinator (`sha256(S') != hashlock`); (c)
//! force a one-sided loss by selectively dropping — that is the lossy / mid-swap-partition case,
//! already proven to recover (G20/G24), the worst case being a refund. Fixtures come from
//! [`crate::test_support`].

use crate::test_support::participant_fixtures;

#[test]
fn a_blind_relay_carries_a_swap_without_learning_it() {
    // G27(a): the swap runs alice — relay — bob with the relay (a plain non-participant) carrying
    // every packet, yet it never learns the swap. A relay holds no SwapSession, so the participant
    // path is skipped entirely: it cannot decode any swap_id off the opaque stream, cannot extract S,
    // and never reaches a phase. Selective dropping by such a relay is the lossy / mid-swap-partition
    // case, already proven to recover (G20/G24), never to lose.
    use crate::mock_radio::MeshHarness;
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::test_support::{wait_until, SETTLE};

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let relay = h.add_node("relay", &[9]);
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "relay");
    h.connect("relay", "bob");

    let (coordinator, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "the swap did not complete through the relay"
    );
    assert_eq!(alice.swap_phase(swap_id), Some(SwapPhase::Settled));
    // The relay carried the whole stream...
    assert!(
        relay.forwarded_count() >= 1,
        "the relay should have carried the swap packets"
    );
    // ...yet never learned the swap: no session → no coordinator → no phase → it never saw S.
    assert!(
        relay.swap_phase(swap_id).is_none(),
        "a blind relay must never reach a swap phase"
    );

    h.shutdown();
}

#[test]
fn a_forged_reveal_with_the_wrong_secret_cannot_settle_a_participant() {
    // G27(b): a relay/attacker that sees the stream cannot forge a settlement. We drive bob (a
    // responder) to BothFunded with his BTC leg locked, then inject a forged PreimageReveal carrying
    // the WRONG secret for the live swap_id. His coordinator rejects it (sha256(S') != hashlock) so he
    // does NOT settle — his funds stay locked for the timeout refund. Only the real S settles him.
    use crate::codec::encode;
    use crate::mock_radio::MeshHarness;
    use crate::node::MeshNode;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::swap_messages::tx_envelope;
    use crate::swap_wire::{encode_swap, SwapEnvelope, SwapLegId};
    use crate::test_support::{wait_until, SETTLE};

    // `ts` makes each injected packet's relay-key distinct (key = type+sender+timestamp), so the
    // legit reveal is not deduped against the forged one.
    let inject = |node: &MeshNode, mt: MessageType, ts: u64, env: &SwapEnvelope| {
        let mut pkt = Packet::new(mt, [9u8; 8], encode_swap(env).unwrap());
        pkt.recipient_id = Some(BROADCAST_RECIPIENT);
        pkt.timestamp_ms = ts;
        node.on_packet_received_from("attacker".to_string(), encode(&pkt).unwrap());
    };

    let (swap_id, _alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());

    // Drive bob to BothFunded: Propose (he accepts), then a NIM FundingProof (he funds his BTC leg).
    let (_a, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    inject(&bob, MessageType::SwapPropose, 1, &propose);
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "bob never accepted"
    );
    inject(
        &bob,
        MessageType::SwapFundingProof,
        2,
        &tx_envelope(swap_id, SwapLegId::Nim, vec![0x11; 248], [0xC1; 32]),
    );
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::BothFunded),
            SETTLE
        ),
        "bob never funded his leg"
    );

    // Forged reveal, WRONG secret → rejected; bob stays BothFunded (no false settlement). The guard
    // is on S (it can never advance him), so this holds whether or not the worker has processed it.
    inject(
        &bob,
        MessageType::SwapPreimageReveal,
        3,
        &tx_envelope(swap_id, SwapLegId::Counterparty, vec![0x99; 32], [0xC9; 32]),
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        bob.swap_phase(swap_id),
        Some(SwapPhase::BothFunded),
        "a forged reveal with the wrong secret must not settle bob"
    );

    // The real secret DOES settle him — the guard is on S, not on liveness.
    inject(
        &bob,
        MessageType::SwapPreimageReveal,
        4,
        &tx_envelope(
            swap_id,
            SwapLegId::Counterparty,
            [42u8; 32].to_vec(),
            [0xCA; 32],
        ),
    );
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "the real secret must settle bob"
    );

    h.shutdown();
}

// --- G30: concurrent-swap cap (anti-DoS) ----------------------------------------------------------

/// A fair-rate `Propose` envelope (3.0 NIM/BTC) for `swap_id` — accepted by any non-strict policy.
fn fair_propose(swap_id: [u8; 16]) -> crate::swap_wire::SwapEnvelope {
    let mut pk = [0x11; 33];
    pk[0] = 0x02;
    crate::swap_messages::SwapProposal {
        swap_id,
        hashlock: crate::swap_leg::sha256(&[42u8; 32]),
        give_amount: 150_000,
        take_amount: 50_000,
        terms: crate::swap::SwapTerms {
            nim_timeout: 10_000,
            counterparty_timeout: 5_000,
        },
        nim_address: [0xA1; 20],
        btc_address: b"tb1qalice".to_vec(),
        btc_pubkey: pk,
        network_id: 5,
    }
    .to_envelope()
}

/// Bob's identity with an accept-all rate and a `cap` on concurrent swaps.
fn bob_identity(cap: usize) -> crate::swap_session::NodeIdentity {
    let mut pk = [0x22; 33];
    pk[0] = 0x02;
    crate::swap_session::NodeIdentity {
        nim_address: [0xB2; 20],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: pk,
        rate_policy: crate::swap_session::RatePolicy::accept_all(),
        max_concurrent_swaps: cap,
        standing_intent: None,
    }
}

#[test]
fn the_session_caps_concurrent_swaps_and_a_freed_slot_reopens() {
    use crate::swap::LadderParams;
    use crate::swap_session::SwapSession;
    use crate::swap_wire::{encode_swap, SwapKind};

    let mut bob = SwapSession::new(bob_identity(2), LadderParams::default());
    let propose = |id| encode_swap(&fair_propose(id)).unwrap();

    // Two Proposes fill the cap (each returns one Accept).
    for id in [[0x01; 16], [0x02; 16]] {
        let out = bob.on_message(SwapKind::Propose, &propose(id), 0).unwrap();
        assert_eq!(out.len(), 1);
    }
    assert_eq!(bob.len(), 2);

    // A third is dropped at the cap: no Accept, no coordinator.
    assert!(bob
        .on_message(SwapKind::Propose, &propose([0x03; 16]), 0)
        .unwrap()
        .is_empty());
    assert_eq!(bob.len(), 2);
    assert!(bob.coordinator(&[0x03; 16]).is_none());

    // Freeing a slot (aborting an un-funded swap) lets a later Propose in again.
    bob.on_message(
        SwapKind::Abort,
        &encode_swap(&crate::swap_messages::abort([0x01; 16], 0)).unwrap(),
        0,
    )
    .unwrap();
    assert_eq!(bob.len(), 1);
    assert_eq!(
        bob.on_message(SwapKind::Propose, &propose([0x04; 16]), 0)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(bob.len(), 2);
}

#[test]
fn a_participant_caps_concurrent_swaps_over_the_mesh() {
    // G30: bob holds at most 2 swaps. Three Proposes flood in: the first two are accepted, the third
    // is dropped (no coordinator). Aborting one frees a slot, so a later Propose is accepted again.
    use crate::codec::encode;
    use crate::mock_radio::MeshHarness;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_wire::{encode_swap, SwapEnvelope};
    use crate::test_support::{wait_until, SETTLE};

    let mut h = MeshHarness::new();
    let bob = h.add_participant("bob", &[2], bob_identity(2), LadderParams::default());

    let inject = |mt: MessageType, env: &SwapEnvelope, ts: u64| {
        let mut pkt = Packet::new(mt, [9u8; 8], encode_swap(env).unwrap());
        pkt.recipient_id = Some(BROADCAST_RECIPIENT);
        pkt.timestamp_ms = ts;
        bob.on_packet_received_from("spammer".to_string(), encode(&pkt).unwrap());
    };

    inject(MessageType::SwapPropose, &fair_propose([0x01; 16]), 1);
    inject(MessageType::SwapPropose, &fair_propose([0x02; 16]), 2);
    assert!(
        wait_until(
            || bob.swap_phase([0x01; 16]) == Some(SwapPhase::Accepted)
                && bob.swap_phase([0x02; 16]) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "the first two proposals should be accepted"
    );

    // A third, past the cap, is dropped (the cap never creates a coordinator, so this is stable).
    inject(MessageType::SwapPropose, &fair_propose([0x03; 16]), 3);
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        bob.swap_phase([0x03; 16]).is_none(),
        "the third proposal must be dropped at the cap"
    );

    // Abort frees a slot, so a later proposal is accepted again.
    inject(
        MessageType::SwapAbort,
        &crate::swap_messages::abort([0x01; 16], 0),
        4,
    );
    assert!(
        wait_until(|| bob.swap_phase([0x01; 16]).is_none(), SETTLE),
        "the aborted swap should clear"
    );
    inject(MessageType::SwapPropose, &fair_propose([0x04; 16]), 5);
    assert!(
        wait_until(
            || bob.swap_phase([0x04; 16]) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "a slot freed by the abort lets a later proposal in"
    );

    h.shutdown();
}

// --- G33: node-level crash recovery -------------------------------------------------------------

#[test]
fn a_node_restored_from_a_snapshot_still_refunds_a_funds_locked_swap() {
    // G33: a participant funds its NIM leg, its live session is snapshotted to bytes, and a NEW node
    // is built restored from those bytes (the original "crashes"). The restored node's worker refund
    // tick still fires past the timeout — G31/G32 proven over the real MeshNode loop.
    use crate::codec::encode;
    use crate::mock_radio::MeshHarness;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::swap_messages::SwapAcceptance;
    use crate::swap_wire::encode_swap;
    use crate::test_support::{make_beacon_packet, wait_until, SETTLE};

    let (swap_id, alice_id, _bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id.clone(), LadderParams::default());

    // Alice originates + an Accept arrives → she funds her NIM leg → SelfFunded (funds locked).
    let (coordinator, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);
    let accept = SwapAcceptance {
        swap_id,
        nim_address: [0xB2; 20],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: {
            let mut k = [0x22; 33];
            k[0] = 0x03;
            k
        },
    }
    .to_envelope();
    let mut pkt = Packet::new(
        MessageType::SwapAccept,
        [9u8; 8],
        encode_swap(&accept).unwrap(),
    );
    pkt.recipient_id = Some(BROADCAST_RECIPIENT);
    alice.on_packet_received_from("bob".to_string(), encode(&pkt).unwrap());
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::SelfFunded),
            SETTLE
        ),
        "alice never funded her leg"
    );

    // Snapshot alice's live session to the on-disk recovery blob.
    let snapshot = alice.swap_snapshot();
    assert!(
        snapshot.len() > 2,
        "the snapshot should hold the funds-locked swap"
    );

    // "Restart": a NEW node restored from the snapshot bytes; the funds-locked swap comes back.
    let revived =
        h.add_participant_restored("revived", &[3], alice_id, LadderParams::default(), snapshot);
    revived.poll_sync(); // populate the restored node's phase mirror
    assert!(
        wait_until(
            || revived.swap_phase(swap_id) == Some(SwapPhase::SelfFunded),
            SETTLE
        ),
        "the restored node did not recover the funds-locked swap"
    );

    // It hears a beacon past T_A and its worker refund tick fires → Refunded + reaped.
    revived.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([7; 8], 10_001, 5, 7, 1),
    );
    assert!(
        wait_until(|| revived.cached_head_height() == Some(10_001), SETTLE),
        "the restored node never cached the head beacon"
    );
    revived.poll_sync();
    assert!(
        wait_until(|| revived.swap_phase(swap_id).is_none(), SETTLE),
        "the restored node's worker refund tick did not fire"
    );

    h.shutdown();
}
