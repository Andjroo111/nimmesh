//! # swap_node — the SwapSession ↔ MeshNode hook (G14)
//!
//! The glue the agenda calls the "MeshNode swap hook": the worker's swap-packet handlers that sit
//! between a [`crate::node::MeshNode`]'s packet receipt and its per-swap [`crate::swap_session`]
//! coordinators. Packets in → coordinator dispatch → reply envelopes out over the radio.
//!
//! Two invariants hold the privacy line (core value #3): **every** node blind-relays a swap packet
//! onward exactly like a `nimiqTx` (the relay never parses terms, addresses, or the signed tx
//! blobs), and only a node that is also a swap **participant** — one carrying a `SwapSession` in its
//! [`WorkerState`] — additionally decodes its own `swap_id` off that otherwise-opaque stream to
//! advance its coordinator and flood a reply. A pure relay does neither.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::codec::encode;
use crate::engine::{relay_key, relay_onward, remember, WorkerCtx, WorkerState};
use crate::packet::{MessageType, Packet, PEER_ID_LEN};
use crate::swap::{SwapPhase, SwapRole};
use crate::swap_wire::{decode_swap, encode_swap, SwapEnvelope, SwapKind, SwapLegId, SWAP_ID_LEN};

/// G36: how many swap intents a single sender may turn into match attempts (coordinator spin-ups)
/// before we start dropping them. Well below [`crate::swap_session::DEFAULT_MAX_CONCURRENT_SWAPS`]
/// so one flooder can never consume more than a small slice of a matcher's swap slots.
pub(crate) const DEFAULT_INTENT_MATCH_CAP_PER_SENDER: u32 = 4;
/// G36: how many distinct senders the throttle tracks before evicting the oldest (memory bound).
const MAX_TRACKED_INTENT_SENDERS: usize = 256;

/// G36: a per-sender cap on how many flooded [`crate::swap_intent::SwapIntent`]s we will turn into
/// match attempts. The concurrency cap bounds how many live coordinators a node holds, but not WHO
/// fills them: without this, one node could flood thousands of distinct (rate-crossing) intents and
/// consume every swap slot, starving real counterparties (RISKS.md #4, the intent-layer analogue of
/// the relay [`crate::dedup::DedupCache`] / per-peer rate limiter). This caps each sender at
/// `cap_per_sender` admitted match attempts and bounds the sender table with oldest-sender eviction
/// — purely count-based, so it is deterministic with no clock. Eviction is the only recovery path,
/// which is sufficient for sim; production would age the budget on a window.
pub(crate) struct IntentThrottle {
    counts: HashMap<[u8; PEER_ID_LEN], u32>,
    order: VecDeque<[u8; PEER_ID_LEN]>,
    cap_per_sender: u32,
    max_senders: usize,
}

impl IntentThrottle {
    pub(crate) fn new() -> Self {
        IntentThrottle {
            counts: HashMap::new(),
            order: VecDeque::new(),
            cap_per_sender: DEFAULT_INTENT_MATCH_CAP_PER_SENDER,
            max_senders: MAX_TRACKED_INTENT_SENDERS,
        }
    }

    /// Admit one match attempt from `sender`: `true` (act on it) while the sender is under its cap,
    /// `false` (drop) once the budget is spent. A first-seen sender starts tracking, evicting the
    /// oldest sender if the table is full.
    pub(crate) fn admit(&mut self, sender: [u8; PEER_ID_LEN]) -> bool {
        if let Some(count) = self.counts.get_mut(&sender) {
            if *count >= self.cap_per_sender {
                return false;
            }
            *count += 1;
            return true;
        }
        if self.order.len() >= self.max_senders {
            if let Some(old) = self.order.pop_front() {
                self.counts.remove(&old);
            }
        }
        self.order.push_back(sender);
        self.counts.insert(sender, 1);
        true
    }
}

/// G37: how many times a node re-floods its standing intent while it stays unmatched.
pub(crate) const DEFAULT_MAX_INTENT_READVERTS: u32 = 5;
/// G37: the re-advertise gap is capped here (ticks), so the exponential backoff doesn't go silent.
const MAX_INTENT_READVERT_INTERVAL: u32 = 8;

