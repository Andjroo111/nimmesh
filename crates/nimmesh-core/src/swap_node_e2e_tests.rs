//! # swap_node_e2e_tests — whole swaps driven over the REAL MeshNode loop (G14–G29, cfg(test))
//!
//! Where [`crate::swap_e2e_tests`] proves the swap *protocol* (state machines + escrows + the wire
//! codec), this drives complete swaps through two or more [`crate::node::MeshNode`] worker loops over
//! the [`crate::mock_radio`] virtual mesh — exercising the integration the agenda built up: the
//! participant hook (G14), the full lifecycle driver (G17), the GC/expiry tick (G16), the node-level
//! refund safety exit (G18), abort teardown (G19), and pending-action retransmit resilience over a
//! lossy mesh (G20). Each test wires a `MeshHarness`, originates/injects swap packets, and asserts on
//! the observable per-swap phase mirror — proving the behaviour end to end, not the session in
//! isolation.

use crate::test_support::{participant_fixtures, terms, GIVE_NIM, TAKE_BTC};

// The swap participant fixtures (`participant_fixtures` / `terms` / `GIVE_NIM` / `TAKE_BTC`) live in
// `test_support` so this suite and the adversarial suite share them and each file stays under the
// 800-line ceiling.

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
fn a_stalled_funds_locked_swap_refunds_itself_via_the_worker_tick() {
    // G18: the safety exit over the mesh. Alice funds her NIM leg, but the counterparty never funds
    // (vanished). Once alice hears a head beacon past her T_A timeout, the worker refund/GC tick
    // refunds her leg (sim) and reaps the swap — proving "worst case is a refund" at the node level,
    // not just in the state-machine model.
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
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    // Alice originates the swap, then an Accept arrives from an off-mesh counterparty → her driver
    // funds the NIM leg → SelfFunded (funds locked).
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

    // The counterparty never funds. Alice hears a head beacon past her T_A timeout (10_000).
    alice.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([7; 8], 10_001, 5, 7, 1),
    );
    assert!(
        wait_until(|| alice.cached_head_height() == Some(10_001), SETTLE),
        "alice never cached the head beacon"
    );

    // The worker refund/GC tick refunds her stalled leg and reaps the swap.
    alice.poll_sync();
    assert!(
        wait_until(|| alice.swap_phase(swap_id).is_none(), SETTLE),
        "the stalled funds-locked swap was not refunded + GC'd by the worker tick"
    );

    h.shutdown();
}

