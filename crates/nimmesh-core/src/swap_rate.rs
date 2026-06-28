//! # swap_rate — the responder's exchange-rate acceptance policy (G29)
//!
//! A responder should not blindly accept any `give/take` amounts. [`RatePolicy`] is the safety floor
//! it can opt into: a minimum NIM-per-BTC rate below which it declines a proposal before spinning up
//! a coordinator. It rides the responder's [`crate::swap_session::NodeIdentity`] and is checked in
//! [`crate::swap_session::SwapSession::on_message`]. Rate selection itself (what's "fair" right now)
//! is the application's concern; this layer only enforces the floor a participant set.

/// The minimum exchange rate a responder will accept, as NIM-per-BTC = `num / den` (luna per sat).
/// The responder gives BTC (`take_amount` sat) and receives NIM (`give_amount` luna); it declines a
/// proposal whose `give/take` is below this (too little NIM for the BTC it would lock). The default
/// [`accept_all`](RatePolicy::accept_all) does no rate check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatePolicy {
    /// Numerator of the minimum NIM-per-BTC rate (luna).
    pub min_nim_per_btc_num: u64,
    /// Denominator of the minimum NIM-per-BTC rate (sat).
    pub min_nim_per_btc_den: u64,
}

impl RatePolicy {
    /// Accept any rate — the responder does no rate check (the default).
    pub const fn accept_all() -> Self {
        RatePolicy {
            min_nim_per_btc_num: 0,
            min_nim_per_btc_den: 1,
        }
    }

    /// A minimum of `num/den` NIM (luna) per BTC (sat).
    pub const fn min_rate(num: u64, den: u64) -> Self {
        RatePolicy {
            min_nim_per_btc_num: num,
            min_nim_per_btc_den: den,
        }
    }

    /// Whether a proposal giving `give_amount` luna for `take_amount` sat clears the minimum rate,
    /// i.e. `give/take >= num/den`. Cross-multiplied in `u128` (no overflow, no float). A zero
    /// `take_amount` forms no rate, so only [`accept_all`](Self::accept_all) admits that degenerate
    /// proposal.
    pub fn accepts(&self, give_amount: u64, take_amount: u64) -> bool {
        if take_amount == 0 {
            return self.min_nim_per_btc_num == 0;
        }
        (give_amount as u128) * (self.min_nim_per_btc_den as u128)
            >= (take_amount as u128) * (self.min_nim_per_btc_num as u128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_all_admits_any_proposal() {
        let p = RatePolicy::accept_all();
        assert!(p.accepts(1, 1_000_000)); // a terrible rate
        assert!(p.accepts(0, 0)); // even a degenerate one
    }

    #[test]
    fn a_min_rate_gates_on_the_cross_product() {
        // Require >= 2.5 NIM per BTC (5/2).
        let p = RatePolicy::min_rate(5, 2);
        assert!(!p.accepts(100_000, 50_000)); // 2.0 < 2.5 — declined
        assert!(p.accepts(125_000, 50_000)); // exactly 2.5 — accepted (>=)
        assert!(p.accepts(150_000, 50_000)); // 3.0 — accepted
        assert!(!p.accepts(1, 0)); // a zero-take proposal forms no rate — declined
    }

    #[test]
    fn the_cross_product_does_not_overflow_on_large_amounts() {
        // u128 math: even near-u64::MAX amounts can't overflow the comparison.
        let p = RatePolicy::min_rate(u64::MAX, 1);
        assert!(!p.accepts(u64::MAX - 1, u64::MAX)); // does not panic, declines
        assert!(p.accepts(u64::MAX, 1));
    }
}
