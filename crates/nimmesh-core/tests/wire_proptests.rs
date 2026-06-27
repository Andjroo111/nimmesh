//! Property / fuzz tests for the G4 nimmesh wire codec.
//!
//! Two invariants the protocol layer must never violate (PROTOCOL.md, GOAL.md core
//! value #6 — treat mesh input as hostile):
//!
//! 1. **Robustness** — [`codec::decode`] / [`envelope::decode_envelope`] must *never*
//!    panic on arbitrary or malformed bytes, only ever return an `Err` (or `Ok`).
//! 2. **Round-trip** — any structurally valid [`Packet`] / [`NimiqEnvelope`] survives
//!    `encode` → `decode` byte-for-byte (padding is transparent).

use nimmesh_core::codec::{decode, encode};
use nimmesh_core::envelope::{decode_envelope, encode_envelope, NimiqEnvelope};
use nimmesh_core::packet::{MessageType, Packet, PacketFlags};
use proptest::prelude::*;

/// Strategy for an arbitrary, well-formed [`Packet`].
fn arb_packet() -> impl Strategy<Value = Packet> {
    let msg_type = prop_oneof![
        Just(MessageType::Fragment),
        Just(MessageType::RequestSync),
        Just(MessageType::NimiqTx),
        Just(MessageType::NimiqTxReceipt),
        Just(MessageType::NimiqHeadBeacon),
    ];
    (
        msg_type,
        any::<u8>(),                                    // ttl
        any::<u64>(),                                   // timestamp_ms
        any::<[u8; 8]>(),                               // sender_id
        proptest::option::of(any::<[u8; 8]>()),         // recipient_id
        proptest::collection::vec(any::<u8>(), 0..600), // payload
        proptest::option::of(any::<[u8; 64]>()),        // signature
        any::<(bool, bool, bool)>(),                    // standalone flags
    )
        .prop_map(
            |(msg_type, ttl, ts, sender, recipient, payload, sig, (c, r, rsr))| Packet {
                version: 1,
                msg_type,
                ttl,
                timestamp_ms: ts,
                flags: PacketFlags {
                    is_compressed: c,
                    has_route: r,
                    is_rsr: rsr,
                },
                sender_id: sender,
                recipient_id: recipient,
                payload,
                signature: sig,
            },
        )
}

/// Strategy for an arbitrary, well-formed [`NimiqEnvelope`] (all values <= 255 B).
fn arb_envelope() -> impl Strategy<Value = NimiqEnvelope> {
    (
        proptest::collection::vec(any::<u8>(), 0..=255), // tx_wire
        any::<u8>(),                                     // network_id
        proptest::option::of(any::<u32>()),              // valid_until
        proptest::option::of(any::<[u8; 32]>()),         // tx_id
        proptest::option::of(proptest::collection::vec(any::<u8>(), 0..=255)), // enc_memo
        any::<bool>(),                                   // want_receipt
    )
        .prop_map(
            |(tx_wire, network_id, valid_until, tx_id, enc_memo, want_receipt)| NimiqEnvelope {
                tx_wire,
                network_id,
                valid_until,
                tx_id,
                enc_memo,
                want_receipt,
            },
        )
}

proptest! {
    /// `decode` never panics on arbitrary bytes — it returns `Ok` or `Err`, period.
    #[test]
    fn decode_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = decode(&bytes);
    }

    /// `decode_envelope` never panics on arbitrary bytes.
    #[test]
    fn decode_envelope_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = decode_envelope(&bytes);
    }

    /// Any valid packet round-trips byte-for-byte through encode → decode.
    #[test]
    fn packet_round_trips(packet in arb_packet()) {
        let bytes = encode(&packet).expect("valid packet encodes");
        let back = decode(&bytes).expect("our own bytes decode");
        prop_assert_eq!(back, packet);
    }

    /// Any valid envelope round-trips, and the decoded form re-encodes identically.
    #[test]
    fn envelope_round_trips(env in arb_envelope()) {
        let bytes = encode_envelope(&env).expect("valid envelope encodes");
        let back = decode_envelope(&bytes).expect("our own bytes decode");
        prop_assert_eq!(&back, &env);
        // Stable: re-encoding the decoded form yields the same bytes.
        prop_assert_eq!(encode_envelope(&back).unwrap(), bytes);
    }

    /// A valid envelope carried as a nimiqTx packet payload survives both layers.
    #[test]
    fn envelope_inside_packet_round_trips(env in arb_envelope(), sender in any::<[u8; 8]>()) {
        let payload = encode_envelope(&env).unwrap();
        let mut packet = Packet::new(MessageType::NimiqTx, sender, payload);
        packet.recipient_id = Some(nimmesh_core::packet::BROADCAST_RECIPIENT);
        let wire = encode(&packet).unwrap();
        let decoded_packet = decode(&wire).unwrap();
        let decoded_env = decode_envelope(&decoded_packet.payload).unwrap();
        prop_assert_eq!(decoded_env, env);
    }
}
