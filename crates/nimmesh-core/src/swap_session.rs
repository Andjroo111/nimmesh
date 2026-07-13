//! # swap_session — route incoming swap packets to the right coordinator (the node↔coordinator glue)
//!
//! A node may have several swaps in flight. [`SwapSession`] owns one [`SwapCoordinator`] per swap,
//! keyed by `swap_id`, and routes each incoming swap packet to the right one — creating a responder
//! coordinator when a fresh `Propose` arrives, and returning the outgoing envelopes to flood. It is
//! the thin layer a [`crate::node::MeshNode`] sits behind: packets in, coordinator dispatch,
//! envelopes out. The chain actions a coordinator implies (fund / claim / extract `S`) stay with the
//! node — it reaches the coordinator via [`coordinator`](SwapSession::coordinator). No bitcoin, and no
//! raw key material: the only key seam it holds is an opaque [`EnclaveKey`] used to *sign* (not expose)
//! an outgoing Propose (S2 / #73) — the seed stays behind the enclave, only signed bytes leave.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::nimiq::signer::EnclaveKey;
use crate::packet::MessageType;
use crate::swap::LadderParams;
use crate::swap_coordinator::{CoordError, SwapContext, SwapCoordinator};
use crate::swap_funding_verify::{
    AcceptAllVerifier, ConfirmationPolicy, FundingVerifier, SwapCaps,
};
use crate::swap_intent::Asset;
use crate::swap_messages::{SwapProposal, PROPOSE_PUBKEY_LEN, PROPOSE_SIG_LEN};
use crate::swap_wire::{
    decode_swap, encode_swap, SwapEnvelope, SwapKind, SwapWireError, BTC_PUBKEY_LEN,
    NIM_ADDRESS_LEN, SWAP_ID_LEN,
};

pub use crate::swap_rate::RatePolicy;

/// G30: the default cap on how many swaps a participant holds at once — a spammer can't make a node
/// hold more than this (the GC reaps the stale ones, freeing slots). A phone realistically runs a
/// handful of swaps; a generous default that still bounds memory against a `Propose` flood.
pub const DEFAULT_MAX_CONCURRENT_SWAPS: usize = 16;

/// This node's fixed identity (everything a coordinator needs that isn't learned from the peer),
/// plus the responder policies this node brings to a swap (rate floor + concurrency cap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// This node's NIM address (20 raw bytes).
    pub nim_address: [u8; NIM_ADDRESS_LEN],
    /// This node's BTC payout address bytes.
    pub btc_address: Vec<u8>,
    /// This node's BTC pubkey (33 bytes).
    pub btc_pubkey: [u8; BTC_PUBKEY_LEN],
    /// The minimum exchange rate this node accepts as a responder (default: [`RatePolicy::accept_all`]).
    pub rate_policy: RatePolicy,
    /// Max swaps this node will hold at once; a fresh `Propose` past it is dropped (anti-DoS,
    /// default [`DEFAULT_MAX_CONCURRENT_SWAPS`]). Own swaps via `add_initiator` are uncapped.
    pub max_concurrent_swaps: usize,
    /// G34: this node's standing discovery intent (the trade it wants). When set and a complementary
    /// intent crosses on rate, the node initiates a swap. `None` = not advertising for a counterparty.
    pub standing_intent: Option<crate::swap_intent::SwapIntent>,
}

/// A routing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The swap envelope didn't decode.
    Wire(SwapWireError),
    /// The target coordinator rejected the message.
    Coord(CoordError),
    /// A message arrived for a `swap_id` we have no coordinator for.
    UnknownSwap,
    /// A `Propose` was missing fields needed to set up the responder.
    BadPropose,
}

impl From<SwapWireError> for SessionError {
    fn from(e: SwapWireError) -> Self {
        SessionError::Wire(e)
    }
}
impl From<CoordError> for SessionError {
    fn from(e: CoordError) -> Self {
        SessionError::Coord(e)
    }
}

