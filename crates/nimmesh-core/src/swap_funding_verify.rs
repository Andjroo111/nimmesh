//! # swap_funding_verify — never fund or reveal on a message alone (finding S1 / #72, slice 1)
//!
//! The cardinal atomic-swap safety rule: before a party funds its own leg (the responder) or reveals
//! the secret by claiming the counterparty leg (the initiator), it MUST confirm the counterparty's
//! HTLC is really on-chain with the agreed terms. The initiator generates `S` and knows it from the
//! start, so without this check a malicious initiator could drive the responder to `InitiatorFunded`
//! off a mere `FundingProof` *message*, let it lock real BTC, then sweep that BTC with `S` while never
//! funding NIM — a one-sided theft. See `docs/swap/INTEGRATION-AGENDA.md` (finding S1, goal G1).
//!
//! This module is the pure decision layer, no I/O and no keys:
//!  - [`HtlcExpectation`] — what the counterparty's on-chain HTLC must satisfy (hashlock, amount,
//!    timeout floor, and the claim recipient = *this* node's key on that leg).
//!  - [`FundingVerifier`] — the sole seam to a chain. Tests use [`SimVerifier`]; slice 2 adds a
//!    gateway-backed impl (NIM RPC / BTC / Polygon) behind the same trait.
//!  - [`require_funded`] — the single go/no-go function. There is no path to `Ok` without a matching,
//!    sufficiently-funded, sufficiently-deep, correct-timeout HTLC; every refusal is an explicit
//!    [`FundingRejected`] the caller can surface and either retry (next tick) or, past the timelock,
//!    refund.
//!
//! [`crate::swap_coordinator::SwapCoordinator`] gates its funded-observed transitions on this
//! (`verify_and_observe_funding`); slice 2 routes the node/session/engine through it exclusively.

use crate::swap_intent::Asset;
use crate::swap_wire::{SwapLegId, HASH_LEN};

/// What the counterparty's on-chain HTLC must satisfy for us to safely proceed against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtlcExpectation {
    /// Which leg this HTLC settles.
    pub leg: SwapLegId,
    /// The shared hashlock `H = SHA-256(S)` — both legs commit to the same preimage.
    pub hashlock: [u8; HASH_LEN],
    /// Minimum acceptable locked amount (the agreed amount for this leg; more is fine, less is not).
    pub min_amount: u64,
    /// Minimum acceptable absolute timeout for this leg — a shorter one would shrink our claim window
    /// below what the ladder assumed, so it is rejected.
    pub min_timeout: u64,
    /// Who the HTLC must pay on claim: *this* node's own claim key/address on this leg (raw bytes).
    /// If the on-chain HTLC pays someone else, the funds are not ours to take and we must not proceed.
    pub recipient: Vec<u8>,
}

/// What a [`FundingVerifier`] found on-chain for a given [`HtlcExpectation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FundingObservation {
    /// A matching HTLC (right hashlock + right recipient) is on-chain; carries its locked amount,
    /// absolute timeout, and confirmation depth so [`require_funded`] can apply the remaining rules.
    Found {
        /// Locked amount (luna for NIM, sat for BTC, minor-units for USDC).
        amount: u64,
        /// The HTLC's absolute timeout (same unit/space as the agreed leg timeout).
        timeout: u64,
        /// How many blocks bury the funding tx (0 = in mempool / unconfirmed).
        confirmations: u32,
    },
    /// No HTLC for this hashlock+recipient is visible on-chain (yet, or never).
    Absent,
    /// An HTLC to our recipient exists but an immutable parameter is wrong (hashlock or recipient).
    Mismatch(MismatchReason),
}

/// Why an on-chain HTLC failed to match the expectation on an immutable parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchReason {
    /// The on-chain HTLC commits to a different hashlock than we agreed — `S` would not open it.
    Hashlock,
    /// The on-chain HTLC pays a recipient other than us — we could never claim it.
    Recipient,
}

