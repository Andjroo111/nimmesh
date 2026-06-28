//! Property tests for the swap-session router ([`nimmesh_core::swap_session::SwapSession`]) over a
//! hostile mesh (G15). A node feeds `on_message` untrusted, arbitrary, out-of-order, replayed
//! packets; whatever the stream, two invariants must hold:
//!
//! 1. **Panic-free** — no sequence of (kind, bytes) can panic the router (it parses untrusted input).
//! 2. **No stranded coordinator** — the session only ever creates a coordinator by *accepting* a
//!    `Propose`, so every coordinator it holds is at least `Accepted`. A coordinator left in the
//!    initial `Proposed` phase would be a half-built / stranded swap; the router must never produce
//!    one. (It also never exceeds one coordinator per distinct `swap_id`.)

use nimmesh_core::swap::{LadderParams, SwapPhase, SwapTerms};
use nimmesh_core::swap_messages::{abort, tx_envelope, SwapAcceptance, SwapProposal};
use nimmesh_core::swap_session::{NodeIdentity, SwapSession};
use nimmesh_core::swap_wire::{
    encode_swap, SwapKind, SwapLegId, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN, SWAP_ID_LEN,
};
use proptest::prelude::*;

const HASHLOCK: [u8; 32] = [0x42; 32];

fn node_identity() -> NodeIdentity {
    let mut pk = [0x22; BTC_PUBKEY_LEN];
    pk[0] = 0x02;
    NodeIdentity {
        nim_address: [0xB2; NIM_ADDRESS_LEN],
        btc_address: b"tb1qnode".to_vec(),
        btc_pubkey: pk,
    }
}

/// A small pool of swap_ids so the stream naturally exercises replays, cross-kind messages for the
/// same id, and concurrent distinct swaps.
fn pool_id(i: usize) -> [u8; SWAP_ID_LEN] {
    [i as u8 + 1; SWAP_ID_LEN]
}

/// A safe-ladder `Propose` for a pooled id — identical on every call for the same id, so a resend is
/// a true replay.
fn valid_propose(i: usize) -> Vec<u8> {
    let mut pk = [0x11; BTC_PUBKEY_LEN];
    pk[0] = 0x02;
    let p = SwapProposal {
        swap_id: pool_id(i),
        hashlock: HASHLOCK,
        give_amount: 100_000,
        take_amount: 50_000,
        terms: SwapTerms {
            nim_timeout: 10_000,
            counterparty_timeout: 5_000,
        },
        nim_address: [0xA1; NIM_ADDRESS_LEN],
        btc_address: b"tb1qalice".to_vec(),
        btc_pubkey: pk,
        network_id: 5,
    };
    encode_swap(&p.to_envelope()).unwrap()
}

/// One step in a hostile packet stream.
#[derive(Debug, Clone)]
enum Step {
    Propose(usize),
    Accept(usize),
    FundingProof(usize),
    Reveal(usize),
    Abort(usize),
    /// Arbitrary bytes decoded as an arbitrary kind — the fuzz core.
    Garbage(Vec<u8>, u8),
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0usize..3).prop_map(Step::Propose),
        (0usize..3).prop_map(Step::Accept),
        (0usize..3).prop_map(Step::FundingProof),
        (0usize..3).prop_map(Step::Reveal),
        (0usize..3).prop_map(Step::Abort),
        (proptest::collection::vec(any::<u8>(), 0..300), 0u8..5)
            .prop_map(|(b, k)| Step::Garbage(b, k)),
    ]
}

fn kind_of(idx: u8) -> SwapKind {
    match idx {
        0 => SwapKind::Propose,
        1 => SwapKind::Accept,
        2 => SwapKind::FundingProof,
        3 => SwapKind::PreimageReveal,
        _ => SwapKind::Abort,
    }
}

fn apply(session: &mut SwapSession, step: &Step) {
    let mut pk = [0x33; BTC_PUBKEY_LEN];
    pk[0] = 0x03;
    let _ = match step {
        Step::Propose(i) => session.on_message(SwapKind::Propose, &valid_propose(*i), 0),
        Step::Accept(i) => {
            let env = SwapAcceptance {
                swap_id: pool_id(*i),
                nim_address: [0xC3; NIM_ADDRESS_LEN],
                btc_address: b"tb1qbob".to_vec(),
                btc_pubkey: pk,
            }
            .to_envelope();
            session.on_message(SwapKind::Accept, &encode_swap(&env).unwrap(), 0)
        }
        Step::FundingProof(i) => {
            let env = tx_envelope(pool_id(*i), SwapLegId::Nim, vec![0x11; 248], [0xC1; 32]);
            session.on_message(SwapKind::FundingProof, &encode_swap(&env).unwrap(), 0)
        }
        Step::Reveal(i) => {
            let env = tx_envelope(
                pool_id(*i),
                SwapLegId::Counterparty,
                HASHLOCK.to_vec(),
                [0xC3; 32],
            );
            session.on_message(SwapKind::PreimageReveal, &encode_swap(&env).unwrap(), 0)
        }
        Step::Abort(i) => {
            let env = abort(pool_id(*i), 0);
            session.on_message(SwapKind::Abort, &encode_swap(&env).unwrap(), 0)
        }
        Step::Garbage(bytes, kind_idx) => session.on_message(kind_of(*kind_idx), bytes, 0),
    };
}

proptest! {
    /// Any interleaving of valid, replayed, out-of-order, aborting, and garbage packets leaves the
    /// router panic-free and never strands a coordinator in the initial `Proposed` phase.
    #[test]
    fn arbitrary_packet_streams_never_panic_or_strand_a_coordinator(
        steps in proptest::collection::vec(arb_step(), 0..80),
    ) {
        let mut session = SwapSession::new(node_identity(), LadderParams::default());
        for step in &steps {
            apply(&mut session, step); // must not panic for any step.
        }
        for (_, phase) in session.phases() {
            prop_assert_ne!(phase, SwapPhase::Proposed, "a coordinator was left half-built");
        }
        // After reaping terminal swaps, nothing terminal lingers either.
        session.reap_terminal();
        for (_, phase) in session.phases() {
            prop_assert!(!phase.is_terminal(), "a terminal coordinator survived reaping");
        }
    }
}