/// G37: a bounded, exponentially-backing-off re-advertise schedule for a node's standing intent. A
/// node that floods an intent and gets no taker would otherwise go silent — but in a dead zone the
/// counterparty may only come into range later. So while still unmatched it re-floods on a schedule
/// that backs off (1, 2, 4, 8, 8, … ticks apart) and stops after `max_sends`. Purely tick-counted, so
/// it is deterministic with no clock. The schedule resets once a swap forms, giving the next idle
/// spell a fresh budget.
pub(crate) struct IntentAdvertiser {
    sent: u32,
    cooldown: u32,
    interval: u32,
    max_sends: u32,
}

impl IntentAdvertiser {
    pub(crate) fn new() -> Self {
        IntentAdvertiser {
            sent: 0,
            cooldown: 0,
            interval: 1,
            max_sends: DEFAULT_MAX_INTENT_READVERTS,
        }
    }

    /// Reset to the initial budget — called once a swap forms, so a later idle spell re-advertises.
    pub(crate) fn reset(&mut self) {
        self.sent = 0;
        self.cooldown = 0;
        self.interval = 1;
    }

    /// Advance one maintenance tick; `true` on a tick that should re-advertise. Fires at most
    /// `max_sends` times, the gap doubling each time (capped at [`MAX_INTENT_READVERT_INTERVAL`]).
    pub(crate) fn on_tick(&mut self) -> bool {
        if self.sent >= self.max_sends {
            return false;
        }
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return false;
        }
        self.sent += 1;
        self.cooldown = self.interval;
        self.interval = self
            .interval
            .saturating_mul(2)
            .min(MAX_INTENT_READVERT_INTERVAL);
        true
    }
}

/// G39: how many maintenance ticks the candidate-collection window stays open before the best
/// crossing intent in it is chosen. Short, so discovery still feels instant, yet long enough to let a
/// few near-simultaneous advertisements arrive and compete on rate.
const INTENT_MATCH_WINDOW_TICKS: u32 = 2;
/// G39: a hard cap on buffered candidates per window (a memory bound atop the per-sender throttle).
const MAX_INTENT_CANDIDATES: usize = 64;

/// G39: a short collection window over crossing swap-intent candidates. A NIM-giver no longer
/// initiates against the FIRST complementary intent it sees; it buffers the crossing candidates that
/// arrive within a bounded tick window, then initiates against the one offering the BEST rate (most
/// BTC per NIM), with a deterministic tie-break. Tick-counted, so it is deterministic with no clock.
pub(crate) struct IntentMatcher {
    candidates: Vec<([u8; SWAP_ID_LEN], crate::swap_intent::SwapIntent)>,
    window: u32,
}

impl IntentMatcher {
    pub(crate) fn new() -> Self {
        IntentMatcher {
            candidates: Vec::new(),
            window: 0,
        }
    }

    /// Whether `swap_id` is already a buffered candidate this window (so a re-flood is ignored).
    pub(crate) fn contains(&self, swap_id: &[u8; SWAP_ID_LEN]) -> bool {
        self.candidates.iter().any(|(id, _)| id == swap_id)
    }

    /// Buffer a crossing candidate, opening the window if it is closed. Bounded by
    /// `MAX_INTENT_CANDIDATES`; a full buffer drops the newcomer.
    pub(crate) fn add_candidate(
        &mut self,
        swap_id: [u8; SWAP_ID_LEN],
        intent: crate::swap_intent::SwapIntent,
    ) {
        if self.candidates.len() >= MAX_INTENT_CANDIDATES {
            return;
        }
        if self.window == 0 {
            self.window = INTENT_MATCH_WINDOW_TICKS;
        }
        self.candidates.push((swap_id, intent));
    }

    /// Advance one maintenance tick. When the window closes, return the BEST buffered candidate's
    /// swap_id and clear the buffer; otherwise `None`.
    pub(crate) fn tick(&mut self) -> Option<[u8; SWAP_ID_LEN]> {
        if self.window == 0 {
            return None;
        }
        self.window -= 1;
        if self.window > 0 {
            return None;
        }
        let best = self.best().map(|(id, _)| *id);
        self.candidates.clear();
        best
    }

    /// The best candidate for the NIM-giver: the highest BTC-per-NIM rate (cross-multiplied to avoid
    /// division), tie-broken by the smaller swap_id so the choice is fully deterministic.
    fn best(&self) -> Option<&([u8; SWAP_ID_LEN], crate::swap_intent::SwapIntent)> {
        self.candidates.iter().reduce(|best, c| {
            // `c` beats `best` when c.btc/c.nim > best.btc/best.nim, i.e. c.btc*best.nim > best.btc*c.nim.
            let lhs = (c.1.btc_amount as u128) * (best.1.nim_amount as u128);
            let rhs = (best.1.btc_amount as u128) * (c.1.nim_amount as u128);
            if lhs > rhs || (lhs == rhs && c.0 < best.0) {
                c
            } else {
                best
            }
        })
    }
}