/// Observe a chain for the HTLC described by an [`HtlcExpectation`]. The sole boundary between the pure
/// swap logic and a real chain: tests use [`SimVerifier`]; slice 2 adds gateway-backed impls behind
/// this same trait. Read-only — carries no keys and signs nothing.
pub trait FundingVerifier: Send + Sync {
    /// Report what is on-chain for `expect`.
    fn observe(&self, expect: &HtlcExpectation) -> FundingObservation;

    /// A2b: hand the verifier the peer's **claimed** funding tx bytes off a `FundingProof`
    /// (the session calls this before every verification attempt). Strictly a HINT, never a
    /// truth: a live verifier uses it to *locate* the funding on-chain (derive the NIM HTLC
    /// contract address from the creation bytes; anchor the Polygon log scan at the named
    /// tx's receipt — the public RPCs cap `eth_getLogs` ranges, so a blind lookback cannot
    /// work) and then verifies everything against the CHAIN in [`observe`](Self::observe).
    /// A forged wire can only make the verifier look in the wrong place, which reads
    /// `Absent` — fail-closed, exactly like no hint at all. Default: ignored (the sim and
    /// the log-scan-only verifiers need no hint).
    fn note_funding_wire(&self, _leg: SwapLegId, _tx_wire: &[u8]) {}
}

/// The sim default confirmation floor — a single flat depth for paths that have no per-chain context
/// (the mock mesh has no real chain, so this is nominal). Real, chain-aware callers use
/// [`ConfirmationPolicy`] instead, which tunes the depth per chain. See #74/G3.
pub const DEFAULT_MIN_CONFIRMATIONS: u32 = 1;

/// Per-chain minimum confirmation depth before a leg's HTLC is treated as `funded`/`settled` (#74/G3).
///
/// A single flat floor is wrong across chains: NIM's Albatross PoS reaches macro-block finality in a
/// few blocks, Bitcoin's PoW needs more burial to be reorg-safe, and Polygon PoS reorgs deeper still.
/// This policy carries one depth per chain and resolves the right one for the leg being verified, so
/// [`require_funded`] refuses a leg that is on-chain but not yet buried to *its* chain's depth — and
/// refuses it AGAIN if a reorg re-buries it shallower (the gate re-runs on every observation, holding
/// no "already funded" memory). Defaults are deliberately low **testnet** values; Phase 4 mainnet
/// gating re-tunes them (see `docs/adr/0003-confirmation-depth-reorg-policy.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmationPolicy {
    /// Depth required on the Nimiq leg.
    nim: u32,
    /// Depth required on the Bitcoin leg.
    btc: u32,
    /// Depth required on the USDC-on-Polygon leg.
    usdc: u32,
}

impl ConfirmationPolicy {
    /// Sane **testnet** per-chain defaults: NIM `2` (fast PoS finality, a couple of blocks past the
    /// funding batch), BTC `3` (moderate PoW burial for signet/testnet), USDC/Polygon `5` (PoS with
    /// deeper probabilistic reorgs). Increasing with reorg risk; low enough to keep the sim/testnet
    /// loop fast. Mainnet gating raises them (BTC→6, Polygon deeper) — never ship these to mainnet.
    pub const fn testnet_defaults() -> Self {
        ConfirmationPolicy {
            nim: 2,
            btc: 3,
            usdc: 5,
        }
    }

    /// The same depth `n` on every chain — handy for tests and for a deployment that wants one floor.
    pub const fn uniform(n: u32) -> Self {
        ConfirmationPolicy {
            nim: n,
            btc: n,
            usdc: n,
        }
    }

    /// Override the NIM-leg depth (builder style).
    pub const fn with_nim(mut self, n: u32) -> Self {
        self.nim = n;
        self
    }
    /// Override the BTC-leg depth (builder style).
    pub const fn with_btc(mut self, n: u32) -> Self {
        self.btc = n;
        self
    }
    /// Override the USDC-leg depth (builder style).
    pub const fn with_usdc(mut self, n: u32) -> Self {
        self.usdc = n;
        self
    }

