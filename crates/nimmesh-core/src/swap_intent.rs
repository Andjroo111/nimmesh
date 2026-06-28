//! # swap_intent — counterparty discovery over the mesh (G34)
//!
//! A swap needs two parties; today they have to already know each other. A [`SwapIntent`] is a
//! lightweight **advertisement** a node floods ("I want to give NIM for BTC at rate R", with my
//! addresses) — no hashlock, no secret, no commitment. A node holding the **complementary** intent
//! (it wants the mirror trade at a crossing rate) reacts by kicking off a real `SwapPropose`: the
//! intent is the discovery layer, the existing swap protocol is the settlement layer.
//!
//! By convention the **NIM-giver is the initiator** (it generates `S` and proposes; it claims the
//! counterparty leg, revealing `S`). So matching is one-sided: a node whose standing intent gives
//! NIM, on seeing a counter-asset giver intent that crosses on rate, initiates; the counter-asset
//! giver just waits for the Propose (its [`crate::swap_rate::RatePolicy`] then governs acceptance).
//! The intent rides the mesh as opaque blind-relayed bytes like every other swap packet.
//!
//! The counterparty asset is either **BTC** or **USDC-on-Polygon** ([`Asset`]); a NIM-giver names the
//! one it wants in `counter_asset`, so NIM⇄USDC discovers exactly like NIM⇄BTC and the two markets
//! stay separate (a NIM-wants-BTC intent never matches a USDC giver).

use ed25519_dalek::Signer;

use crate::swap_wire::{BTC_PUBKEY_LEN, NIM_ADDRESS_LEN};

/// Which asset the advertiser **funds** in the trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asset {
    /// The advertiser gives NIM (so it is the would-be initiator).
    Nim,
    /// The advertiser gives BTC (so it is the would-be responder).
    Btc,
    /// The advertiser gives USDC on Polygon (so it is the would-be responder). NIM⇄USDC discovers over
    /// the mesh exactly like NIM⇄BTC; the counterparty leg is `swap_usdc_leg::UsdcLeg` (P2).
    Usdc,
}

/// A discovery advertisement: the trade a node wants, plus the addresses a matcher needs to propose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapIntent {
    /// Which side the advertiser funds.
    pub gives: Asset,
    /// The **non-NIM** asset of this trade (BTC or USDC). A non-NIM giver sets this to its own `gives`;
    /// a NIM giver sets it to the asset it WANTS in return. A swap only forms when both sides name the
    /// same `counter_asset` — needed because BTC sats and 6-decimal micro-USDC are different scales, so
    /// `btc_amount`'s unit (and thus the rate-cross) is only meaningful once the counter asset is fixed.
    pub counter_asset: Asset,
    /// NIM in the trade, in luna.
    pub nim_amount: u64,
    /// BTC in the trade, in satoshis.
    pub btc_amount: u64,
    /// The last chain height at which this advertisement is still valid. Once the head passes it the
    /// intent is **expired** — a matcher won't act on it and a relay won't carry it onward (G35). An
    /// advertiser sets this to its current head plus a freshness window.
    pub expiry_height: u64,
    /// The smallest NIM trade size (in luna) the advertiser will accept (G40). A swap whose NIM size is
    /// below this is too small to match. `0` means "no lower bound".
    pub min_nim: u64,
    /// The largest NIM trade size (in luna) the advertiser will accept (G40). A swap whose NIM size is
    /// above this is too large to match. `u64::MAX` means "no upper bound".
    pub max_nim: u64,
    /// The advertiser's Ed25519 public key (32 bytes). It must hash to `nim_address` and is the key the
    /// `signature` verifies under — together they authenticate the intent (G41).
    pub nim_pubkey: [u8; INTENT_PUBKEY_LEN],
    /// The advertiser's NIM address (20 raw bytes) — `Blake2b-256(nim_pubkey)[..20]` for an authentic
    /// intent.
    pub nim_address: [u8; NIM_ADDRESS_LEN],
    /// The advertiser's BTC claimant pubkey (33 bytes).
    pub btc_pubkey: [u8; BTC_PUBKEY_LEN],
    /// The advertiser's BTC payout address bytes.
    pub btc_address: Vec<u8>,
    /// The Albatross network id (a swap only forms within one network).
    pub network_id: u8,
    /// The advertiser's Ed25519 signature (64 bytes) over [`SwapIntent::signing_bytes`] — every field
    /// EXCEPT this one. A matcher acts only on an intent whose signature verifies (G41).
    pub signature: [u8; INTENT_SIG_LEN],
}

