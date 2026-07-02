//! # swap_leg_select — which counterparty leg a matched intent settles on (P6)
//!
//! The discovery layer ([`crate::swap_intent`]) now advertises a `counter_asset` (BTC or USDC). When a
//! NIM-giver matches a counter-asset giver, the settlement layer has to build the RIGHT counterparty
//! leg: a Bitcoin P2WSH leg for BTC, or a [`crate::swap_usdc_leg::UsdcLeg`] for USDC. This is the tiny,
//! chain-agnostic SEAM that names that choice from the matched `counter_asset` — pure core code (no
//! `bitcoin-leg`/`polygon-leg` dep), so both the live engine and the sim agree on leg selection.
//!
//! The NIM leg is always the initiator's native HTLC; only the COUNTERPARTY leg varies, so this maps
//! the non-NIM `counter_asset` to its leg kind (and rejects `Nim`, which is never a counterparty).

use crate::swap_intent::Asset;

/// The kind of counterparty leg a swap settles on — selected from the matched intent's `counter_asset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterpartyLeg {
    /// A Bitcoin P2WSH HTLC leg (`swap_btc_leg::BtcSwapLeg`, behind `bitcoin-leg`).
    Btc,
    /// A USDC-on-Polygon HTLC leg (`swap_usdc_leg::UsdcLeg`, behind `polygon-leg`).
    Usdc,
}

/// The counterparty leg for a matched intent's `counter_asset`, or `None` if the asset can't be a
/// counterparty leg (`Nim` — the initiator side, never the counterparty).
pub fn counterparty_leg_for(counter_asset: Asset) -> Option<CounterpartyLeg> {
    match counter_asset {
        Asset::Btc => Some(CounterpartyLeg::Btc),
        Asset::Usdc => Some(CounterpartyLeg::Usdc),
        Asset::Nim => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_asset_selects_the_matching_leg() {
        assert_eq!(counterparty_leg_for(Asset::Btc), Some(CounterpartyLeg::Btc));
        assert_eq!(
            counterparty_leg_for(Asset::Usdc),
            Some(CounterpartyLeg::Usdc)
        );
        // NIM is the initiator's own leg, never the counterparty.
        assert_eq!(counterparty_leg_for(Asset::Nim), None);
    }
}