/// How many maintenance ticks a node keeps re-flooding a swap's last action before giving up — the
/// retransmit budget that lets a swap survive a lossy mesh (G20). Generous: a healthy mesh advances
/// the swap (resetting the budget) within a tick or two; a very lossy one gets many retries.
pub(crate) const RETRANSMIT_TTL: u8 = 32;

/// G11: the per-swap secret drawer a session holds (see `SwapSession::with_secret_source`).
pub type SecretSource = Box<dyn Fn(&[u8; SWAP_ID_LEN]) -> [u8; 32] + Send + Sync>;

/// The last action a node emitted for a swap, buffered for retransmit. Decoupled from the
/// coordinator's lifetime so a Settled initiator keeps re-flooding its `PreimageReveal` (the mesh's
/// stand-in for the responder reading `S` off the on-chain claim) for a bounded window after it
/// settled.
struct PendingAction {
    msg_type: MessageType,
    payload: Vec<u8>,
    ttl: u8,
}

/// One node's view of all its in-flight swaps.
pub struct SwapSession {
    pub(crate) identity: NodeIdentity,
    pub(crate) ladder: LadderParams,
    pub(crate) coordinators: HashMap<[u8; SWAP_ID_LEN], SwapCoordinator>,
    /// #189 (money-path): every `swap_id` this node has EVER initiated as the NIM-giver.
    /// The secret source is a pure function of `swap_id`, so re-initiating one would reissue
    /// an already-revealed `S` (a settled swap published it) — the counterparty could then
    /// claim the fresh HTLC with public data, no escrow. A permanent tombstone: an id here
    /// is never initiated again. A legitimate repeat trade re-advertises under a fresh
    /// ephemeral identity (G45), which yields a fresh `swap_id`.
    pub(crate) initiated_ever: HashSet<[u8; SWAP_ID_LEN]>,
    /// G20: per-swap last-emitted action, re-flooded each tick (TTL-bounded) to recover a message
    /// dropped over a lossy mesh.
    pending: HashMap<[u8; SWAP_ID_LEN], PendingAction>,
    /// S1 / #72: the on-chain funding verifier this node consults before a `FundingProof` may advance
    /// a coordinator to a funded phase (so the responder never funds, and the initiator never reveals
    /// `S`, on a message alone). Defaults to [`AcceptAllVerifier`] — the mesh sim has no chain to
    /// observe, so the seam is wired but nominal; a real node injects a gateway-backed verifier.
    verifier: Box<dyn FundingVerifier>,
    /// S2 / #73: this node's NIM identity key, used to *authenticate* an outgoing `Propose` (sign its
    /// [`SwapProposal::signing_bytes`]). Only a signature and public key ever leave the enclave — the
    /// seed never crosses this seam. `None` on a responder-only node (it never originates a Propose);
    /// an initiating node injects the key that owns its `identity.nim_address`.
    propose_signer: Option<Arc<dyn EnclaveKey>>,
    /// #74 / G3: per-chain confirmation-depth policy. A `FundingProof` only advances a coordinator
    /// once the counterparty's HTLC is buried to *its chain's* depth (NIM shallower than BTC/USDC),
    /// and a reorg that re-buries it shallower stalls it again. Defaults to the testnet policy.
    confirm_policy: ConfirmationPolicy,
    /// #74 / G3: which chain this node's mesh swaps settle on the counterparty side (BTC today — USDC
    /// swaps drive `SwapEngine` directly, not this session path). Selects the counterparty-leg depth
    /// from `confirm_policy`. Defaults to [`Asset::Btc`].
    counterparty_chain: Asset,
    /// G11: draws the initiator secret `S` for a swap this node originates. Default =
    /// deterministic `sim_secret` (tests); production injects a CSPRNG-backed PRF via
    /// [`SwapSession::with_secret_source`].
    pub(crate) secret_source: SecretSource,
    /// C1: whether `secret_source` is still the deterministic sim default. Flipped false by
    /// [`with_secret_source`](Self::with_secret_source); [`live_safety`](Self::live_safety)
    /// refuses a live pairing while it is true (a public-derivable `S` loses both legs).
    secret_is_sim: bool,
    /// §8.2 hard per-swap cap. `None` = unbounded (the testnet/sim loop); the mainnet swap path
    /// sets [`SwapCaps::mainnet_first_swap`], and the coordinator gate then REFUSES to originate
    /// (initiator) or accept (responder) a swap above it — so real value can never exceed the
    /// agreed ≤ $5 test size even if a counterparty asks for more.
    caps: Option<SwapCaps>,
}

