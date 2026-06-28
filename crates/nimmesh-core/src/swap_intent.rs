//! # swap_intent — counterparty discovery over the mesh (G34)
//!
//! A swap needs two parties; today they have to already know each other. A [`SwapIntent`] is a
//! lightweight **advertisement** a node floods ("I want to give NIM for BTC at rate R", with my
//! addresses) — no hashlock, no secret, no commitment. A node holding the **complementary** intent
//! (it wants the mirror trade at a crossing rate) reacts by kicking off a real `SwapPropose`: the
//! intent is the discovery layer, the existing swap protocol is the settlement layer.
//!
//! By convention the **NIM-giver is the initiator** (it generates `S` and proposes; it claims the
//! BTC leg, revealing `S`). So matching is one-sided: a node whose standing intent gives NIM, on
//! seeing a BTC-giver intent that crosses on rate, initiates; a BTC-giver just waits for the Propose
//! (its [`crate::swap_rate::RatePolicy`] then governs acceptance). The intent rides the mesh as
//! opaque blind-relayed bytes like every other swap packet.

use crate::swap_wire::{BTC_PUBKEY_LEN, NIM_ADDRESS_LEN};

/// Which asset the advertiser **funds** in the trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asset {
    /// The advertiser gives NIM (so it is the would-be initiator).
    Nim,
    /// The advertiser gives BTC (so it is the would-be responder).
    Btc,
}

/// A discovery advertisement: the trade a node wants, plus the addresses a matcher needs to propose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapIntent {
    /// Which side the advertiser funds.
    pub gives: Asset,
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
    /// The advertiser's NIM address (20 raw bytes).
    pub nim_address: [u8; NIM_ADDRESS_LEN],
    /// The advertiser's BTC claimant pubkey (33 bytes).
    pub btc_pubkey: [u8; BTC_PUBKEY_LEN],
    /// The advertiser's BTC payout address bytes.
    pub btc_address: Vec<u8>,
    /// The Albatross network id (a swap only forms within one network).
    pub network_id: u8,
}

impl SwapIntent {
    /// Whether **this** node (holding `self` as its standing intent) should INITIATE a swap in
    /// response to `incoming`. True only when: `self` gives NIM and `incoming` gives BTC (so this
    /// node is the NIM-giver / initiator), the networks match, both amounts are non-zero, and the
    /// rates cross — `self`'s NIM-per-BTC is at least what `incoming` asks, i.e.
    /// `self.nim/self.btc >= incoming.nim/incoming.btc`, cross-multiplied to avoid float / overflow.
    /// A BTC-giver never initiates (it waits for the Propose), so this is one-sided by design.
    pub fn would_initiate_against(&self, incoming: &SwapIntent) -> bool {
        self.gives == Asset::Nim
            && incoming.gives == Asset::Btc
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
}

/// A decode failure (carries no payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentError {
    /// The bytes ended early.
    Truncated,
    /// A field was outside its domain (bad asset tag).
    Malformed,
}

/// Encode an intent to bytes (flooded as a `SwapIntent` packet payload).
pub fn encode_intent(intent: &SwapIntent) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(match intent.gives {
        Asset::Nim => 0,
        Asset::Btc => 1,
    });
    out.extend_from_slice(&intent.nim_amount.to_be_bytes());
    out.extend_from_slice(&intent.btc_amount.to_be_bytes());
    out.extend_from_slice(&intent.expiry_height.to_be_bytes());
    out.extend_from_slice(&intent.min_nim.to_be_bytes());
    out.extend_from_slice(&intent.max_nim.to_be_bytes());
    out.extend_from_slice(&intent.nim_address);
    out.extend_from_slice(&intent.btc_pubkey);
    out.push(intent.network_id);
    out.extend_from_slice(&(intent.btc_address.len() as u16).to_be_bytes());
    out.extend_from_slice(&intent.btc_address);
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
    let gives = match take(1).ok_or(t)?[0] {
        0 => Asset::Nim,
        1 => Asset::Btc,
        _ => return Err(IntentError::Malformed),
    };
    let nim_amount = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let btc_amount = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let expiry_height = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let min_nim = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let max_nim = u64::from_be_bytes(take(8).ok_or(t)?.try_into().unwrap());
    let nim_address: [u8; NIM_ADDRESS_LEN] = take(NIM_ADDRESS_LEN).ok_or(t)?.try_into().unwrap();
    let btc_pubkey: [u8; BTC_PUBKEY_LEN] = take(BTC_PUBKEY_LEN).ok_or(t)?.try_into().unwrap();
    let network_id = take(1).ok_or(t)?[0];
    let addr_len = u16::from_be_bytes(take(2).ok_or(t)?.try_into().unwrap()) as usize;
    let btc_address = take(addr_len).ok_or(t)?.to_vec();
    Ok(SwapIntent {
        gives,
        nim_amount,
        btc_amount,
        expiry_height,
        min_nim,
        max_nim,
        nim_address,
        btc_pubkey,
        network_id,
        btc_address,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(gives: Asset, nim: u64, btc: u64) -> SwapIntent {
        SwapIntent {
            gives,
            nim_amount: nim,
            btc_amount: btc,
            expiry_height: 1_000_000,
            min_nim: 0,
            max_nim: u64::MAX,
            nim_address: [0xA1; NIM_ADDRESS_LEN],
            btc_pubkey: {
                let mut k = [0x11; BTC_PUBKEY_LEN];
                k[0] = 0x02;
                k
            },
            btc_address: b"tb1qalice".to_vec(),
            network_id: 5,
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
        assert_eq!(decode_intent(&encode_intent(&i)), Ok(i));
    }

    #[test]
    fn decode_rejects_short_and_bad_bytes_without_panicking() {
        assert_eq!(decode_intent(&[]), Err(IntentError::Truncated));
        assert_eq!(decode_intent(&[0xFF]), Err(IntentError::Malformed)); // bad asset tag
        assert_eq!(decode_intent(&[0x00, 0x01]), Err(IntentError::Truncated)); // ends mid-record
    }
}