    /// The required depth for a given chain.
    pub const fn required(&self, chain: Asset) -> u32 {
        match chain {
            Asset::Nim => self.nim,
            Asset::Btc => self.btc,
            Asset::Usdc => self.usdc,
        }
    }

    /// The required depth for the leg being verified. The NIM leg always uses the NIM depth; the
    /// counterparty leg uses whichever chain (`counterparty`) that swap settles on (BTC or USDC).
    /// This is the seam the coordinator/session gate calls to turn its `HtlcExpectation.leg` into a
    /// concrete `min_confirmations` for [`require_funded`].
    pub const fn required_for_leg(&self, leg: SwapLegId, counterparty: Asset) -> u32 {
        match leg {
            SwapLegId::Nim => self.nim,
            SwapLegId::Counterparty => self.required(counterparty),
        }
    }
}

impl Default for ConfirmationPolicy {
    /// Testnet defaults — an un-configured node is safe-by-default (never zero-confirmation).
    fn default() -> Self {
        ConfirmationPolicy::testnet_defaults()
    }
}

/// A verifier that accepts any funding — the mesh **sim** default, since the mock mesh has no chain to
/// observe (nodes only exchange messages). It exists to WIRE the gate into the mesh path so it is
/// enforced-by-construction: a real node overrides it with a gateway-backed verifier (NIM RPC / BTC /
/// Polygon), and the reject path is proven with a rejecting verifier in the session tests. In the sim
/// this keeps the happy path settling while the seam is exercised on every funding step.
#[derive(Debug, Clone, Default)]
pub struct AcceptAllVerifier;

impl FundingVerifier for AcceptAllVerifier {
    fn observe(&self, _expect: &HtlcExpectation) -> FundingObservation {
        FundingObservation::Found {
            amount: u64::MAX,
            timeout: u64::MAX,
            confirmations: u32::MAX,
        }
    }
}

/// The decision to proceed was refused — with the reason, so the caller surfaces it and keeps waiting
/// (retry next tick) or, past the timelock, refunds. It never silently proceeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FundingRejected {
    /// Nothing on-chain yet — wait and retry.
    NotFundedYet,
    /// On-chain but not buried deep enough for reorg safety.
    TooShallow {
        /// Depth observed.
        have: u32,
        /// Depth required.
        need: u32,
    },
    /// Locked less than agreed.
    Underfunded {
        /// Amount observed.
        have: u64,
        /// Amount required.
        need: u64,
    },
    /// Timeout shorter than agreed — our claim window would be too small.
    TimeoutTooShort {
        /// Timeout observed.
        have: u64,
        /// Timeout required.
        need: u64,
    },
    /// An immutable parameter does not match what we agreed.
    Mismatch(MismatchReason),
}

/// The heart of the safety property: turn a [`FundingObservation`] into a go/no-go. Returns the
/// confirmation depth on success. Every failure is an explicit [`FundingRejected`] — there is no way
/// to get `Ok` without a matching, sufficiently-funded, sufficiently-deep, correct-timeout HTLC.
pub fn require_funded(
    obs: &FundingObservation,
    expect: &HtlcExpectation,
    min_confirmations: u32,
) -> Result<u32, FundingRejected> {
    match obs {
        FundingObservation::Absent => Err(FundingRejected::NotFundedYet),
        FundingObservation::Mismatch(reason) => Err(FundingRejected::Mismatch(*reason)),
        FundingObservation::Found {
            amount,
            timeout,
            confirmations,
        } => {
            if *amount < expect.min_amount {
                return Err(FundingRejected::Underfunded {
                    have: *amount,
                    need: expect.min_amount,
                });
            }
            if *timeout < expect.min_timeout {
                return Err(FundingRejected::TimeoutTooShort {
                    have: *timeout,
                    need: expect.min_timeout,
                });
            }
            if *confirmations < min_confirmations {
                return Err(FundingRejected::TooShallow {
                    have: *confirmations,
                    need: min_confirmations,
                });
            }
            Ok(*confirmations)
        }
    }
}

