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
    /// M4 (ADR-0010): the mesh-head anchor `min_timeout`'s term units are relative to — the head the
    /// swap's terms were minted against, carried in the [`crate::swap_coordinator::SwapContext`]. A
    /// live verifier maps an on-chain wall-clock timeout back into term units against THIS anchor
    /// (`(on_chain − now) + slack + term_anchor`), so the single `timeout ≥ min_timeout` gate is the
    /// intended wall-clock floor even when real heads are in the millions. Sim verifiers ignore it.
    pub term_anchor: u64,
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

    /// C1 (money-path eligibility): whether this verifier's `observe` reads a REAL chain.
    /// **Default `false` — fail-closed:** only the gateway-backed verifiers (NIM RPC,
    /// deployed-Amoy log scans) opt in. [`crate::swap_session::SwapSession::live_safety`]
    /// refuses to pair a LIVE signer with any verifier that answers `false` here — so the
    /// sim [`AcceptAllVerifier`] (whose unconditional `Found{MAX,MAX,MAX}` would fully
    /// reopen the S1 fund-on-message theft) can never guard real funds, and a new verifier
    /// must *deliberately* declare itself chain-backed before it may.
    fn chain_backed(&self) -> bool {
        false
    }
}

/// The sim default confirmation floor — a single flat depth for paths that have no per-chain context
/// (the mock mesh has no real chain, so this is nominal). Real, chain-aware callers use
/// [`ConfirmationPolicy`] instead, which tunes the depth per chain. See #74/G3.
pub const DEFAULT_MIN_CONFIRMATIONS: u32 = 1;