impl SwapSession {
    /// A session for `identity` with the given safety ladder.
    pub fn new(identity: NodeIdentity, ladder: LadderParams) -> Self {
        SwapSession {
            identity,
            ladder,
            coordinators: HashMap::new(),
            initiated_ever: HashSet::new(),
            pending: HashMap::new(),
            verifier: Box::new(AcceptAllVerifier),
            propose_signer: None,
            confirm_policy: ConfirmationPolicy::testnet_defaults(),
            counterparty_chain: Asset::Btc,
            secret_source: Box::new(crate::swap_node::sim_secret),
            secret_is_sim: true,
            caps: None,
        }
    }

    /// §8.2: set a HARD per-swap cap the coordinator gate refuses above (propose + accept). The
    /// mainnet swap path wires [`SwapCaps::mainnet_first_swap`]; the testnet/sim loop leaves it
    /// unset (unbounded). Enforced in code, not config — a hostile/buggy peer asking for more than
    /// the cap gets no Accept (and this node never originates above it).
    pub fn with_caps(mut self, caps: SwapCaps) -> Self {
        self.caps = Some(caps);
        self
    }

    /// Whether a swap of `nim_luna` NIM ⇄ `counter_amount` of this session's counterparty asset is
    /// within the hard per-swap cap. `true` when no cap is configured (unbounded testnet/sim).
    pub(crate) fn within_caps(&self, nim_luna: u64, counter_amount: u64) -> bool {
        match &self.caps {
            None => true,
            Some(caps) => caps.admits(nim_luna, counter_amount, self.counterparty_chain),
        }
    }

    /// G11: inject the PRODUCTION per-swap secret source (a CSPRNG-backed PRF). The default is
    /// the deterministic `sim_secret` — reproducible for the no-RNG test suite, and loudly
    /// marked never-for-real-funds. A live participant MUST replace it before any real-money
    /// signer is wired.
    pub fn with_secret_source(mut self, source: SecretSource) -> Self {
        self.secret_source = source;
        self.secret_is_sim = false;
        self
    }

