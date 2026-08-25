//! # transport_mtu_tests — a signed Nimiq tx must survive every transport in ONE unit
//!
//! nimmesh's expansion beyond Bluetooth rests on a single property: a signed Albatross
//! transfer is small enough to cross a constrained radio **without fragmentation**. See
//! [`docs/TRANSPORTS.md`]. Every prior-art attempt to put Bitcoin on a LoRa mesh has to
//! chunk-and-reassemble; we do not, and that difference is the whole reason the LoRa /
//! SMS work is cheap for us.
//!
//! That property is a protocol invariant, not a happy accident, so it is asserted here.
//! If someone widens the wire format past a transport's MTU, CI goes red in this file
//! instead of a payment silently failing in a field with no cell signal.
//!
//! Sizes are taken from a REAL serialized transaction, never from a magic number, so a
//! layout change cannot pass by leaving a stale constant untouched.

use crate::nimiq::address::Address;
use crate::nimiq::tx::{Transfer, BASIC_WIRE_LEN, PUBLIC_KEY_LEN, SIGNATURE_LEN};

/// Meshtastic's maximum payload for one packet. 256-byte packet, 237 usable.
/// <https://meshtastic.org/docs/development/firmware/portnum/>
const MESHTASTIC_MAX_PAYLOAD: usize = 237;

/// Meshtastic's own header overhead inside that packet, subtracted before we measure.
const MESHTASTIC_HEADER: usize = 16;

/// One binary SMS under GSM 03.38, 8-bit encoding, no concatenation header. This is
/// the RAW PDU budget (modem-readable); the port-addressed form an Android app can
/// receive pays a 7-byte UDH, leaving 133 — see docs/SMS-TRANSPORT.md for the
/// 130-byte compact form that fits it.
const SMS_BINARY_MAX_PAYLOAD: usize = 140;

/// The smallest nimmesh BLE padding block (PROTOCOL.md: PKCS#7 to 256/512/1024/2048).
const BLE_SMALLEST_BLOCK: usize = 256;

/// A raw LoRa frame payload ceiling (SX126x, explicit header).
const LORA_RAW_MAX_PAYLOAD: usize = 255;

/// Build a real, fully-formed basic transfer and serialize it the way the mesh does.
fn real_signed_wire() -> Vec<u8> {
    let transfer = Transfer {
        sender: Address([0x11; 20]),
        recipient: Address([0x22; 20]),
        // Deliberately the widest realistic values: max-ish amount and a high block
        // height, so no integer is accidentally serialized short.
        value: u64::MAX,
        fee: u64::MAX,
        validity_start_height: u32::MAX,
        network_id: 42,
    };
    transfer.serialize_basic(&[0xAB; PUBLIC_KEY_LEN], &[0xCD; SIGNATURE_LEN])
}

#[test]
fn a_real_signed_transfer_is_exactly_the_advertised_139_bytes() {
    let wire = real_signed_wire();
    assert_eq!(
        wire.len(),
        BASIC_WIRE_LEN,
        "serialize_basic disagrees with BASIC_WIRE_LEN"
    );
    assert_eq!(
        wire.len(),
        139,
        "the 139-byte claim is load-bearing in README.md and docs/TRANSPORTS.md; \
         if the wire format legitimately changed, update BOTH docs in the same commit"
    );
}

#[test]
fn a_transaction_fits_one_meshtastic_packet_with_room_to_spare() {
    let wire = real_signed_wire();
    let budget = MESHTASTIC_MAX_PAYLOAD - MESHTASTIC_HEADER;
    assert!(
        wire.len() <= budget,
        "a signed tx ({} B) no longer fits one Meshtastic packet ({} B usable). \
         Single-packet delivery is the entire LoRa thesis: losing it means \
         chunk-and-reassemble, which is what btcmesh has to do.",
        wire.len(),
        budget
    );
    // Not a cliff-edge fit. Guard the margin so a slow creep gets caught early.
    assert!(
        budget - wire.len() >= 64,
        "margin collapsed to {} B; the format is creeping toward the Meshtastic MTU",
        budget - wire.len()
    );
}

#[test]
fn a_transaction_fits_a_single_binary_sms() {
    let wire = real_signed_wire();
    assert!(
        wire.len() <= SMS_BINARY_MAX_PAYLOAD,
        "a signed tx ({} B) no longer fits one binary SMS ({} B). This is the \
         cell-signal-but-no-data path (issue #24) and it has only ever had {} B \
         of headroom.",
        wire.len(),
        SMS_BINARY_MAX_PAYLOAD,
        SMS_BINARY_MAX_PAYLOAD - BASIC_WIRE_LEN
    );
}

#[test]
fn a_transaction_fits_the_transports_we_already_ship_or_plan() {
    let wire = real_signed_wire();
    for (name, mtu) in [
        ("BLE smallest padding block", BLE_SMALLEST_BLOCK),
        ("raw LoRa frame", LORA_RAW_MAX_PAYLOAD),
    ] {
        assert!(
            wire.len() <= mtu,
            "a signed tx ({} B) does not fit {} ({} B)",
            wire.len(),
            name,
            mtu
        );
    }
}

#[test]
fn the_sms_headroom_is_exactly_one_byte_and_that_is_documented() {
    // docs/TRANSPORTS.md leans on this being startlingly tight. If it ever loosens or
    // tightens, the doc is wrong and should be corrected rather than quietly drifting.
    assert_eq!(
        SMS_BINARY_MAX_PAYLOAD - BASIC_WIRE_LEN,
        1,
        "docs/TRANSPORTS.md claims exactly one byte of SMS headroom"
    );
}