/// G36/G37/G39: the worker's discovery-layer state — the per-sender match-attempt throttle, the
/// standing-intent re-advertise schedule, and the best-rate match window.
pub(crate) struct IntentState {
    pub(crate) throttle: IntentThrottle,
    pub(crate) advertiser: IntentAdvertiser,
    pub(crate) matcher: IntentMatcher,
    /// G51: the peer count at the last maintenance tick — a growth signals a (re)connected link, which
    /// resets the re-advertise budget so a node that went silent while cut off advertises again.
    pub(crate) last_peer_count: usize,
}

impl IntentState {
    pub(crate) fn new() -> Self {
        IntentState {
            throttle: IntentThrottle::new(),
            advertiser: IntentAdvertiser::new(),
            matcher: IntentMatcher::new(),
            last_peer_count: 0,
        }
    }
}

/// G42: read-only counters for the discovery layer — a pure-observability view of what the intent
/// gates do. Monotonic and `Relaxed` (like the worker's other counters); a participant bumps them as
/// intents are seen, matched, or dropped at each gate. No behaviour depends on them.
#[derive(Default)]
pub(crate) struct IntentMetrics {
    seen: AtomicUsize,
    matched: AtomicUsize,
    dropped_rate: AtomicUsize,
    dropped_expiry: AtomicUsize,
    dropped_throttle: AtomicUsize,
    dropped_signature: AtomicUsize,
    readvertised: AtomicUsize,
}

#[cfg(test)]
#[path = "swap_node_test_hooks.rs"]
mod test_hooks;
#[cfg(test)]
pub(crate) use test_hooks::{start_swap, swap_phase, IntentMetricsSnapshot};