    /// C1 (the money-path construction gate): whether this session is safe to pair with a
    /// LIVE [`crate::swap_signer::SwapSigner`]. Every live-unsafe default must have been
    /// replaced: the funding verifier must be chain-backed (never the sim
    /// [`AcceptAllVerifier`], whose unconditional `Found` reopens the S1 fund-on-message
    /// theft), the secret source must not be the deterministic sim stand-in (its `S` is
    /// public-derivable), and the per-chain confirmation floors must be non-zero.
    /// [`crate::node::MeshNode`] refuses to build a live-signer node that fails this — the
    /// safety no longer rests on remembering `.with_*()` calls.
    pub fn live_safety(&self) -> Result<(), &'static str> {
        if !self.verifier.chain_backed() {
            return Err("funding verifier is not chain-backed (sim/accept-all)");
        }
        if self.secret_is_sim {
            return Err("secret source is the deterministic sim stand-in");
        }
        if self
            .confirm_policy
            .required_for_leg(crate::swap_wire::SwapLegId::Nim, self.counterparty_chain)
            == 0
            || self.confirm_policy.required_for_leg(
                crate::swap_wire::SwapLegId::Counterparty,
                self.counterparty_chain,
            ) == 0
        {
            return Err("confirmation policy allows zero-confirmation funding");
        }
        Ok(())
    }

    /// Inject this node's NIM identity key so it signs each `Propose` it originates (S2 / #73). The
    /// key must own `identity.nim_address` (its public key hashes to it), so a responder can
    /// authenticate the proposed terms via [`SwapProposal::verify_envelope`]. Only signed bytes + the
    /// public key leave the enclave; the seed never does.
    pub fn with_propose_signer(mut self, key: Arc<dyn EnclaveKey>) -> Self {
        self.propose_signer = Some(key);
        self
    }

    /// S2 / #73: authenticate an outgoing initiator `Propose` by signing its terms with this node's
    /// NIM enclave key, returning the wire-signed envelope. If this node carries no propose signer (a
    /// responder-only node never originates) — or the enclave returns malformed key material — the
    /// envelope is returned unchanged, and an enforcing responder will reject it as unauthenticated.
    pub(crate) fn sign_propose(&self, propose: &SwapEnvelope) -> SwapEnvelope {
        let Some(key) = self.propose_signer.as_ref() else {
            return propose.clone();
        };
        let Some(proposal) = SwapProposal::from_envelope(propose) else {
            return propose.clone();
        };
        let (Ok(pubkey), Ok(signature)) = (
            <[u8; PROPOSE_PUBKEY_LEN]>::try_from(key.public_key()),
            <[u8; PROPOSE_SIG_LEN]>::try_from(key.sign_content(proposal.signing_bytes())),
        ) else {
            return propose.clone();
        };
        proposal.to_signed_envelope(pubkey, signature)
    }

    /// Inject the on-chain funding verifier (S1 / #72). A real node passes a gateway-backed verifier
    /// so a `FundingProof` only advances a coordinator once the counterparty's HTLC is confirmed
    /// on-chain; the default is [`AcceptAllVerifier`] (the sim has no chain to observe).
    pub fn with_funding_verifier(mut self, verifier: Box<dyn FundingVerifier>) -> Self {
        self.verifier = verifier;
        self
    }

    /// Set the per-chain confirmation-depth policy (#74 / G3). A real node tunes how deep each chain's
    /// HTLC must be buried before it funds/reveals against it; the default is
    /// [`ConfirmationPolicy::testnet_defaults`]. A reorg that re-buries a leg below its depth stalls
    /// the swap again (re-verified on every `FundingProof`).
    pub fn with_confirmation_policy(mut self, policy: ConfirmationPolicy) -> Self {
        self.confirm_policy = policy;
        self
    }

    /// Set which chain this node's mesh swaps settle on the counterparty side (#74 / G3) — picks the
    /// counterparty-leg depth from the confirmation policy. Defaults to [`Asset::Btc`].
    pub fn with_counterparty_chain(mut self, chain: Asset) -> Self {
        self.counterparty_chain = chain;
        self
    }

    /// Register an initiator coordinator this node started, so its `Accept`/`FundingProof` route to it.
    pub fn add_initiator(&mut self, swap_id: [u8; SWAP_ID_LEN], coordinator: SwapCoordinator) {
        self.initiated_ever.insert(swap_id); // #189: never reissue this swap's secret
        self.coordinators.insert(swap_id, coordinator);
    }

    /// #189: has this node ever initiated `swap_id` (so its `S` may be public)? A repeat
    /// initiation is refused — see [`initiated_ever`](Self::initiated_ever).
    pub(crate) fn has_initiated(&self, swap_id: &[u8; SWAP_ID_LEN]) -> bool {
        self.initiated_ever.contains(swap_id)
    }

    /// The coordinator for a swap, for the node to drive its chain actions (fund / claim / reveal).
    pub fn coordinator(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> Option<&mut SwapCoordinator> {
        self.coordinators.get_mut(swap_id)
    }

    /// The number of in-flight swaps.
    pub fn len(&self) -> usize {
        self.coordinators.len()
    }

    /// Whether there are no in-flight swaps.
    pub fn is_empty(&self) -> bool {
        self.coordinators.is_empty()
    }

    /// A snapshot of every in-flight swap's `(swap_id, phase)` — the node mirrors this into shared
    /// state so the FFI side can observe a swap's progress without reaching into the worker thread.
    pub fn phases(&self) -> Vec<([u8; SWAP_ID_LEN], crate::swap::SwapPhase)> {
        self.coordinators
            .iter()
            .map(|(id, c)| (*id, c.phase()))
            .collect()
    }

    /// Route an incoming swap packet (`kind` from the [`MessageType`], `payload` the TLV bytes) to
    /// the right coordinator. Returns outgoing `(MessageType, bytes)` envelopes to flood. A fresh
    /// `Propose` spins up a responder coordinator; `Accept`/`FundingProof` dispatch to the existing
    /// one; `Abort` drops it. `PreimageReveal` is routed (the node extracts `S` and drives the
    /// claim itself via [`coordinator`](Self::coordinator)).
    pub fn on_message(
        &mut self,
        kind: SwapKind,
        payload: &[u8],
        head: u64,
    ) -> Result<Vec<(MessageType, Vec<u8>)>, SessionError> {
        // A malformed payload is dropped here (panic-free decode of untrusted mesh bytes) — no
        // coordinator is touched, so a hostile frame can neither create nor corrupt state.
        let env = decode_swap(kind, payload)?;
        let swap_id = env.swap_id;
        match kind {
            SwapKind::Propose => {
                // Replay / collision guard: a `Propose` for a `swap_id` we already track must NEVER
                // clobber the in-flight coordinator (whether one we initiated or an accepted
                // responder). Drop the duplicate with no reply — the live swap is untouched.
                if self.coordinators.contains_key(&swap_id) {
                    return Ok(Vec::new());
                }
                // Concurrency cap (anti-DoS): a `Propose`-spammer can't make us hold more than the
                // cap at once. Drop a fresh Propose when full (the GC reaps stale ones, freeing
                // slots). Own swaps (`add_initiator`) bypass this.
                if self.coordinators.len() >= self.identity.max_concurrent_swaps {
                    return Ok(Vec::new());
                }
                let p = SwapProposal::from_envelope(&env).ok_or(SessionError::BadPropose)?;
                // Rate gate: decline a lopsided proposal before spinning up a responder. We get
                // `give_amount` NIM for the `take_amount` BTC we'd lock; if that rate is below our
                // floor, drop it silently (no coordinator, no Accept).
                if !self
                    .identity
                    .rate_policy
                    .accepts(p.give_amount, p.take_amount)
                {
                    return Ok(Vec::new());
                }
                // §8.2 hard per-swap cap: the responder funds a leg AUTOMATICALLY, so it must
                // never accept a swap above the agreed test size even if the proposer asks for
                // more. Refuse silently (no coordinator, no Accept), exactly like the rate gate.
                if !self.within_caps(p.give_amount, p.take_amount) {
                    return Ok(Vec::new());
                }
                let ctx = SwapContext {
                    swap_id,
                    terms: p.terms,
                    hashlock: p.hashlock,
                    nim_address: self.identity.nim_address,
                    btc_address: self.identity.btc_address.clone(),
                    btc_pubkey: self.identity.btc_pubkey,
                    give_amount: p.give_amount,
                    take_amount: p.take_amount,
                    network_id: p.network_id,
                    // M4: OUR head is the anchor the live timeout mappings subtract — the
                    // initiator minted the terms against its own (beacon-synced) head.
                    term_anchor: head,
                };
                let mut coord = SwapCoordinator::new_responder(ctx, self.ladder);
                // Gate BEFORE inserting: a rejected `Propose` (unsafe ladder, mismatched fields)
                // leaves no half-built coordinator stranded in the map.
                let accept = coord.recv_propose(&env, head)?;
                self.coordinators.insert(swap_id, coord);
                Ok(vec![(MessageType::SwapAccept, encode_swap(&accept)?)])
            }
            SwapKind::Accept => {
                self.route(&swap_id)?.recv_accept(&env, head)?;
                Ok(vec![])
            }
            SwapKind::FundingProof => {
                // S1 / #72: a `FundingProof` message no longer advances the swap by itself — the node
                // first confirms the counterparty's HTLC on-chain via its verifier. It advances only on
                // a verified funding; otherwise it stays put (a not-yet-confirmed funding retries next
                // tick; a real mismatch/underfunding stalls to a timeout refund). The verifier and the
                // coordinators map are disjoint fields, so we borrow both directly (not via `route`).
                let coord = self
                    .coordinators
                    .get_mut(&swap_id)
                    .ok_or(SessionError::UnknownSwap)?;
                // A2b: hand the proof's claimed funding wire to the verifier as an UNTRUSTED
                // locating hint (a live NIM verifier derives the HTLC contract address from it;
                // the Polygon verifier anchors its capped log scan at the named tx). Scoped to
                // swaps we actually track; the chain stays the sole truth in `observe`.
                if let (Some(leg), Some(wire)) = (env.leg, env.tx_wire.as_deref()) {
                    self.verifier.note_funding_wire(leg, wire);
                }
                // #74 / G3: verify at the counterparty leg's *own* chain depth, not one flat floor —
                // the responder checks the initiator's NIM leg (NIM depth), the initiator checks the
                // counterparty leg (BTC/USDC depth). Fields are disjoint from `coord`.
                let leg = coord.counterparty_expectation().leg;
                let min_confirmations = self
                    .confirm_policy
                    .required_for_leg(leg, self.counterparty_chain);
                coord.verify_and_observe_funding(self.verifier.as_ref(), min_confirmations)?;
                Ok(vec![])
            }
            SwapKind::PreimageReveal => {
                // The node extracts S from the carried BTC claim and drives the NIM claim itself.
                self.route(&swap_id)?; // just validate we know this swap
                Ok(vec![])
            }
            SwapKind::Abort => {
                // The counterparty bailed: free the slot — but ONLY if WE have no funds locked. A
                // counterparty abort must never drop our funded coordinator; that leg still has to
                // refund via the timeout path (a dropped coordinator would strand the refund). A
                // stray Abort for an unknown swap is a harmless no-op.
                if self
                    .coordinators
                    .get(&swap_id)
                    .is_some_and(|c| !c.phase().has_funds_locked())
                {
                    self.coordinators.remove(&swap_id);
                    self.pending.remove(&swap_id); // torn down — stop retransmitting.
                }
                Ok(vec![])
            }
        }
    }

    fn route(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> Result<&mut SwapCoordinator, SessionError> {
        self.coordinators
            .get_mut(swap_id)
            .ok_or(SessionError::UnknownSwap)
    }

    /// Drop every coordinator that has reached a terminal phase (Settled / Aborted / Refunded),
    /// freeing its slot. Returns how many were reaped. The node calls this after it settles a swap;
    /// the session GC tick (G16) calls it to keep a node from being memory-exhausted by stale swaps.
    pub fn reap_terminal(&mut self) -> usize {
        let before = self.coordinators.len();
        self.coordinators.retain(|_, c| !c.phase().is_terminal());
        before - self.coordinators.len()
    }

    /// Forget a swap (e.g. the node aborted it locally), freeing its slot. Returns whether one was
    /// present.
    pub fn remove(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> bool {
        self.coordinators.remove(swap_id).is_some()
    }

    /// Locally cancel an **un-funded** swap: drop it and return a `SwapAbort` envelope to flood so
    /// the counterparty frees its slot too (symmetric teardown). Returns `None` — and changes
    /// nothing — if the swap is unknown or holds funds: a funded swap must **refund** via the
    /// timeout path, never abort (aborting would strand the counterparty's funded leg).
    pub fn cancel(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> Option<SwapEnvelope> {
        let c = self.coordinators.get(swap_id)?;
        if c.phase().has_funds_locked() {
            return None;
        }
        self.coordinators.remove(swap_id);
        self.pending.remove(swap_id);
        Some(crate::swap_messages::abort(*swap_id, 0))
    }

    /// G20: remember the last action this node emitted for a swap, so the node re-floods it on the
    /// maintenance tick until the swap advances — recovering a `Propose`/`Accept`/`FundingProof`/
    /// `PreimageReveal` dropped over a lossy mesh. Replacing the action resets its retransmit budget.
    pub fn record_action(
        &mut self,
        swap_id: [u8; SWAP_ID_LEN],
        msg_type: MessageType,
        payload: Vec<u8>,
    ) {
        self.pending.insert(
            swap_id,
            PendingAction {
                msg_type,
                payload,
                ttl: RETRANSMIT_TTL,
            },
        );
    }

    /// G20: the actions to re-flood this tick — every buffered pending action, decrementing each
    /// one's retransmit budget and dropping those that have run out. Called by the worker tick.
    pub fn pending_retransmits(&mut self) -> Vec<(MessageType, Vec<u8>)> {
        let mut out = Vec::new();
        self.pending.retain(|_, a| {
            out.push((a.msg_type, a.payload.clone()));
            a.ttl -= 1;
            a.ttl > 0
        });
        out
    }

    /// GC + safety-exit tick. First, any funds-locked swap whose own timeout has passed **refunds
    /// itself** — the always-available safety exit when a swap stalls (worst case: you get your own
    /// funds back). Then drop every coordinator that is terminal (Settled/Aborted/Refunded, incl.
    /// the just-refunded) or **stale** — a non-funded negotiation whose timelock has passed — so a
    /// long-lived node can't be memory-exhausted by half-opened or abandoned swaps. Returns the
    /// swap_ids dropped as **stale un-funded** negotiations — the ones for which the node should
    /// flood a teardown `SwapAbort` so the counterparty frees its slot too (a refund-reaped or
    /// already-terminal swap is dropped but NOT returned: it needs no teardown message). The node
    /// drives this off its worker maintenance tick.
    pub fn tick(&mut self, head: u64) -> Vec<[u8; SWAP_ID_LEN]> {
        // Safety exit: a funds-locked swap past its timeout refunds itself (no-op for non-funded or
        // not-yet-due swaps). In the sim the transition is the refund; production broadcasts the tx.
        for c in self.coordinators.values_mut() {
            let _ = c.refund_after_timeout(head);
        }
        // Stale = un-funded + non-terminal + past its timelock — these get a teardown Abort.
        let stale: Vec<[u8; SWAP_ID_LEN]> = self
            .coordinators
            .iter()
            .filter(|(_, c)| c.is_stale(head))
            .map(|(id, _)| *id)
            .collect();
        // Drop terminal + stale coordinators. Prune the pending action for every drop EXCEPT a
        // Settled one: a Settled initiator's `PreimageReveal` must keep propagating to the responder
        // (its own TTL expires it, decoupled from the now-gone coordinator — G20); a refund / abort /
        // stale teardown's action is dead, so stop retransmitting it.
        let prune: Vec<[u8; SWAP_ID_LEN]> = self
            .coordinators
            .iter()
            .filter(|(_, c)| {
                (c.phase().is_terminal() && c.phase() != crate::swap::SwapPhase::Settled)
                    || c.is_stale(head)
            })
            .map(|(id, _)| *id)
            .collect();
        self.coordinators
            .retain(|_, c| !c.phase().is_terminal() && !c.is_stale(head));
        for id in prune {
            self.pending.remove(&id);
        }
        stale
    }
}