/// The synthetic confirmation depth a gateway-backed USDC verifier reports for an escrow whose
/// inclusion block is at or below the chain's `finalized` tag (Polygon Heimdall v2 milestone
/// finality, live 2025-07-10, ~5 s). Deterministic milestone finality is strictly stronger than
/// ANY probabilistic depth — once a milestone signs the block there is nothing deeper to wait for —
/// so a finalized escrow is reported at `u32::MAX`, which clears [`require_funded`]'s per-chain
/// depth gate under *any* [`ConfirmationPolicy`]. This keeps the pure `require_funded` /
/// [`FundingObservation`] safety core UNCHANGED: finality is expressed as "maximally buried" in the
/// verifier, never as a new bypass in the go/no-go. The depth-count fallback still applies whenever
/// the RPC does not serve the tag (see `polygon_verifier` / `amoy_swap_verifier`). See ADR-0003
/// addendum (2026-07-15).
pub const FINALIZED_CONFIRMATIONS: u32 = u32::MAX;

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

    /// **MAINNET — FAST-FINALITY profile (default, M7 / ADR-0003 addendum 2026-07-15).** The
    /// per-chain floors calibrated to the FIRST ≤ $5 self-swaps (both sides Andjroo's own wallets,
    /// timelock-refundable either way) with the <30 s settlement goal — replacing probabilistic
    /// depth-counting with deterministic finality where a chain offers it. NOT custodial/high-value
    /// finality:
    /// - **NIM 2** — Albatross is a BFT PoS chain producing 1 s micro-blocks under a single elected
    ///   producer per slot; a micro-block reorg needs a *slashable* fork proof (equivocation), so
    ///   deep NIM reorgs are economically self-defeating. 2 blocks (~2 s) covers the realistic
    ///   1-block micro-fork with a block of margin; the timelock refund is the worst-case backstop
    ///   for a ≤ $5 leg. (This is a fast probabilistic floor, NOT macro-block finality — the batch's
    ///   macro block, the true BFT-final point, is up to ~1 min away and would blow the settlement
    ///   budget. The paranoid profile restores the old 10.)
    /// - **USDC-on-Polygon 8** — the USDC verifier's PRIMARY burial signal is the RPC `finalized`
    ///   tag (Polygon Heimdall v2 milestone finality, live 2025-07-10, ~5 s, reorgs capped at ~2
    ///   blocks); this depth-8 value is the FALLBACK for an RPC that does not serve the tag — 4× the
    ///   ~2-block reorg cap. Both signals are safe; finality is simply faster (see `polygon_verifier`
    ///   / `amoy_swap_verifier`).
    /// - **BTC 2** — unchanged. For a ≤ $5 self-swap whose timelock refund is the worst-case floor,
    ///   2 confirmations is a pragmatic small-amount burial (not the 6 a high-value BTC settlement
    ///   wants). BTC only ever gates the *unfunded* leg here anyway.
    ///
    /// These are the ≤ $5 self-swap values; a LARGER mainnet swap MUST raise them (a separate,
    /// reviewed change) — prefer [`mainnet_paranoid`](Self::mainnet_paranoid) as the deeper base.
    /// Off-by-default: nothing selects this policy until the mainnet swap path is explicitly enabled
    /// ([`crate::mainnet_swap`]).
    pub const fn mainnet_defaults() -> Self {
        ConfirmationPolicy {
            nim: 2,
            btc: 2,
            usdc: 8,
        }
    }

    /// **MAINNET — CONSERVATIVE (paranoid) profile.** The pre-fast-finality M6 depths, kept as an
    /// explicitly-named, one-line revert from [`mainnet_defaults`](Self::mainnet_defaults): NIM 10
    /// (deep micro-block burial), USDC 64 (~2 min pure depth-count, no reliance on the finality
    /// tag), BTC 2 (unchanged). Selecting this makes the money path ignore the `finalized` fast-path
    /// benefit and wait out the deeper probabilistic depths — strictly slower, never less safe. Use
    /// it to bisect a settlement-safety concern, or as the deeper base a larger-value swap raises
    /// from. Not wired by default; a reviewed change swaps `mainnet_defaults()` for this at the
    /// call site ([`crate::swap_live_ffi`]).
    pub const fn mainnet_paranoid() -> Self {
        ConfirmationPolicy {
            nim: 10,
            btc: 2,
            usdc: 64,
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

/// A HARD per-swap ceiling enforced IN CODE (never config) at the coordinator gate — the
/// `docs/MAINNET-GATING.md` §8.2 "hard per-swap cap wired". A swap PROPOSING or ACCEPTING more
/// than any of these on the money-path is refused before any real value can move, so the
/// responder's automatic funding (the §8.1 crux) can never exceed the agreed test size even if a
/// hostile/buggy counterparty asks for more.
///
/// [`SwapSession`](crate::swap_session::SwapSession) carries an optional cap (`None` on the
/// unbounded testnet/sim loop); the mainnet swap path sets [`SwapCaps::mainnet_first_swap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapCaps {
    /// Max NIM the swap may involve (luna).
    pub max_nim_luna: u64,
    /// Max USDC the counter leg may involve (micro-USDC, 6 decimals).
    pub max_usdc_micro: u64,
    /// Max BTC the counter leg may involve (satoshi).
    pub max_btc_sat: u64,
}

impl SwapCaps {
    /// The hard MAINNET first-swap ceiling: ≤ 50 NIM (5 000 000 luna) / ≤ 5 USDC (5 000 000 µUSDC)
    /// / ≤ 20 000 sat. The ≤ $5 self-swap envelope named in the guard-lift.
    pub const fn mainnet_first_swap() -> Self {
        SwapCaps {
            max_nim_luna: 50 * 100_000,
            max_usdc_micro: 5 * 1_000_000,
            max_btc_sat: 20_000,
        }
    }

    /// Whether a swap giving/taking `nim_luna` NIM against `counter_amount` of `counter` is within
    /// the caps. The NIM leg is always capped; the counter leg is capped by its chain's ceiling.
    pub fn admits(&self, nim_luna: u64, counter_amount: u64, counter: Asset) -> bool {
        if nim_luna > self.max_nim_luna {
            return false;
        }
        match counter {
            Asset::Nim => true, // a NIM/NIM trade never occurs; nothing further to cap
            Asset::Usdc => counter_amount <= self.max_usdc_micro,
            Asset::Btc => counter_amount <= self.max_btc_sat,
        }
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
#[path = "swap_funding_verify_tests.rs"]
mod tests;
