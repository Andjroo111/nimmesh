//! Property tests for the swap-wire TLV codec ([`nimmesh_core::swap_wire`]):
//!
//! 1. **Robustness** — `decode_swap` must *never* panic on arbitrary bytes for any [`SwapKind`];
//!    it returns `Ok` or a structured `Err`, period (it parses untrusted mesh input).
//! 2. **Round-trip** — any valid [`SwapEnvelope`] survives `encode_swap` → `decode_swap`
//!    byte-for-byte, and re-encoding the decoded form is stable. Covers the `btc_pubkey` field.

use nimmesh_core::swap_wire::{
    decode_swap, encode_swap, SwapEnvelope, SwapKind, SwapLegId, BTC_PUBKEY_LEN, HASH_LEN,
    NIM_ADDRESS_LEN, SWAP_ID_LEN,
};
use proptest::prelude::*;

fn arb_leg() -> impl Strategy<Value = SwapLegId> {
    prop_oneof![Just(SwapLegId::Nim), Just(SwapLegId::Counterparty)]
}

/// A fully-populated envelope (every field set with a valid length) — it satisfies every kind's
/// required-field set, so it can be decoded as any [`SwapKind`].
fn arb_swap_envelope() -> impl Strategy<Value = SwapEnvelope> {
    // Nested 7+7 tuples — proptest's tuple Strategy impl tops out below 14 elements.
    (
        (
            any::<[u8; SWAP_ID_LEN]>(),
            any::<[u8; HASH_LEN]>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<[u8; NIM_ADDRESS_LEN]>(),
        ),
        (
            proptest::collection::vec(any::<u8>(), 0..=255), // counterparty_address (≤ TLV max)
            arb_leg(),
            proptest::collection::vec(any::<u8>(), 0..=255), // tx_wire
            any::<[u8; HASH_LEN]>(),
            any::<u8>(),
            any::<u8>(),
            any::<[u8; BTC_PUBKEY_LEN]>(),
        ),
    )
        .prop_map(
            |(
                (swap_id, hashlock, give, take, nim_t, cp_t, nim_addr),
                (cp_addr, leg, tx_wire, tx_id, network, reason, btc_pk),
            )| SwapEnvelope {
                swap_id,
                hashlock: Some(hashlock),
                give_amount: Some(give),
                take_amount: Some(take),
                nim_timeout: Some(nim_t),
                counterparty_timeout: Some(cp_t),
                nim_address: Some(nim_addr),
                counterparty_address: Some(cp_addr),
                leg: Some(leg),
                tx_wire: Some(tx_wire),
                tx_id: Some(tx_id),
                network_id: Some(network),
                reason: Some(reason),
                btc_pubkey: Some(btc_pk),
            },
        )
}

proptest! {
    /// `decode_swap` never panics on arbitrary bytes, for every kind — only Ok or Err.
    #[test]
    fn decode_swap_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
        kind_idx in 0u8..5,
    ) {
        let kind = match kind_idx {
            0 => SwapKind::Propose,
            1 => SwapKind::Accept,
            2 => SwapKind::FundingProof,
            3 => SwapKind::PreimageReveal,
            _ => SwapKind::Abort,
        };
        let _ = decode_swap(kind, &bytes);
    }

    /// Any valid (fully-populated) envelope round-trips byte-for-byte, and re-encoding is stable.
    #[test]
    fn envelope_round_trips(env in arb_swap_envelope()) {
        let bytes = encode_swap(&env).unwrap();
        let back = decode_swap(SwapKind::Propose, &bytes).expect("our own bytes decode");
        prop_assert_eq!(&back, &env);
        prop_assert_eq!(encode_swap(&back).unwrap(), bytes);
    }

    /// A decoded envelope re-decodes identically regardless of which kind we read it as (the kinds
    /// only differ in which fields are *required*, never in how a present field is parsed).
    #[test]
    fn decode_is_kind_agnostic_for_present_fields(env in arb_swap_envelope()) {
        let bytes = encode_swap(&env).unwrap();
        let as_propose = decode_swap(SwapKind::Propose, &bytes).unwrap();
        let as_funding = decode_swap(SwapKind::FundingProof, &bytes).unwrap();
        prop_assert_eq!(as_propose, as_funding);
    }
}
