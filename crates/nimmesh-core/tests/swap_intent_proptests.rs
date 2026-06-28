//! Property tests for the discovery-intent codec + authenticity ([`nimmesh_core::swap_intent`]):
//!
//! 1. **Robustness** — `decode_intent` must *never* panic on arbitrary bytes; it returns `Ok` or a
//!    structured `Err` (it parses untrusted, hostile mesh floods).
//! 2. **Round-trip** — any well-formed [`SwapIntent`] survives `encode_intent` → `decode_intent`
//!    byte-for-byte, covering every field (incl. the G41 pubkey + signature and the G40 size band).
//! 3. **Authenticity** — `verify_authentic` never panics on arbitrary input; a freshly `sign_intent`-ed
//!    intent always verifies, and ANY single-byte tamper of the signed content or the signature breaks
//!    it (G41 forgery resistance, as a property over arbitrary intents).

use nimmesh_core::swap_intent::{
    decode_intent, encode_intent, sign_intent, Asset, SwapIntent, INTENT_PUBKEY_LEN, INTENT_SIG_LEN,
};
use nimmesh_core::swap_wire::{BTC_PUBKEY_LEN, NIM_ADDRESS_LEN};
use proptest::prelude::*;

fn arb_asset() -> impl Strategy<Value = Asset> {
    prop_oneof![Just(Asset::Nim), Just(Asset::Btc), Just(Asset::Usdc)]
}

/// A fully-arbitrary well-formed intent (every field independently generated).
fn arb_intent() -> impl Strategy<Value = SwapIntent> {
    (
        (
            arb_asset(),
            arb_asset(), // counter_asset (independently arbitrary — the codec carries any pair)
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<[u8; INTENT_PUBKEY_LEN]>(),
        ),
        (
            any::<[u8; NIM_ADDRESS_LEN]>(),
            any::<[u8; BTC_PUBKEY_LEN]>(),
            proptest::collection::vec(any::<u8>(), 0..=255), // btc_address (≤ u16 TLV len)
            any::<u8>(),
            any::<[u8; INTENT_SIG_LEN]>(),
        ),
    )
        .prop_map(
            |(
                (
                    gives,
                    counter_asset,
                    nim_amount,
                    btc_amount,
                    expiry_height,
                    min_nim,
                    max_nim,
                    nim_pubkey,
                ),
                (nim_address, btc_pubkey, btc_address, network_id, signature),
            )| SwapIntent {
                gives,
                counter_asset,
                nim_amount,
                btc_amount,
                expiry_height,
                min_nim,
                max_nim,
                nim_pubkey,
                nim_address,
                btc_pubkey,
                btc_address,
                network_id,
                signature,
            },
        )
}

proptest! {
    /// `decode_intent` never panics on arbitrary bytes — only `Ok` or a structured `Err`.
    #[test]
    fn decode_intent_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = decode_intent(&bytes);
    }

    /// Any well-formed intent round-trips byte-for-byte, and re-encoding the decoded form is stable.
    #[test]
    fn intent_round_trips(intent in arb_intent()) {
        let bytes = encode_intent(&intent);
        let back = decode_intent(&bytes).expect("our own bytes decode");
        prop_assert_eq!(&back, &intent);
        prop_assert_eq!(encode_intent(&back), bytes);
    }

    /// `verify_authentic` never panics on an arbitrary (probably-unsigned) intent.
    #[test]
    fn verify_authentic_never_panics(intent in arb_intent()) {
        let _ = intent.verify_authentic();
    }

    /// A freshly signed intent verifies; any single-byte tamper of the signed content or the signature
    /// makes it fail (forgery resistance as a property over arbitrary intents + seeds).
    #[test]
    fn a_signed_intent_verifies_and_any_tamper_breaks_it(
        base in arb_intent(),
        seed in any::<[u8; 32]>(),
    ) {
        let mut intent = base;
        sign_intent(&mut intent, &seed);
        prop_assert!(intent.verify_authentic());
        // It survives the wire round-trip and still verifies.
        prop_assert!(decode_intent(&encode_intent(&intent)).unwrap().verify_authentic());

        // Tamper a signed-content field → the signature no longer covers it.
        let mut content = intent.clone();
        content.btc_amount ^= 1;
        prop_assert!(!content.verify_authentic());

        // Tamper the claimed address → the pubkey no longer hashes to it.
        let mut addr = intent.clone();
        addr.nim_address[0] ^= 0xFF;
        prop_assert!(!addr.verify_authentic());

        // Tamper the signature itself → it no longer verifies.
        let mut sig = intent.clone();
        sig.signature[0] ^= 0xFF;
        prop_assert!(!sig.verify_authentic());
    }
}