#[test]
fn a_gc_abort_tears_down_the_swap_on_the_counterparty_over_the_mesh() {
    // G19: symmetric teardown. Two participants accept the same proposal (un-funded) but the phantom
    // initiator never funds. When bob's GC drops his stale swap, he floods a SwapAbort — and carol,
    // who never even heard the head beacon, frees her slot too. An abort from one participant clears
    // the swap on the other over the real mesh.
    use crate::codec::encode;
    use crate::mock_radio::MeshHarness;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::swap_session::NodeIdentity;
    use crate::swap_wire::{encode_swap, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN};
    use crate::test_support::{make_beacon_packet, wait_until, SETTLE};

    let (swap_id, _alice_id, bob_id, alice_ctx) = participant_fixtures();
    let carol_id = NodeIdentity {
        nim_address: [0xC3; NIM_ADDRESS_LEN],
        btc_address: b"tb1qcarol".to_vec(),
        btc_pubkey: {
            let mut k = [0x33; BTC_PUBKEY_LEN];
            k[0] = 0x02;
            k
        },
        rate_policy: crate::swap_session::RatePolicy::accept_all(),
        max_concurrent_swaps: crate::swap_session::DEFAULT_MAX_CONCURRENT_SWAPS,
    };
    let mut h = MeshHarness::new();
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    let carol = h.add_participant("carol", &[3], carol_id, LadderParams::default());
    h.connect("bob", "carol");

    // A Propose injected at bob is accepted by bob AND relayed to carol, who also accepts — two
    // responders to a phantom initiator that never funds. Both sit at Accepted (un-funded).
    let (_a, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    let mut pkt = Packet::new(
        MessageType::SwapPropose,
        [9u8; 8],
        encode_swap(&propose).unwrap(),
    );
    pkt.recipient_id = Some(BROADCAST_RECIPIENT);
    bob.on_packet_received_from("alice".to_string(), encode(&pkt).unwrap());
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "bob never accepted"
    );
    assert!(
        wait_until(
            || carol.swap_phase(swap_id) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "carol never accepted the relayed propose"
    );

    // Bob hears a head beacon past T_A → his GC drops the stale swap AND floods a teardown Abort.
    bob.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([7; 8], 10_001, 5, 7, 1),
    );
    assert!(
        wait_until(|| bob.cached_head_height() == Some(10_001), SETTLE),
        "bob never cached the beacon"
    );
    bob.poll_sync();

    // Bob's swap is GC'd, and his Abort clears carol's swap too — symmetric teardown over the mesh.
    assert!(
        wait_until(|| bob.swap_phase(swap_id).is_none(), SETTLE),
        "bob's stale swap was not GC'd"
    );
    assert!(
        wait_until(|| carol.swap_phase(swap_id).is_none(), SETTLE),
        "carol's swap was not torn down by bob's abort"
    );

    h.shutdown();
}

#[test]
fn a_swap_survives_a_lossy_mesh_via_retransmits() {
    // G20: over a mesh that drops a third of every flood, a swap still completes — each side
    // re-floods its last action on the maintenance tick until every message (Propose, Accept, both
    // FundingProofs, and crucially the PreimageReveal that carries S) gets through. Without the
    // retransmit a single dropped message would strand the swap. The ether's PRNG is deterministic,
    // so this is reproducible, not flaky.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_coordinator::SwapCoordinator;
    use std::time::Duration;

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");
    h.ether().set_loss(0.3); // a third of every flood is dropped

    let (coordinator, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    // Drive the maintenance tick at a calm cadence (a delay between ticks lets each node's worker
    // drain its queue), re-flooding each side's last action until every message gets through. With
    // no head beacon the head stays 0, so a coordinator can ONLY leave the session by Settled → reap
    // (never stale/refund) — so both sides going empty proves both settled (each claimed its leg),
    // the reveal having survived the loss via retransmit.
    let mut done = false;
    for _ in 0..150 {
        alice.poll_sync();
        bob.poll_sync();
        std::thread::sleep(Duration::from_millis(20));
        if alice.swap_phase(swap_id).is_none() && bob.swap_phase(swap_id).is_none() {
            done = true;
            break;
        }
    }
    assert!(
        done,
        "the swap did not settle on both sides over the lossy mesh"
    );

    h.shutdown();
}

#[test]
fn many_concurrent_swaps_all_settle_over_a_lossy_mesh() {
    // G21: N independent swaps run at once between the same two participants over a mesh that drops
    // a fifth of every flood and delays the rest — stressing the session's per-swap_id routing and
    // the G20 retransmit under concurrency. Each swap has its own id + secret. With no head beacon
    // the head stays 0, so a coordinator can ONLY leave the session by Settled → reap (never stale /
    // refund) — so EVERY swap going empty on BOTH sides proves every swap settled with no one-sided
    // leg left stuck. A swap that wedged half-done (e.g. a responder that never got its reveal)
    // would linger forever and fail the assertion.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_coordinator::{SwapContext, SwapCoordinator};
    use crate::swap_leg::sha256;
    use crate::swap_wire::SWAP_ID_LEN;
    use std::time::Duration;

    const N: usize = 6;

    let (_sid0, alice_id, bob_id, _ctx0) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id.clone(), LadderParams::default());
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");
    h.ether().set_loss(0.2);
    h.ether().set_latency(Duration::from_micros(100));

    // Start N independent swaps, each with a distinct swap_id and secret.
    let swap_ids: Vec<[u8; SWAP_ID_LEN]> = (0..N).map(|i| [0xC0 + i as u8; SWAP_ID_LEN]).collect();
    for (i, sid) in swap_ids.iter().enumerate() {
        let secret = [i as u8 + 1; 32];
        let ctx = SwapContext {
            swap_id: *sid,
            terms: terms(),
            hashlock: sha256(&secret),
            nim_address: alice_id.nim_address,
            btc_address: alice_id.btc_address.clone(),
            btc_pubkey: alice_id.btc_pubkey,
            give_amount: GIVE_NIM,
            take_amount: TAKE_BTC,
            network_id: 5,
        };
        let (coord, propose) = SwapCoordinator::new_initiator(ctx, secret, LadderParams::default());
        alice.start_swap(*sid, coord, propose);
    }

    // Drive the maintenance tick at a calm cadence until ALL swaps have settled + reaped on both
    // sides. Retransmit carries every swap's messages through the loss independently.
    let mut done = false;
    for _ in 0..400 {
        alice.poll_sync();
        bob.poll_sync();
        std::thread::sleep(Duration::from_millis(20));
        if swap_ids
            .iter()
            .all(|sid| alice.swap_phase(*sid).is_none() && bob.swap_phase(*sid).is_none())
        {
            done = true;
            break;
        }
    }
    assert!(
        done,
        "not all {N} concurrent swaps settled on both sides over the lossy mesh"
    );

    h.shutdown();
}