/// Ed25519 public-key length carried by an intent (G41).
pub const INTENT_PUBKEY_LEN: usize = 32;
/// Ed25519 signature length carried by an intent (G41).
pub const INTENT_SIG_LEN: usize = 64;

impl SwapIntent {
    /// Whether **this** node (holding `self` as its standing intent) should INITIATE a swap in
    /// response to `incoming`. True only when: `self` gives NIM and `incoming` gives exactly the
    /// non-NIM asset `self` wants (`incoming.gives == self.counter_asset`, and `incoming` is not also
    /// a NIM-giver), so this node is the NIM-giver / initiator; the networks match; both counterparty
    /// amounts are non-zero; and the rates cross — `self`'s NIM-per-counter is at least what `incoming`
    /// asks, i.e. `self.nim/self.counter >= incoming.nim/incoming.counter`, cross-multiplied to avoid
    /// float / overflow. (Both sides express `btc_amount` in the same unit because `counter_asset`
    /// matches — sats for BTC, micro-USDC for USDC.) A counter-asset giver never initiates (it waits
    /// for the Propose), so this is one-sided by design, and a NIM-wants-BTC intent never matches a
    /// USDC giver (or vice versa).
    pub fn would_initiate_against(&self, incoming: &SwapIntent) -> bool {
        self.gives == Asset::Nim
            && incoming.gives != Asset::Nim
            && incoming.gives == self.counter_asset
            && self.network_id == incoming.network_id
            && self.btc_amount > 0
            && incoming.btc_amount > 0
            && (self.nim_amount as u128) * (incoming.btc_amount as u128)
                >= (incoming.nim_amount as u128) * (self.btc_amount as u128)
    }

    /// Whether this intent is still valid at chain height `head`. It expires once the head passes
    /// `expiry_height`, so a stale ad stops matching (and stops being relayed) instead of lingering on
    /// the mesh forever (G35). `head == 0` (no beacon heard yet) treats every non-degenerate intent as
    /// fresh, matching how the swap timelocks already read an unknown head as 0.
    pub fn is_fresh(&self, head: u64) -> bool {
        head <= self.expiry_height
    }

    /// Whether the NIM trade sizes are mutually acceptable (G40). The swap executes at the initiator's
    /// (`self`'s) amounts, so both parties must be willing to trade at roughly that size: `self`'s NIM
    /// size must fall in `incoming`'s band, and `incoming`'s advertised NIM size must fall in `self`'s
    /// band. This stops a rate-crossing but wildly mis-sized counterparty (a dust or a whale) matching.
    pub fn amount_compatible(&self, incoming: &SwapIntent) -> bool {
        self.nim_amount >= incoming.min_nim
            && self.nim_amount <= incoming.max_nim
            && incoming.nim_amount >= self.min_nim
            && incoming.nim_amount <= self.max_nim
    }

    /// The canonical bytes an intent's [`signature`](SwapIntent::signature) covers (G41): every field
    /// EXCEPT the signature itself, i.e. the full wire encoding minus the trailing 64 signature bytes.
    pub fn signing_bytes(&self) -> Vec<u8> {
        encode_intent_content(self)
    }

