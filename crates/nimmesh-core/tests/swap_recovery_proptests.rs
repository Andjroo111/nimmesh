//! Property tests for the G32 crash-recovery snapshot byte codec
//! ([`nimmesh_core::swap_recovery`]):
//!
//! 1. **Round-trip** — any snapshot survives `encode → decode → encode` byte-for-byte (so decode
//!    reconstructs every field; the re-encode equality is checked instead of the struct, which has no
//!    `Debug` because it carries the secret).
//! 2. **Robustness** — `decode_snapshot` never panics on arbitrary bytes (it parses an untrusted
//!    on-disk file): `Ok` or a structured `Err`, period.

use nimmesh_core::swap::{SwapPhase, SwapRole, SwapTerms};
use nimmesh_core::swap_coordinator::{CoordinatorSnapshot, SwapContext};
use nimmesh_core::swap_recovery::{decode_snapshot, encode_snapshot};
use proptest::collection::vec;
use proptest::prelude::*;

fn phase_of(tag: u8) -> SwapPhase {
    match tag {
        0 => SwapPhase::Proposed,
        1 => SwapPhase::Accepted,
        2 => SwapPhase::InitiatorFunded,
        3 => SwapPhase::SelfFunded,
        4 => SwapPhase::BothFunded,
        5 => SwapPhase::Revealed,
        6 => SwapPhase::Settled,
        7 => SwapPhase::Aborted,
        _ => SwapPhase::Refunded,
    }
}

proptest! {
    /// Any snapshot encodes and decodes back to the same bytes (so no field is lost or reordered).
    #[test]
    fn snapshot_round_trips_byte_for_byte(
        role_tag in 0u8..2,
        phase_tag in 0u8..9,
        swap_id in any::<[u8; 16]>(),
        nim_timeout in any::<u64>(),
        counterparty_timeout in any::<u64>(),
        hashlock in any::<[u8; 32]>(),
        nim_address in any::<[u8; 20]>(),
        btc_pubkey in any::<[u8; 33]>(),
        give_amount in any::<u64>(),
        take_amount in any::<u64>(),
        network_id in any::<u8>(),
        btc_address in vec(any::<u8>(), 0..300),
        secret in proptest::option::of(any::<[u8; 32]>()),
        peer_btc_pubkey in proptest::option::of(any::<[u8; 33]>()),
    ) {
        let snap = CoordinatorSnapshot {
            role: if role_tag == 0 { SwapRole::Initiator } else { SwapRole::Responder },
            phase: phase_of(phase_tag),
            ctx: SwapContext {
                swap_id,
                terms: SwapTerms { nim_timeout, counterparty_timeout },
                hashlock,
                nim_address,
                btc_address,
                btc_pubkey,
                give_amount,
                take_amount,
                network_id,
            },
            secret,
            peer_btc_pubkey,
        };

        let bytes = encode_snapshot(std::slice::from_ref(&snap));
        let decoded = decode_snapshot(&bytes).expect("our own bytes decode");
        prop_assert_eq!(decoded.len(), 1);
        // Re-encoding the decoded form reproduces the bytes exactly → every field round-tripped.
        prop_assert_eq!(encode_snapshot(&decoded), bytes);
    }

    /// A list of snapshots round-trips too (the `u16` count + sequential records).
    #[test]
    fn a_list_round_trips(n in 0usize..8, seed in any::<u8>()) {
        let snaps: Vec<CoordinatorSnapshot> = (0..n).map(|i| CoordinatorSnapshot {
            role: SwapRole::Initiator,
            phase: phase_of((i as u8) % 9),
            ctx: SwapContext {
                swap_id: [seed.wrapping_add(i as u8); 16],
                terms: SwapTerms { nim_timeout: 10_000, counterparty_timeout: 5_000 },
                hashlock: [seed; 32],
                nim_address: [0xA1; 20],
                btc_address: b"tb1qalice".to_vec(),
                btc_pubkey: [0x02; 33],
                give_amount: 100_000,
                take_amount: 50_000,
                network_id: 5,
            },
            secret: Some([42; 32]),
            peer_btc_pubkey: None,
        }).collect();
        let bytes = encode_snapshot(&snaps);
        let decoded = decode_snapshot(&bytes).expect("round-trips");
        prop_assert_eq!(decoded.len(), n);
        prop_assert_eq!(encode_snapshot(&decoded), bytes);
    }

    /// `decode_snapshot` never panics on arbitrary bytes — only `Ok` or `Err`.
    #[test]
    fn decode_never_panics(bytes in vec(any::<u8>(), 0..2048)) {
        let _ = decode_snapshot(&bytes);
    }
}