#[test]
fn a_rejoining_participant_catches_up_a_swap_via_store_and_forward() {
    // G22: a swap Propose floods while bob is not yet on the mesh; a relay stores it (G7
    // store-and-forward). When bob joins late and issues a gossip-sync (requestSync), the relay
    // serves him the missed Propose, he accepts, and the whole swap drives to completion over the
    // relay — proving the swap inherits the mesh's offline resilience. No node is ever polled, so
    // the G20 retransmit plays no part: bob's ONLY path to the Propose (which flooded before he
    // existed) is the gossip-sync reply, so his settling proves the store-and-forward catch-up.
    use crate::mock_radio::MeshHarness;
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::test_support::{wait_until, SETTLE};

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let relay = h.add_node("relay", &[9]);
    h.connect("alice", "relay");

    // Alice proposes while bob is absent; the relay stores the Propose for later store-and-forward.
    let (coordinator, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);
    assert!(
        wait_until(|| relay.recent_stored() >= 1, SETTLE),
        "the relay never stored the swap Propose for store-and-forward"
    );

    // Bob joins late and gossip-syncs: the relay serves him the missed Propose, he accepts, and the
    // swap drives end to end over the two-hop relay (no loss, no polling → a pure catch-up). Both
    // settle and — with no maintenance tick to reap them — stay Settled for the assertion.
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("bob", "relay");
    bob.request_sync();

    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "bob did not catch up + settle the swap via store-and-forward"
    );
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "alice's side did not settle after bob's catch-up"
    );

    h.shutdown();
}

#[test]
fn a_full_swap_rides_a_multi_hop_relay_line_end_to_end() {
    // G23: alice and bob are separated by FOUR blind relays in a line —
    // alice — r1 — r2 — r3 — r4 — bob — so every swap message rides a 5-hop path through nodes with
    // no stake in the swap. In a sparse line every relay forwards deterministically (degree 2 < the
    // high-degree threshold), and 5 hops sits inside the 7-hop TTL budget (a message arrives at the
    // far end with TTL 3), so the whole swap drives to Settled on both ends with no loss and no
    // polling. Proves swaps ride a DEEP mesh, not just a direct link or a single relay.
    use crate::mock_radio::MeshHarness;
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::test_support::{wait_until, SETTLE};

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    for (i, name) in ["r1", "r2", "r3", "r4"].iter().enumerate() {
        h.add_node(name, &[11 + i as u8]);
    }
    h.connect("alice", "r1");
    h.connect("r1", "r2");
    h.connect("r2", "r3");
    h.connect("r3", "r4");
    h.connect("r4", "bob");

    let (coordinator, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    // The swap drives end to end across the line; with no maintenance tick to reap them, both ends
    // stay Settled for the assertion. Neither leg is left one-sided.
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "bob never settled the swap across the relay line"
    );
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "alice never settled the swap across the relay line"
    );

    h.shutdown();
}