    /// Whether this intent is authentic (G41): its embedded `nim_pubkey` hashes to the claimed
    /// `nim_address`, AND its `signature` is a valid Ed25519 signature (RFC-8032 `verify_strict`) over
    /// [`signing_bytes`](SwapIntent::signing_bytes). A forged intent — a key that doesn't match the
    /// address, a tampered field, or a junk signature — returns `false`. Pure + panic-free.
    pub fn verify_authentic(&self) -> bool {
        if crate::nimiq::address::Address::from_public_key(&self.nim_pubkey).as_bytes()
            != &self.nim_address
        {
            return false;
        }
        let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&self.nim_pubkey) else {
            return false;
        };
        vk.verify_strict(
            &self.signing_bytes(),
            &ed25519_dalek::Signature::from_bytes(&self.signature),
        )
        .is_ok()
    }
}

/// Sign `intent` with the Ed25519 secret seed (G41), filling its `nim_pubkey`, `nim_address`, and
/// `signature` so it passes [`SwapIntent::verify_authentic`]. The advertiser/test helper for producing
/// an authentic intent — the seed stays on the host (a production advertiser signs via its enclave).
pub fn sign_intent(intent: &mut SwapIntent, secret: &[u8; 32]) {
    let sk = ed25519_dalek::SigningKey::from_bytes(secret);
    let pubkey = sk.verifying_key().to_bytes();
    intent.nim_pubkey = pubkey;
    intent.nim_address = *crate::nimiq::address::Address::from_public_key(&pubkey).as_bytes();
    intent.signature = sk.sign(&intent.signing_bytes()).to_bytes();
}

/// G45: the PRIVACY-PRESERVING way to advertise. Sign `intent` under a **fresh, ephemeral** NIM key
/// derived from per-advertisement randomness, NOT the node's main key — so the flooded intent reveals
/// nothing about the advertiser's main wallet and two of its advertisements don't link on the NIM
/// identity. The advertiser MUST also rotate the BTC pubkey/address it puts in the intent (set them
/// from a per-advertisement BTC key before calling this) and sweep the swap proceeds to its main
/// wallet afterward. Mechanically identical to [`sign_intent`]; the name makes the safe path the
/// obvious one. Full threat model + residual leaks: `docs/swap/DISCOVERY-PRIVACY.md`.
pub fn sign_intent_ephemeral(intent: &mut SwapIntent, ephemeral_nim_seed: &[u8; 32]) {
    sign_intent(intent, ephemeral_nim_seed);
}

/// A decode failure (carries no payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentError {
    /// The bytes ended early.
    Truncated,
    /// A field was outside its domain (bad asset tag).
    Malformed,
}

/// The 1-byte wire tag for an asset.
fn asset_to_u8(a: Asset) -> u8 {
    match a {
        Asset::Nim => 0,
        Asset::Btc => 1,
        Asset::Usdc => 2,
    }
}

/// Decode a 1-byte asset tag (`Malformed` on an unknown byte).
fn asset_from_u8(b: u8) -> Result<Asset, IntentError> {
    match b {
        0 => Ok(Asset::Nim),
        1 => Ok(Asset::Btc),
        2 => Ok(Asset::Usdc),
        _ => Err(IntentError::Malformed),
    }
}

/// Encode an intent's **signed content** — every field except the trailing signature (G41). This is
/// exactly what [`SwapIntent::signing_bytes`] signs and verifies over.
fn encode_intent_content(intent: &SwapIntent) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(asset_to_u8(intent.gives));
    out.push(asset_to_u8(intent.counter_asset));
    out.extend_from_slice(&intent.nim_amount.to_be_bytes());
    out.extend_from_slice(&intent.btc_amount.to_be_bytes());
    out.extend_from_slice(&intent.expiry_height.to_be_bytes());
    out.extend_from_slice(&intent.min_nim.to_be_bytes());
    out.extend_from_slice(&intent.max_nim.to_be_bytes());
    out.extend_from_slice(&intent.nim_pubkey);
    out.extend_from_slice(&intent.nim_address);
    out.extend_from_slice(&intent.btc_pubkey);
    out.push(intent.network_id);
    out.extend_from_slice(&(intent.btc_address.len() as u16).to_be_bytes());
    out.extend_from_slice(&intent.btc_address);
    out
}