/// A deterministic in-memory verifier: it returns a fixed [`FundingObservation`] regardless of the
/// expectation. Enough to drive the coordinator gate + the sim swap; a real gateway-backed verifier
/// (slice 2) will instead query the chain and build the observation from what it finds.
#[derive(Debug, Clone)]
pub struct SimVerifier {
    obs: FundingObservation,
}

impl SimVerifier {
    /// A verifier that always reports `obs`.
    pub fn returning(obs: FundingObservation) -> Self {
        SimVerifier { obs }
    }
    /// A verifier reporting a well-funded, deep, correct HTLC (the honest happy path).
    pub fn healthy(amount: u64, timeout: u64, confirmations: u32) -> Self {
        SimVerifier::returning(FundingObservation::Found {
            amount,
            timeout,
            confirmations,
        })
    }
}

impl FundingVerifier for SimVerifier {
    fn observe(&self, _expect: &HtlcExpectation) -> FundingObservation {
        self.obs.clone()
    }
}

/// One HTLC as it appears on a (sim) chain — the shape a gateway-backed verifier reads off a real
/// chain. Matched by `(leg, recipient, hashlock)`; `confirmations` grows as blocks bury it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnChainHtlc {
    /// Which leg this HTLC settles.
    pub leg: SwapLegId,
    /// The hashlock it commits to.
    pub hashlock: [u8; HASH_LEN],
    /// Who it pays on claim (raw key/address bytes).
    pub recipient: Vec<u8>,
    /// Locked amount.
    pub amount: u64,
    /// Absolute timeout.
    pub timeout: u64,
    /// Confirmation depth (0 = unconfirmed).
    pub confirmations: u32,
}

/// A verifier backed by an in-memory ledger of [`OnChainHtlc`]s: a faithful chain oracle for tests and
/// the sim, and the reference matching logic a gateway-backed [`FundingVerifier`] mirrors against a
/// real chain. Unlike [`SimVerifier`] (fixed answer), this actually *matches* the expectation: it finds
/// the deepest HTLC on the leg that pays **our** recipient under **our** hashlock. An HTLC that pays us
/// under a different hashlock, or our hashlock paying someone else, is a hard [`MismatchReason`];
/// nothing at all is [`FundingObservation::Absent`].
#[derive(Debug, Clone, Default)]
pub struct LedgerVerifier {
    htlcs: Vec<OnChainHtlc>,
}

impl LedgerVerifier {
    /// An empty ledger — nothing funded yet.
    pub fn new() -> Self {
        LedgerVerifier { htlcs: Vec::new() }
    }
    /// Publish (or re-publish at a deeper confirmation) an on-chain HTLC.
    pub fn fund(&mut self, htlc: OnChainHtlc) {
        self.htlcs.push(htlc);
    }

    /// Model a chain **reorg** that re-buries every funded HTLC to `confirmations` (a shallower depth
    /// than it had reached). The gate re-observes and, if the new depth is below the leg's policy,
    /// refuses again (#74/G3). Caps rather than sets, so a reorg never *deepens* a tx.
    pub fn reorg_to(&mut self, confirmations: u32) {
        for h in &mut self.htlcs {
            h.confirmations = h.confirmations.min(confirmations);
        }
    }

    /// Model a deep reorg that **orphans** the funding tx entirely (it leaves the canonical chain).
    /// The gate then sees nothing on-chain — [`FundingObservation::Absent`] — i.e. NotFundedYet.
    pub fn orphan_all(&mut self) {
        self.htlcs.clear();
    }
}