#[test]
fn a_swap_partitioned_mid_flight_recovers_after_the_heal() {
    // G24: a transient outage that strikes DURING the swap. We pace the start with per-hop latency so
    // we can catch alice in a stable funds-locked state (SelfFunded — her NIM leg funded), then cut
    // the alice↔bob link. While partitioned the swap is stuck mid-flight and — crucially — alice's
    // funded leg does NOT refund (the head stays 0, so no timeout fires). After the heal the G20
    // retransmit carries the stalled messages through and the swap completes on both sides. Proves
    // resilience to an outage during the swap, not just before it.
    use crate::mock_radio::MeshHarness;
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::test_support::{wait_until, SETTLE};
    use std::time::Duration;

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");
    // Pace the opening so SelfFunded is a wide, catchable window (~100ms) before bob's funding proof
    // would advance alice further.
    h.ether().set_latency(Duration::from_millis(50));

    let (coordinator, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    // Catch alice with her NIM leg funded (mid-flight), then cut the link and drop the pacing.
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::SelfFunded),
            SETTLE
        ),
        "alice never funded her leg (the swap never reached the mid-flight state)"
    );
    h.ether().partition("alice", "bob");
    h.ether().set_latency(Duration::ZERO);

    // While partitioned: drive the tick; the swap stays stuck and alice's funded leg stays LOCKED —
    // it must not refund (head is 0, so no timeout has passed). Bob can make no progress.
    for _ in 0..15 {
        alice.poll_sync();
        bob.poll_sync();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        alice
            .swap_phase(swap_id)
            .is_some_and(|p| p.has_funds_locked()),
        "alice's funded leg should stay locked across the partition (no premature refund, no settle)"
    );

    // Heal: the retransmit carries the stalled messages through and the swap completes both sides.
    h.ether().heal("alice", "bob");
    let mut done = false;
    for _ in 0..200 {
        alice.poll_sync();
        bob.poll_sync();
        std::thread::sleep(Duration::from_millis(20));
        if alice.swap_phase(swap_id).is_none() && bob.swap_phase(swap_id).is_none() {
            done = true;
            break;
        }
    }
    assert!(
        done,
        "the swap did not recover + settle on both sides after the partition healed"
    );

    h.shutdown();
}