impl IntentMetrics {
    pub(crate) fn note_seen(&self) {
        self.seen.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn note_matched(&self) {
        self.matched.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn note_dropped_rate(&self) {
        self.dropped_rate.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn note_dropped_expiry(&self) {
        self.dropped_expiry.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn note_dropped_throttle(&self) {
        self.dropped_throttle.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn note_dropped_signature(&self) {
        self.dropped_signature.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn note_readvertised(&self) {
        self.readvertised.fetch_add(1, Ordering::Relaxed);
    }

    /// G9 (#80): the same consistent read as an FFI-boundary
    /// [`FfiIntentMetrics`](crate::swap_intent::FfiIntentMetrics) (counts widened to `u64`). Exposed
    /// to the app via [`crate::node::MeshNode::discovery_metrics`].
    pub(crate) fn ffi_snapshot(&self) -> crate::swap_intent::FfiIntentMetrics {
        let g = |c: &AtomicUsize| c.load(Ordering::Relaxed) as u64;
        crate::swap_intent::FfiIntentMetrics {
            seen: g(&self.seen),
            matched: g(&self.matched),
            dropped_rate: g(&self.dropped_rate),
            dropped_expiry: g(&self.dropped_expiry),
            dropped_throttle: g(&self.dropped_throttle),
            dropped_signature: g(&self.dropped_signature),
            readvertised: g(&self.readvertised),
        }
    }
}

/// A swap packet (`0x40`–`0x44`). Blind-relay it onward like any flooded packet, and — if this node
/// is a participant — route it to our own [`crate::swap_session::SwapSession`] and flood whatever
/// reply it produces (a `Propose` → an `Accept`, etc.).
pub(crate) fn handle_swap_packet(
    ctx: &WorkerCtx,
    packet: Packet,
    src: Option<&str>,
    st: &mut WorkerState,
) {
    // First-seen gate: a re-flooded duplicate must neither re-relay nor re-drive a coordinator.
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    remember(ctx, st, &packet);

    // G35: a swap intent dies at every node — participant or pure relay — once the chain head passes
    // its `expiry_height`. We decode only to read that field; a stale ad is neither matched nor
    // relayed onward, so it stops propagating instead of lingering on the mesh forever. (A malformed
    // intent falls through to the blind relay, exactly as before — relay never inspects payloads.)
    if packet.msg_type == MessageType::SwapIntent {
        let head = ctx.cached_head().map(u64::from).unwrap_or(0);
        if let Ok(intent) = crate::swap_intent::decode_intent(&packet.payload) {
            // G42: a participant counts every intent it observes; a pure relay carries them blind.
            if st.swap.is_some() {
                ctx.intent_metrics.note_seen();
            }
            if !intent.is_fresh(head) {
                if st.swap.is_some() {
                    ctx.intent_metrics.note_dropped_expiry();
                }
                return;
            }
        }
    }

    // Participant path: route the packet to our own session and flood whatever it produces. A pure
    // relay (no session) skips this entirely. The `on_message` borrow of `st` is dropped before we
    // flood (which re-borrows `st` for dedup/remember), so the replies are collected first.
    if st.swap.is_some() {
        if packet.msg_type == MessageType::SwapIntent {
            // G34/G39: discovery — buffer the intent against our standing one as a match-window
            // candidate (the window close in `gc_tick` initiates the best). G36: keyed by the flood's
            // origin (`sender_id`) so the per-sender throttle caps a flooder, not a real advertiser.
            handle_intent(ctx, st, packet.sender_id, &packet.payload);
        } else if let Some(kind) = SwapKind::from_message_type(packet.msg_type) {
            let head = ctx.cached_head().map(u64::from).unwrap_or(0);
            let mut replies = match st.swap.as_mut() {
                Some(session) => session
                    .on_message(kind, &packet.payload, head)
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            // G17: after the message advanced our coordinator, take this node's next phase-driven
            // chain action (fund / claim / settle, sim tx bytes) and flood its envelope(s) too.
            replies.extend(drive_swap(st, kind, &packet.payload, head));
            // G20: remember this swap's latest action so the tick can re-flood it if it's dropped.
            // All replies here are for the incoming packet's swap_id.
            if let (Some(swap_id), Some(last)) = (
                decode_swap(kind, &packet.payload).ok().map(|e| e.swap_id),
                replies.last().cloned(),
            ) {
                if let Some(session) = st.swap.as_mut() {
                    session.record_action(swap_id, last.0, last.1);
                }
            }
            sync_swap_phases(ctx, st);
            for (mt, payload) in replies {
                flood_swap_reply(ctx, mt, payload, st);
            }
        }
    }

    // Blind relay onward (source link excluded), identical to the pure-relay path.
    relay_onward(ctx, packet, src, st);
}

/// G17/G26: the node-side swap **driver**. After an incoming message advanced our coordinator for a
/// swap, take this node's next phase-driven chain action and return the envelope(s) to flood. Each
/// inbound message triggers at most one action — the coordinator's phase is the idempotency guard,
/// so a replayed/duplicate message (which doesn't change the phase) drives nothing twice. The actual
/// funding/claim tx bytes come from this node's [`SwapSigner`] (today a `MockSigner` stand-in; the
/// real NIM/BTC signer drops in through the same seam, money-path gated).
fn drive_swap(
    st: &mut WorkerState,
    last_kind: SwapKind,
    last_payload: &[u8],
    head: u64,
) -> Vec<(MessageType, Vec<u8>)> {
    let Ok(env) = decode_swap(last_kind, last_payload) else {
        return Vec::new();
    };
    let swap_id = env.swap_id;

    // A2b: surface the peer's protocol-carried addressing to the signer seam (a live signer
    // needs the counterparty's payout addresses; the SwapContext holds only our own). Only for
    // a swap this node actually holds a coordinator for — the session already vetted it (a
    // Propose was signature-verified before its responder coordinator was built; an Accept
    // only lands on a swap we initiated). The signer applies first-report-wins.
    if matches!(last_kind, SwapKind::Propose | SwapKind::Accept)
        && st
            .swap
            .as_ref()
            .is_some_and(|s| s.coordinators.contains_key(&swap_id))
    {
        if let Some(signer) = st.signer.as_ref() {
            match last_kind {
                SwapKind::Propose => {
                    if let Some(p) = crate::swap_messages::SwapProposal::from_envelope(&env) {
                        signer.note_peer(swap_id, p.nim_address, &p.btc_address);
                    }
                }
                SwapKind::Accept => {
                    if let Some(a) = crate::swap_messages::SwapAcceptance::from_envelope(&env) {
                        signer.note_peer(swap_id, a.nim_address, &a.btc_address);
                    }
                }
                _ => {}
            }
        }
    }

    // A responder learns S off an incoming PreimageReveal (the signed claim carried it; sim: the
    // tx_wire is the 32-byte secret), verifies it opens the hashlock, and advances to Revealed — it
    // then claims its leg + settles in the match below.
    if last_kind == SwapKind::PreimageReveal {
        if let Some(secret) = env
            .tx_wire
            .as_deref()
            .and_then(|w| w.get(..32))
            .and_then(|s| <[u8; 32]>::try_from(s).ok())
        {
            if let Some(coord) = coordinator(st, &swap_id) {
                let _ = coord.recv_reveal(&env, secret);
            }
        }
    }

    // Read this swap's action context, then build the tx via the signer and feed it to the
    // coordinator. (role/phase/secret are Copy, so the read borrow is dropped before the signer +
    // coordinator borrows.)
    let Some((role, phase, secret, sctx)) =
        coordinator(st, &swap_id).map(|c| (c.role(), c.phase(), c.secret(), c.context().clone()))
    else {
        return Vec::new();
    };

    let mut out = Vec::new();
    match (role, phase) {
        // Initiator: once the responder accepted, fund the NIM leg → FundingProof.
        (SwapRole::Initiator, SwapPhase::Accepted) => {
            if let Some((wire, id)) = sign_funding(st, &sctx, SwapLegId::Nim) {
                if let Some(c) = coordinator(st, &swap_id) {
                    if let Ok(fp) = c.fund(head, wire, id) {
                        push_env(&mut out, MessageType::SwapFundingProof, &fp);
                    }
                }
            }
        }
        // Responder: once it has seen the initiator's funding, fund the BTC leg → FundingProof.
        (SwapRole::Responder, SwapPhase::InitiatorFunded) => {
            if let Some((wire, id)) = sign_funding(st, &sctx, SwapLegId::Counterparty) {
                if let Some(c) = coordinator(st, &swap_id) {
                    if let Ok(fp) = c.fund(head, wire, id) {
                        push_env(&mut out, MessageType::SwapFundingProof, &fp);
                    }
                }
            }
        }
        // Initiator: once both legs are funded, claim the counterparty leg (revealing S) → settle.
        (SwapRole::Initiator, SwapPhase::BothFunded) => {
            // M3: gate the reveal deadline BEFORE the signer acts. A LIVE claim broadcasts
            // `withdraw(S)` — the on-chain reveal — so the coordinator's identical gate
            // inside `claim_and_reveal` would fire only after S was already public. Too
            // close to `T_B` → keep S secret and take the refund path once it passes.
            let deadline_ok = coordinator(st, &swap_id).is_some_and(|c| c.reveal_deadline_ok(head));
            if !deadline_ok {
                return out;
            }
            if let Some((wire, id)) = secret.and_then(|s| sign_claim(st, &sctx, s)) {
                if let Some(c) = coordinator(st, &swap_id) {
                    if let Ok(rev) = c.claim_and_reveal(head, wire, id) {
                        push_env(&mut out, MessageType::SwapPreimageReveal, &rev);
                        let _ = c.settle();
                    }
                }
            }
        }
        // Responder: S is out — claim OUR NIM leg with it (a live signer builds + lands the
        // on-chain claim; the mock returns a stand-in), then settle. No mesh message — the
        // claim lives on-chain, and the phase flip makes this idempotent across replays.
        (SwapRole::Responder, SwapPhase::Revealed) => {
            if let Some(s) = secret {
                let _ = sign_claim(st, &sctx, s);
            }
            if let Some(c) = coordinator(st, &swap_id) {
                let _ = c.settle();
            }
        }
        _ => {}
    }
    out
}

/// This node's coordinator for a swap (the participant session's, if any).
fn coordinator<'a>(
    st: &'a mut WorkerState,
    swap_id: &[u8; SWAP_ID_LEN],
) -> Option<&'a mut crate::swap_coordinator::SwapCoordinator> {
    st.swap.as_mut().and_then(|s| s.coordinator(swap_id))
}

/// G34/G39: a received swap intent. If this node holds a complementary standing intent that crosses
/// on rate (so it is the NIM-giver / initiator) and is under its concurrency cap, BUFFER it as a
/// candidate for the current match window (G39) — the window close in [`gc_tick`] then initiates
/// against the best-rate candidate. A BTC-giver (or no standing intent) buffers nothing and just
/// waits for a Propose. Returns no immediate reply (initiation is deferred to the window close).
fn handle_intent(ctx: &WorkerCtx, st: &mut WorkerState, sender: [u8; PEER_ID_LEN], payload: &[u8]) {
    let Ok(incoming) = crate::swap_intent::decode_intent(payload) else {
        return;
    };
    // G41: only act on an authentically-signed intent — its pubkey must hash to the claimed NIM
    // address and its signature must verify. A forged advertisement can't make us initiate a swap.
    if !incoming.verify_authentic() {
        ctx.intent_metrics.note_dropped_signature();
        return;
    }
    let swap_id = {
        let Some(session) = st.swap.as_ref() else {
            return;
        };
        let Some(standing) = session.identity.standing_intent.as_ref() else {
            return;
        };
        if !standing.would_initiate_against(&incoming) || !standing.amount_compatible(&incoming) {
            ctx.intent_metrics.note_dropped_rate(); // wrong side / non-crossing rate / mis-sized (G40)
            return;
        }
        if session.len() >= session.identity.max_concurrent_swaps {
            return; // at the concurrency cap (not a discovery-gate drop; left uncounted)
        }
        let swap_id = derive_swap_id(standing, &incoming);
        if session.coordinators.contains_key(&swap_id) {
            return; // already initiating this exact swap
        }
        if session.has_initiated(&swap_id) {
            return; // #189: we already ran this swap once; its S may be public. A repeat
                    // trade needs a fresh ephemeral identity (→ fresh swap_id), not a reissue.
        }
        swap_id
    };

    // G39: a re-flood we've already buffered this window is ignored (so it doesn't double-charge).
    if st.intents.matcher.contains(&swap_id) {
        return;
    }
    // G36: charge the sender's budget — a flooder that has spent it can't keep filling the buffer.
    if !st.intents.throttle.admit(sender) {
        ctx.intent_metrics.note_dropped_throttle();
        return;
    }
    // G39: collect it; the window close initiates against the best candidate, not the first.
    st.intents.matcher.add_candidate(swap_id, incoming);
}

/// G34/G39: build this node's initiator coordinator for the `swap_id` chosen out of the match window
/// and return its `Propose` to flood. Uses this node's standing-intent terms + the sim secret
/// (money-path gated). Re-checks the concurrency cap / dedup at close time, since state may have moved
/// on while the window was open.
fn initiate_from_intent(
    st: &mut WorkerState,
    swap_id: [u8; SWAP_ID_LEN],
    head: u64,
) -> Vec<(MessageType, Vec<u8>)> {
    let (standing, ladder) = {
        let Some(session) = st.swap.as_ref() else {
            return Vec::new();
        };
        let Some(standing) = session.identity.standing_intent.clone() else {
            return Vec::new();
        };
        if session.coordinators.contains_key(&swap_id)
            || session.has_initiated(&swap_id) // #189: never reissue a used swap's secret
            || session.len() >= session.identity.max_concurrent_swaps
        {
            return Vec::new();
        }
        // §8.2 hard per-swap cap: never PROPOSE above the agreed test size (mirrors the accept cap).
        if !session.within_caps(standing.nim_amount, standing.btc_amount) {
            return Vec::new();
        }
        (standing, session.ladder)
    };

    let secret = match st.swap.as_ref() {
        Some(session) => (session.secret_source)(&swap_id),
        None => return Vec::new(),
    };
    let ctx = crate::swap_coordinator::SwapContext {
        swap_id,
        // Sim timelocks (T_A / T_B), anchored to the live mesh head so a REAL device (head in
        // the millions) doesn't mint born-expired swaps. At head 0 these are the exact fixed
        // values the deterministic swap tests rely on (10_000 / 5_000). The real money-path
        // derives tighter windows from the Δ_safe ladder (gated).
        terms: crate::swap::SwapTerms {
            nim_timeout: head + 10_000,
            counterparty_timeout: head + 5_000,
        },
        hashlock: crate::swap_leg::sha256(&secret),
        nim_address: standing.nim_address,
        btc_address: standing.btc_address.clone(),
        btc_pubkey: standing.btc_pubkey,
        give_amount: standing.nim_amount,
        take_amount: standing.btc_amount,
        network_id: standing.network_id,
        // M4: the head these terms were just minted against IS the anchor the live
        // timeout mappings subtract (ADR-0010) — never 0 against a real, million-high head.
        term_anchor: head,
    };
    let (coord, propose) =
        crate::swap_coordinator::SwapCoordinator::new_initiator(ctx, secret, ladder);
    // S2 / #73: authenticate the Propose under this node's NIM enclave key before it floods, so the
    // responder can verify the proposed terms weren't injected or tampered in transit. The seed stays
    // behind the `EnclaveKey` seam — only the signature + public key ride the wire.
    let signed = st
        .swap
        .as_ref()
        .map(|s| s.sign_propose(&propose))
        .unwrap_or(propose);
    if let Some(session) = st.swap.as_mut() {
        session.add_initiator(swap_id, coord);
    }
    match encode_swap(&signed) {
        Ok(bytes) => vec![(MessageType::SwapPropose, bytes)],
        Err(_) => Vec::new(),
    }
}

/// A deterministic swap_id for an intent-initiated swap (unique per advertiser/counterparty pair).
pub(crate) fn derive_swap_id(
    standing: &crate::swap_intent::SwapIntent,
    incoming: &crate::swap_intent::SwapIntent,
) -> [u8; SWAP_ID_LEN] {
    let mut buf = Vec::new();
    buf.extend_from_slice(&standing.nim_address);
    buf.extend_from_slice(&standing.btc_pubkey);
    buf.extend_from_slice(&incoming.btc_pubkey);
    let h = crate::swap_leg::sha256(&buf);
    h[..SWAP_ID_LEN].try_into().unwrap()
}

/// A SIM stand-in for the swap secret `S`. **GATED:** production MUST draw `S` from a CSPRNG — a
/// predictable secret lets an attacker pre-claim the BTC leg. This deterministic placeholder exists
/// only so the no-RNG sim/test is reproducible; it is never used on a real-fund path.
pub(crate) fn sim_secret(swap_id: &[u8; SWAP_ID_LEN]) -> [u8; 32] {
    let mut buf = b"NIMMESH-SIM-INTENT-SECRET-DO-NOT-USE-IN-PROD".to_vec();
    buf.extend_from_slice(swap_id);
    crate::swap_leg::sha256(&buf)
}

/// Ask this node's signer to build the funding tx for `leg` (money-path seam).
fn sign_funding(
    st: &WorkerState,
    ctx: &crate::swap_coordinator::SwapContext,
    leg: SwapLegId,
) -> Option<(Vec<u8>, [u8; 32])> {
    st.signer.as_ref().and_then(|s| s.build_funding(ctx, leg))
}

/// Ask this node's signer to build the claim tx for/with `secret` (money-path seam).
fn sign_claim(
    st: &WorkerState,
    ctx: &crate::swap_coordinator::SwapContext,
    secret: [u8; 32],
) -> Option<(Vec<u8>, [u8; 32])> {
    st.signer.as_ref().and_then(|s| s.build_claim(ctx, secret))
}

/// Encode a driven envelope and tag it with its flood [`MessageType`].
fn push_env(out: &mut Vec<(MessageType, Vec<u8>)>, mt: MessageType, env: &SwapEnvelope) {
    if let Ok(bytes) = encode_swap(env) {
        out.push((mt, bytes));
    }
}

/// Flood a swap envelope this node originated (an `Accept` reply, or a `Propose`). Mirrors the
/// `flood_local_tx` discipline: remember our own packet so its echo is not re-flooded and so
/// gossip-sync can serve it to a peer that was out of range.
pub(crate) fn flood_swap_reply(
    ctx: &WorkerCtx,
    mt: MessageType,
    payload: Vec<u8>,
    st: &mut WorkerState,
) {
    let packet = ctx.build_packet(mt, payload);
    st.relay_seen.insert(relay_key(&packet));
    remember(ctx, st, &packet);
    if let Ok(bytes) = encode(&packet) {
        ctx.flood(bytes);
    }
}

/// Refresh the node's observable swap-phase mirror to exactly the session's live set, so the FFI
/// side can read a swap's progress without reaching into the worker-thread-local session. Rebuilt
/// (not merged) so a swap the GC tick dropped vanishes from the mirror too.
pub(crate) fn sync_swap_phases(ctx: &WorkerCtx, st: &WorkerState) {
    let mut map = ctx.swaps.lock().unwrap();
    map.clear();
    if let Some(session) = st.swap.as_ref() {
        for (id, phase) in session.phases() {
            map.insert(id, phase);
        }
    }
}

/// G16/G18/G19: GC tick — refund funds-locked-expired swaps + drop terminal/stale ones (so a
/// long-lived node sheds abandoned half-opened swaps), then (G19) flood a teardown `SwapAbort` for
/// each stale un-funded swap dropped so the counterparty frees its slot too. Refresh the phase
/// mirror. Driven off the worker's maintenance tick.
pub(crate) fn gc_tick(ctx: &WorkerCtx, st: &mut WorkerState) {
    if st.swap.is_none() {
        return;
    }
    let head = ctx.cached_head().map(u64::from).unwrap_or(0);
    // G20: re-flood each live swap's last action (recover a message dropped over a lossy mesh). The
    // coordinator's phase absorbs duplicates, so this is idempotent. Retransmit BEFORE the GC reap
    // so a just-Settled initiator's reveal still goes out.
    let retransmits = match st.swap.as_mut() {
        Some(session) => session.pending_retransmits(),
        None => Vec::new(),
    };
    for (mt, payload) in retransmits {
        flood_swap_reply(ctx, mt, payload, st);
    }
    let aborted = match st.swap.as_mut() {
        Some(session) => session.tick(head),
        None => Vec::new(),
    };
    for swap_id in aborted {
        if let Ok(payload) = encode_swap(&crate::swap_messages::abort(swap_id, 0)) {
            flood_swap_reply(ctx, MessageType::SwapAbort, payload, st);
        }
    }
    // G39: close the match window (if due) and initiate against the best buffered candidate, recording
    // the Propose so a lossy mesh retransmits it.
    if let Some(swap_id) = st.intents.matcher.tick() {
        let replies = initiate_from_intent(st, swap_id, head);
        if !replies.is_empty() {
            ctx.intent_metrics.note_matched(); // a discovered swap actually started (G42)
        }
        for (mt, payload) in replies {
            if let Some(session) = st.swap.as_mut() {
                session.record_action(swap_id, mt, payload.clone());
            }
            flood_swap_reply(ctx, mt, payload, st);
        }
    }
    readvertise_intent(ctx, st, head);
    sync_swap_phases(ctx, st);
}

/// G37: re-advertise this node's standing swap intent on the maintenance tick. While the node is
/// still **unmatched** (holds no coordinator) and its intent is **fresh** (G35), re-flood the
/// standing intent on the [`IntentAdvertiser`]'s bounded backing-off schedule, so a counterparty that
/// only later comes into range can still find it. Once a swap forms we stop and reset the budget.
fn readvertise_intent(ctx: &WorkerCtx, st: &mut WorkerState, head: u64) {
    // G51: a node whose peer set GREW since the last tick just (re)connected a link — reset the
    // re-advertise budget so an advertiser that exhausted it while cut off starts advertising again.
    // (A delivery-only outage that never drops the peer, e.g. `ether.partition`, leaves the count
    // unchanged and so keeps the G37 budget limit — see the G47 budget-exhaustion test.)
    let peers = ctx.peer_degree();
    if peers > st.intents.last_peer_count {
        st.intents.advertiser.reset();
    }
    st.intents.last_peer_count = peers;

    let intent = {
        let Some(session) = st.swap.as_ref() else {
            return;
        };
        if !session.coordinators.is_empty() {
            // A swap formed → stop advertising; reset so a later idle spell starts a fresh campaign.
            st.intents.advertiser.reset();
            return;
        }
        match &session.identity.standing_intent {
            Some(i) if i.is_fresh(head) => i.clone(),
            _ => return, // no standing intent, or it has expired (G35) → nothing to re-advertise
        }
    };
    if st.intents.advertiser.on_tick() {
        ctx.intent_metrics.note_readvertised(); // G42
        let bytes = crate::swap_intent::encode_intent(&intent);
        flood_swap_reply(ctx, MessageType::SwapIntent, bytes, st);
    }
}

/// The live swaps this node participates in, as FFI [`crate::swap_intent::FfiSwapMatch`] records
/// (`swap_id` hex + phase), sorted by id. Reads the observable phase mirror (kept current by
/// [`sync_swap_phases`]) so the app lists in-flight swaps without touching the worker session.
pub(crate) fn active_swaps(ctx: &WorkerCtx) -> Vec<crate::swap_intent::FfiSwapMatch> {
    use crate::swap_intent::FfiSwapMatch;
    let map = ctx.swaps.lock().unwrap();
    let mut out: Vec<FfiSwapMatch> = map
        .iter()
        .map(|(id, phase)| FfiSwapMatch {
            swap_id: crate::nimiq::hex::bytes_to_hex(id),
            phase: (*phase).into(),
        })
        .collect();
    out.sort_by(|a, b| a.swap_id.cmp(&b.swap_id));
    out
}