impl FundingVerifier for LedgerVerifier {
    fn observe(&self, expect: &HtlcExpectation) -> FundingObservation {
        let mut pays_us = false;
        let mut best: Option<&OnChainHtlc> = None;
        for h in self.htlcs.iter().filter(|h| h.leg == expect.leg) {
            if h.recipient == expect.recipient {
                pays_us = true;
                if h.hashlock == expect.hashlock
                    && best.map_or(true, |b| h.confirmations > b.confirmations)
                {
                    best = Some(h);
                }
            }
        }
        if let Some(h) = best {
            return FundingObservation::Found {
                amount: h.amount,
                timeout: h.timeout,
                confirmations: h.confirmations,
            };
        }
        if pays_us {
            // Pays us, but not under the hashlock we agreed — `S` would never open it.
            return FundingObservation::Mismatch(MismatchReason::Hashlock);
        }
        if self
            .htlcs
            .iter()
            .any(|h| h.leg == expect.leg && h.hashlock == expect.hashlock)
        {
            // Our hashlock is on-chain but paying someone else — not ours to take.
            return FundingObservation::Mismatch(MismatchReason::Recipient);
        }
        FundingObservation::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect() -> HtlcExpectation {
        HtlcExpectation {
            leg: SwapLegId::Nim,
            hashlock: [0x11; HASH_LEN],
            min_amount: 100_000,
            min_timeout: 10_000,
            recipient: vec![0xA1; 20],
        }
    }

    #[test]
    fn a_correct_deep_funding_is_accepted() {
        let obs = FundingObservation::Found {
            amount: 100_000,
            timeout: 10_000,
            confirmations: 6,
        };
        assert_eq!(require_funded(&obs, &expect(), 3), Ok(6));
    }

    #[test]
    fn an_overfunded_later_timeout_is_still_accepted() {
        // More amount and a longer timeout than agreed only help us — accept.
        let obs = FundingObservation::Found {
            amount: 250_000,
            timeout: 99_999,
            confirmations: 3,
        };
        assert_eq!(require_funded(&obs, &expect(), 3), Ok(3));
    }

    #[test]
    fn absent_funding_is_not_funded_yet() {
        assert_eq!(
            require_funded(&FundingObservation::Absent, &expect(), 1),
            Err(FundingRejected::NotFundedYet)
        );
    }

    #[test]
    fn a_wrong_hashlock_is_a_hard_mismatch() {
        let obs = FundingObservation::Mismatch(MismatchReason::Hashlock);
        assert_eq!(
            require_funded(&obs, &expect(), 1),
            Err(FundingRejected::Mismatch(MismatchReason::Hashlock))
        );
    }

    #[test]
    fn a_wrong_recipient_is_a_hard_mismatch() {
        let obs = FundingObservation::Mismatch(MismatchReason::Recipient);
        assert_eq!(
            require_funded(&obs, &expect(), 1),
            Err(FundingRejected::Mismatch(MismatchReason::Recipient))
        );
    }

    #[test]
    fn underfunding_is_rejected() {
        // The classic lie: an HTLC that exists but locks less than agreed.
        let obs = FundingObservation::Found {
            amount: 99_999,
            timeout: 10_000,
            confirmations: 6,
        };
        assert_eq!(
            require_funded(&obs, &expect(), 3),
            Err(FundingRejected::Underfunded {
                have: 99_999,
                need: 100_000
            })
        );
    }

    #[test]
    fn a_too_short_timeout_is_rejected() {
        // A shorter timeout than agreed would shrink our claim window below the ladder's assumption.
        let obs = FundingObservation::Found {
            amount: 100_000,
            timeout: 9_999,
            confirmations: 6,
        };
        assert_eq!(
            require_funded(&obs, &expect(), 3),
            Err(FundingRejected::TimeoutTooShort {
                have: 9_999,
                need: 10_000
            })
        );
    }

    #[test]
    fn a_shallow_funding_is_rejected_until_buried() {
        // Reorg safety: a match that is not yet deep enough must wait, not proceed.
        let obs = FundingObservation::Found {
            amount: 100_000,
            timeout: 10_000,
            confirmations: 2,
        };
        assert_eq!(
            require_funded(&obs, &expect(), 3),
            Err(FundingRejected::TooShallow { have: 2, need: 3 })
        );
    }

    #[test]
    fn amount_is_checked_before_depth() {
        // A shallow AND underfunded HTLC surfaces the economic lie first (order is deterministic).
        let obs = FundingObservation::Found {
            amount: 1,
            timeout: 10_000,
            confirmations: 0,
        };
        assert_eq!(
            require_funded(&obs, &expect(), 3),
            Err(FundingRejected::Underfunded {
                have: 1,
                need: 100_000
            })
        );
    }

    #[test]
    fn sim_verifier_healthy_passes_the_gate() {
        let v = SimVerifier::healthy(100_000, 10_000, 6);
        let obs = v.observe(&expect());
        assert_eq!(require_funded(&obs, &expect(), 3), Ok(6));
    }

    fn on_chain(recipient: Vec<u8>, hashlock: [u8; HASH_LEN], confirmations: u32) -> OnChainHtlc {
        OnChainHtlc {
            leg: SwapLegId::Nim,
            hashlock,
            recipient,
            amount: 100_000,
            timeout: 10_000,
            confirmations,
        }
    }

    #[test]
    fn ledger_absent_when_empty() {
        let ledger = LedgerVerifier::new();
        assert_eq!(ledger.observe(&expect()), FundingObservation::Absent);
    }

    #[test]
    fn ledger_finds_our_matching_htlc() {
        let mut ledger = LedgerVerifier::new();
        ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 4));
        assert_eq!(
            ledger.observe(&expect()),
            FundingObservation::Found {
                amount: 100_000,
                timeout: 10_000,
                confirmations: 4
            }
        );
    }

    #[test]
    fn ledger_reports_the_deepest_matching_htlc() {
        // The same HTLC re-published as it is buried deeper — the deepest confirmation wins.
        let mut ledger = LedgerVerifier::new();
        ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 1));
        ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 5));
        assert!(matches!(
            ledger.observe(&expect()),
            FundingObservation::Found {
                confirmations: 5,
                ..
            }
        ));
    }

    #[test]
    fn ledger_flags_an_htlc_that_pays_us_under_the_wrong_hashlock() {
        let mut ledger = LedgerVerifier::new();
        ledger.fund(on_chain(vec![0xA1; 20], [0x99; HASH_LEN], 6)); // pays us, wrong hashlock
        assert_eq!(
            ledger.observe(&expect()),
            FundingObservation::Mismatch(MismatchReason::Hashlock)
        );
    }

    #[test]
    fn ledger_flags_our_hashlock_paying_someone_else() {
        let mut ledger = LedgerVerifier::new();
        ledger.fund(on_chain(vec![0xBE; 20], [0x11; HASH_LEN], 6)); // right hashlock, not our recipient
        assert_eq!(
            ledger.observe(&expect()),
            FundingObservation::Mismatch(MismatchReason::Recipient)
        );
    }

    #[test]
    fn ledger_result_flows_through_require_funded() {
        // End to end: a deep, matching ledger entry passes the full gate; a shallow one is rejected.
        let mut ledger = LedgerVerifier::new();
        ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 2));
        assert_eq!(
            require_funded(&ledger.observe(&expect()), &expect(), 3),
            Err(FundingRejected::TooShallow { have: 2, need: 3 })
        );
        ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 3));
        assert_eq!(
            require_funded(&ledger.observe(&expect()), &expect(), 3),
            Ok(3)
        );
    }

    // ---- G3 / #74: per-chain confirmation policy + reorg re-verification ----

    #[test]
    fn confirmation_policy_testnet_defaults_are_per_chain() {
        let p = ConfirmationPolicy::testnet_defaults();
        // Per-chain depths, increasing with reorg risk / finality uncertainty (see the ADR).
        assert_eq!(p.required(Asset::Nim), 2);
        assert_eq!(p.required(Asset::Btc), 3);
        assert_eq!(p.required(Asset::Usdc), 5);
        // Default == the testnet defaults, so an un-configured node is safe-by-default.
        assert_eq!(ConfirmationPolicy::default(), p);
    }

    #[test]
    fn confirmation_policy_required_for_leg_resolves_counterparty_chain() {
        let p = ConfirmationPolicy::testnet_defaults();
        // The NIM leg always uses the NIM depth, whatever the counterparty chain is.
        assert_eq!(p.required_for_leg(SwapLegId::Nim, Asset::Btc), 2);
        assert_eq!(p.required_for_leg(SwapLegId::Nim, Asset::Usdc), 2);
        // The counterparty leg uses *that* chain's depth — BTC and USDC differ.
        assert_eq!(p.required_for_leg(SwapLegId::Counterparty, Asset::Btc), 3);
        assert_eq!(p.required_for_leg(SwapLegId::Counterparty, Asset::Usdc), 5);
    }

    #[test]
    fn confirmation_policy_uniform_and_builder_override() {
        assert_eq!(ConfirmationPolicy::uniform(4).required(Asset::Btc), 4);
        let p = ConfirmationPolicy::testnet_defaults().with_btc(6);
        assert_eq!(p.required(Asset::Btc), 6);
        assert_eq!(p.required(Asset::Nim), 2); // other chains unchanged
    }

    #[test]
    fn shallow_then_deep_advances_only_when_buried_at_policy_depth() {
        // Depth < policy never advances; the same HTLC, once buried to the policy depth, passes.
        let need = ConfirmationPolicy::testnet_defaults().required(Asset::Nim);
        assert!(need >= 2, "test assumes NIM depth > 1");
        let htlc = |confs| OnChainHtlc {
            leg: SwapLegId::Nim,
            hashlock: [0x11; HASH_LEN],
            recipient: vec![0xA1; 20],
            amount: 100_000,
            timeout: 10_000,
            confirmations: confs,
        };
        let mut ledger = LedgerVerifier::new();
        ledger.fund(htlc(need - 1)); // one block short of the policy depth
        assert_eq!(
            require_funded(&ledger.observe(&expect()), &expect(), need),
            Err(FundingRejected::TooShallow {
                have: need - 1,
                need
            })
        );
        ledger.fund(htlc(need)); // buried to the policy depth
        assert_eq!(
            require_funded(&ledger.observe(&expect()), &expect(), need),
            Ok(need)
        );
    }

    #[test]
    fn a_reorg_below_policy_depth_refuses_again() {
        // Re-verify on reorg (#74/G3): an HTLC buried deep enough to pass can be re-orged shallower;
        // the same gate must refuse it AGAIN — no funded transition may rest on a leg that reorged
        // below its policy depth.
        let need = ConfirmationPolicy::testnet_defaults().required(Asset::Nim);
        assert!(need >= 2, "test assumes NIM depth > 1");
        let mut ledger = LedgerVerifier::new();
        ledger.fund(OnChainHtlc {
            leg: SwapLegId::Nim,
            hashlock: [0x11; HASH_LEN],
            recipient: vec![0xA1; 20],
            amount: 100_000,
            timeout: 10_000,
            confirmations: need + 4, // comfortably buried — passes now
        });
        assert!(require_funded(&ledger.observe(&expect()), &expect(), need).is_ok());

        // A reorg re-buries the funding tx below the policy depth → the gate refuses it again.
        ledger.reorg_to(need - 1);
        assert_eq!(
            require_funded(&ledger.observe(&expect()), &expect(), need),
            Err(FundingRejected::TooShallow {
                have: need - 1,
                need
            })
        );
    }

    #[test]
    fn a_reorg_that_orphans_the_funding_tx_reads_as_absent() {
        // A deep reorg can orphan the funding tx entirely (depth → gone). The gate then sees nothing
        // on-chain, i.e. NotFundedYet — the honest "wait / eventually refund" path, never an advance.
        let mut ledger = LedgerVerifier::new();
        ledger.fund(OnChainHtlc {
            leg: SwapLegId::Nim,
            hashlock: [0x11; HASH_LEN],
            recipient: vec![0xA1; 20],
            amount: 100_000,
            timeout: 10_000,
            confirmations: 8,
        });
        assert!(matches!(
            ledger.observe(&expect()),
            FundingObservation::Found { .. }
        ));
        ledger.orphan_all();
        assert_eq!(ledger.observe(&expect()), FundingObservation::Absent);
        assert_eq!(
            require_funded(&ledger.observe(&expect()), &expect(), 1),
            Err(FundingRejected::NotFundedYet)
        );
    }
}