#[test]
fn a_full_swap_drives_through_a_pluggable_signer() {
    // G26: the signer seam is pluggable. Alice is wired with a DIFFERENT SwapSigner (other stand-in
    // funding bytes; the claim still carries S in its first 32 bytes), and a whole swap still drives
    // to Settled on both sides. Proves the real NIM/BTC money-path signer drops in through the same
    // trait without touching the driver or the protocol. (Funding blobs stay <= 255 B: the wire TLV
    // value length is one byte.)
    use crate::mock_radio::MeshHarness;
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_coordinator::SwapCoordinator;
    use crate::swap_signer::SwapSigner;
    use crate::swap_wire::{SwapLegId, SWAP_ID_LEN};
    use crate::test_support::{wait_until, SETTLE};

    struct PaddedSigner;
    impl SwapSigner for PaddedSigner {
        fn build_funding(
            &self,
            leg: SwapLegId,
            _swap_id: [u8; SWAP_ID_LEN],
        ) -> (Vec<u8>, [u8; 32]) {
            let wire = match leg {
                SwapLegId::Nim => vec![0xAB; 200],
                SwapLegId::Counterparty => vec![0xCD; 64],
            };
            (wire, [0x09; 32])
        }
        fn build_claim(
            &self,
            _swap_id: [u8; SWAP_ID_LEN],
            secret: [u8; 32],
        ) -> (Vec<u8>, [u8; 32]) {
            let mut wire = secret.to_vec(); // S first, so the responder reads it back...
            wire.extend_from_slice(&[0xEE; 16]); // ...then padding the signer is free to add.
            (wire, [0x08; 32])
        }
    }

    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let mut h = MeshHarness::new();
    let alice = h.add_participant_with_signer(
        "alice",
        &[1],
        alice_id,
        LadderParams::default(),
        Box::new(PaddedSigner),
    );
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");

    let (coordinator, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "alice never settled through the pluggable signer"
    );
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "bob never settled (the pluggable signer's claim must still carry S for the responder)"
    );

    h.shutdown();
}

#[test]
fn a_responder_declines_a_below_rate_proposal_over_the_mesh() {
    // G29: bob's NodeIdentity carries a rate floor of >= 2.5 NIM per BTC. A lopsided Propose (2.0
    // NIM/BTC) is declined at the node — no coordinator, no phase, no Accept — while a fair Propose
    // (3.0) is accepted. Proves the responder's rate policy gates over the real node loop.
    use crate::codec::encode;
    use crate::mock_radio::MeshHarness;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap::{LadderParams, SwapPhase, SwapTerms};
    use crate::swap_leg::sha256;
    use crate::swap_messages::SwapProposal;
    use crate::swap_session::{NodeIdentity, RatePolicy};
    use crate::swap_wire::encode_swap;
    use crate::test_support::{wait_until, SETTLE};

    let bob_id = NodeIdentity {
        nim_address: [0xB2; 20],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: {
            let mut k = [0x22; 33];
            k[0] = 0x02;
            k
        },
        rate_policy: RatePolicy::min_rate(5, 2), // >= 2.5 NIM per BTC
        max_concurrent_swaps: crate::swap_session::DEFAULT_MAX_CONCURRENT_SWAPS,
    };
    let mut h = MeshHarness::new();
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());

    // `ts` keeps each injected Propose's relay-key distinct (type+sender+timestamp).
    let propose = |swap_id: [u8; 16], give: u64, take: u64, ts: u64| {
        let mut pk = [0x11; 33];
        pk[0] = 0x02;
        let env = SwapProposal {
            swap_id,
            hashlock: sha256(&[42u8; 32]),
            give_amount: give,
            take_amount: take,
            terms: SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            nim_address: [0xA1; 20],
            btc_address: b"tb1qalice".to_vec(),
            btc_pubkey: pk,
            network_id: 5,
        }
        .to_envelope();
        let mut pkt = Packet::new(
            MessageType::SwapPropose,
            [9u8; 8],
            encode_swap(&env).unwrap(),
        );
        pkt.recipient_id = Some(BROADCAST_RECIPIENT);
        pkt.timestamp_ms = ts;
        bob.on_packet_received_from("alice".to_string(), encode(&pkt).unwrap());
    };

    propose([0xD1; 16], 100_000, 50_000, 1); // 2.0 NIM/BTC — below the floor
    propose([0xD2; 16], 150_000, 50_000, 2); // 3.0 NIM/BTC — fair

    // The fair proposal is accepted...
    assert!(
        wait_until(
            || bob.swap_phase([0xD2; 16]) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "the fair-rate proposal should be accepted"
    );
    // ...and the below-rate one created no coordinator at all.
    assert!(
        bob.swap_phase([0xD1; 16]).is_none(),
        "the below-rate proposal must be declined (no coordinator)"
    );

    h.shutdown();
}