/// Encode an intent to bytes (flooded as a `SwapIntent` packet payload): the signed content followed
/// by the 64-byte signature (G41).
pub fn encode_intent(intent: &SwapIntent) -> Vec<u8> {
    let mut out = encode_intent_content(intent);
    out.extend_from_slice(&intent.signature);
    out
}

/// Decode an intent from bytes — panic-free on arbitrary input (every read is bounds-checked).
pub fn decode_intent(bytes: &[u8]) -> Result<SwapIntent, IntentError> {
    let mut pos = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let end = pos.checked_add(n)?;
        let s = bytes.get(pos..end)?;
        pos = end;
        Some(s)
    };
    let t = IntentError::Truncated;
    let gives = asset_from_u8(take(1).ok_or(t)?[0])?;
    let counter_asset = asset_from_u8(take(1).ok_or(t)?[0])?;
    let nim_amount = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let btc_amount = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let expiry_height = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let min_nim = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let max_nim = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let nim_pubkey: [u8; INTENT_PUBKEY_LEN] = take(INTENT_PUBKEY_LEN).ok_or(t)?.try_into().unwrap();
    let nim_address: [u8; NIM_ADDRESS_LEN] = take(NIM_ADDRESS_LEN).ok_or(t)?.try_into().unwrap();
    let btc_pubkey: [u8; BTC_PUBKEY_LEN] = take(BTC_PUBKEY_LEN).ok_or(t)?.try_into().unwrap();
    let network_id = take(1).ok_or(t)?[0];
    let addr_len = u16::from_be_bytes(take(2).ok_or(t)?.try_into().unwrap()) as usize;
    let btc_address = take(addr_len).ok_or(t)?.to_vec();
    let signature: [u8; INTENT_SIG_LEN] = take(INTENT_SIG_LEN).ok_or(t)?.try_into().unwrap();
    Ok(SwapIntent {
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
        network_id,
        btc_address,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(gives: Asset, nim: u64, btc: u64) -> SwapIntent {
        // A NIM-giver wants BTC by default (the historic NIM⇄BTC pairs); a counter-asset giver names
        // its own asset. `intent_wanting` overrides the NIM-giver's wanted asset for NIM⇄USDC.
        intent_wanting(
            gives,
            if gives == Asset::Nim {
                Asset::Btc
            } else {
                gives
            },
            nim,
            btc,
        )
    }

    fn intent_wanting(gives: Asset, counter_asset: Asset, nim: u64, btc: u64) -> SwapIntent {
        SwapIntent {
            gives,
            counter_asset,
            nim_amount: nim,
            btc_amount: btc,
            expiry_height: 1_000_000,
            min_nim: 0,
            max_nim: u64::MAX,
            nim_pubkey: [0x07; INTENT_PUBKEY_LEN],
            nim_address: [0xA1; NIM_ADDRESS_LEN],
            btc_pubkey: {
                let mut k = [0x11; BTC_PUBKEY_LEN];
                k[0] = 0x02;
                k
            },
            btc_address: b"tb1qalice".to_vec(),
            network_id: 5,
            signature: [0u8; INTENT_SIG_LEN],
        }
    }

    #[test]
    fn a_nim_giver_initiates_against_a_crossing_btc_giver() {
        // I give 200_000 NIM for 50_000 BTC (4.0 NIM/BTC). A BTC-giver asking 3.0 (150_000/50_000)
        // crosses → I initiate. One asking 5.0 (250_000/50_000) does not.
        let me = intent(Asset::Nim, 200_000, 50_000);
        assert!(me.would_initiate_against(&intent(Asset::Btc, 150_000, 50_000)));
        assert!(me.would_initiate_against(&intent(Asset::Btc, 200_000, 50_000))); // exact cross
        assert!(!me.would_initiate_against(&intent(Asset::Btc, 250_000, 50_000)));
    }

    #[test]
    fn a_nim_giver_initiates_against_a_crossing_usdc_giver_exactly_like_btc() {
        // NIM⇄USDC behaves identically: I give 200_000 NIM and want 100_000000 micro-USDC (100 USDC).
        // A USDC-giver asking less NIM (a better rate) crosses; one asking more does not. `btc_amount`
        // carries the counterparty amount in micro-USDC here (both sides agree on counter_asset=Usdc).
        let me = intent_wanting(Asset::Nim, Asset::Usdc, 200_000, 100_000_000);
        let usdc = |nim| intent(Asset::Usdc, nim, 100_000_000);
        assert!(me.would_initiate_against(&usdc(150_000)));
        assert!(me.would_initiate_against(&usdc(200_000))); // exact cross
        assert!(!me.would_initiate_against(&usdc(250_000)));
    }

    #[test]
    fn a_nim_giver_never_matches_the_wrong_counter_asset() {
        // Wanting BTC does NOT match a USDC giver, and wanting USDC does NOT match a BTC giver — even
        // when the raw amounts would cross. The counter_asset gate keeps the two markets separate.
        let wants_btc = intent_wanting(Asset::Nim, Asset::Btc, 200_000, 50_000);
        let wants_usdc = intent_wanting(Asset::Nim, Asset::Usdc, 200_000, 50_000);
        assert!(!wants_btc.would_initiate_against(&intent(Asset::Usdc, 150_000, 50_000)));
        assert!(!wants_usdc.would_initiate_against(&intent(Asset::Btc, 150_000, 50_000)));
        // ...but each matches its own asset.
        assert!(wants_btc.would_initiate_against(&intent(Asset::Btc, 150_000, 50_000)));
        assert!(wants_usdc.would_initiate_against(&intent(Asset::Usdc, 150_000, 50_000)));
    }

    #[test]
    fn a_usdc_giver_never_initiates() {
        // Like a BTC-giver, a USDC-giver waits for the Propose; it never initiates.
        assert!(
            !intent(Asset::Usdc, 200_000, 100_000_000).would_initiate_against(&intent_wanting(
                Asset::Nim,
                Asset::Usdc,
                150_000,
                100_000_000
            ))
        );
    }

    #[test]
    fn a_usdc_intent_round_trips_through_the_codec() {
        let mut i = intent_wanting(Asset::Usdc, Asset::Usdc, 123_456, 78_900_000);
        i.expiry_height = 4_242;
        sign_intent(&mut i, &[0x5A; 32]); // exercise pubkey + signature + both asset tags through the codec
        assert_eq!(decode_intent(&encode_intent(&i)), Ok(i));
    }

    #[test]
    fn a_btc_giver_never_initiates_and_same_side_never_matches() {
        // A BTC-giver waits for the Propose; it never initiates.
        assert!(
            !intent(Asset::Btc, 200_000, 50_000).would_initiate_against(&intent(
                Asset::Nim,
                150_000,
                50_000
            ))
        );
        // Two NIM-givers (or two BTC-givers) are not counterparties.
        assert!(
            !intent(Asset::Nim, 200_000, 50_000).would_initiate_against(&intent(
                Asset::Nim,
                150_000,
                50_000
            ))
        );
    }

    #[test]
    fn a_cross_network_or_zero_amount_intent_never_matches() {
        let mut other = intent(Asset::Btc, 150_000, 50_000);
        other.network_id = 6; // different network
        assert!(!intent(Asset::Nim, 200_000, 50_000).would_initiate_against(&other));
        // Zero amounts form no rate.
        assert!(
            !intent(Asset::Nim, 200_000, 0).would_initiate_against(&intent(
                Asset::Btc,
                150_000,
                50_000
            ))
        );
        assert!(
            !intent(Asset::Nim, 200_000, 50_000).would_initiate_against(&intent(
                Asset::Btc,
                150_000,
                0
            ))
        );
    }

    #[test]
    fn an_intent_expires_once_the_head_passes_its_expiry_height() {
        // G35: fresh up to and including the expiry height, expired after.
        let mut i = intent(Asset::Nim, 200_000, 50_000);
        i.expiry_height = 1_000;
        assert!(i.is_fresh(0)); // no beacon heard yet
        assert!(i.is_fresh(999));
        assert!(i.is_fresh(1_000)); // valid through the expiry height inclusive
        assert!(!i.is_fresh(1_001)); // head moved past it → stale
    }

    #[test]
    fn amount_compatibility_rejects_a_mis_sized_counterparty() {
        // G40: alice gives 200k NIM and will trade sizes in [50k, 500k] NIM. A counterparty wanting
        // 180k NIM (with a wide band) is compatible; one wanting 5M or a dust 40 NIM is not.
        let mut me = intent(Asset::Nim, 200_000, 50_000);
        me.min_nim = 50_000;
        me.max_nim = 500_000;
        let ok = intent(Asset::Btc, 180_000, 50_000); // 180k ∈ [50k, 500k], wide band accepts 200k
        let whale = intent(Asset::Btc, 5_000_000, 1_250_000); // 5M > 500k
        let dust = intent(Asset::Btc, 40, 10); // 40 < 50k
        assert!(me.amount_compatible(&ok));
        assert!(!me.amount_compatible(&whale));
        assert!(!me.amount_compatible(&dust));
        // It is symmetric: if the counterparty's band excludes our 200k size, we're incompatible too.
        let mut picky = intent(Asset::Btc, 180_000, 50_000);
        picky.min_nim = 1_000_000; // counterparty only wants ≥ 1M-NIM swaps
        assert!(!me.amount_compatible(&picky));
    }

    #[test]
    fn intent_round_trips_through_the_codec() {
        let mut i = intent(Asset::Btc, 123_456, 7_890);
        i.expiry_height = 4_242; // exercise the new field through the codec
        i.min_nim = 1_111;
        i.max_nim = 9_999_999; // exercise the band fields through the codec
        sign_intent(&mut i, &[0x5A; 32]); // exercise the pubkey + signature through the codec
        assert_eq!(decode_intent(&encode_intent(&i)), Ok(i));
    }

    #[test]
    fn a_signed_intent_is_authentic_but_any_forgery_is_rejected() {
        // G41: a properly-signed intent verifies; each forgery mode (wrong-key address, a tampered
        // field, a junk signature) is rejected.
        let mut authentic = intent(Asset::Btc, 180_000, 50_000);
        sign_intent(&mut authentic, &[0x42; 32]);
        assert!(authentic.verify_authentic());
        // It survives the wire round-trip and still verifies.
        assert!(decode_intent(&encode_intent(&authentic))
            .unwrap()
            .verify_authentic());

        // (a) pubkey no longer hashes to the claimed address.
        let mut wrong_addr = authentic.clone();
        wrong_addr.nim_address[0] ^= 0xFF;
        assert!(!wrong_addr.verify_authentic());

        // (b) a field changed after signing → the signature no longer covers the content.
        let mut tampered = authentic.clone();
        tampered.nim_amount += 1;
        assert!(!tampered.verify_authentic());

        // (c) a junk signature.
        let mut junk = authentic.clone();
        junk.signature = [0u8; INTENT_SIG_LEN];
        assert!(!junk.verify_authentic());
    }

    #[test]
    fn decode_rejects_short_and_bad_bytes_without_panicking() {
        assert_eq!(decode_intent(&[]), Err(IntentError::Truncated));
        assert_eq!(decode_intent(&[0xFF]), Err(IntentError::Malformed)); // bad asset tag
        assert_eq!(decode_intent(&[0x00, 0x01]), Err(IntentError::Truncated)); // ends mid-record
    }
}
