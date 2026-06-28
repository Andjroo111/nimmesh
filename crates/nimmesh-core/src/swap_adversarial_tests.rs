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
