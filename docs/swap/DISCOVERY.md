# Discovery layer (G34–G58)

How two strangers in a Bluetooth dead zone find each other and start a NIM⇄BTC atomic swap, with no
server and no shared knowledge of a counterparty. Discovery is a thin layer *on top of* the existing
swap settlement protocol (`SWAP.md` / `MESH-INTEGRATION.md`): it floods lightweight advertisements,
matches a complementary pair, and hands off to the unchanged `SwapCoordinator` Propose→settle flow.

This is the engineering map — file + function pointers and the test that proves each piece. What's
deliberately NOT built (the money-path / native-bridge half) is in [`OWNER-GATED.md`](OWNER-GATED.md).

## The advertisement: `SwapIntent`

A flooded advertisement (`crates/nimmesh-core/src/swap_intent.rs`), wire type `MessageType::SwapIntent
= 0x45` (`packet.rs`), blind-relayed like a `nimiqTx`. Fields (encode/decode are bounds-checked,
panic-free):

| field | purpose | goal |
|---|---|---|
| `gives` (`Asset::Nim`/`Btc`) | which side the advertiser funds | G34 |
| `nim_amount` / `btc_amount` | the trade + rate | G34 |
| `expiry_height` | chain height the ad is valid through | G35 |
| `min_nim` / `max_nim` | acceptable trade-size band | G40 |
| `nim_pubkey` (32) + `signature` (64) | Ed25519 authentication | G41 |
| `nim_address` (20) | NIM receive address (= Blake2b(nim_pubkey)) | G34/G41 |
| `btc_pubkey` (33) + `btc_address` | BTC HTLC keys | G34 |
| `network_id` | a swap only forms within one network | G34 |

By convention the **NIM-giver is the initiator**: it sees a complementary BTC-giver ad, generates the
secret `S`, and Proposes. So matching is one-sided — a BTC-giver just floods and waits.

## Lifecycle

```
advertise (sign)  →  flood / blind-relay  →  [gates]  →  match window  →  Propose  →  settle
   sign_intent          handle_swap_packet    handle_intent   gc_tick         (existing SwapCoordinator
   (G41)                (relay_onward)         (buffer)        initiate_…       Propose→Accept→fund→
                                                              from_intent       reveal→settle)
```

- **Advertise** — `sign_intent` (or `sign_intent_ephemeral`, G45) fills `nim_pubkey`/`nim_address`/
  `signature`. A node re-floods its standing intent on a schedule (`readvertise_intent`, G37).
- **Receive** — `swap_node::handle_swap_packet`: first-seen dedup (`relay_seen`), then the freshness
  gate, then (if a participant) `handle_intent`, then blind `relay_onward`.
- **Match window** — `handle_intent` buffers a crossing candidate; `gc_tick` closes the window and
  `initiate_from_intent` builds the initiator coordinator + floods the `SwapPropose`.
- **Settle** — from the Propose on, it's the ordinary swap (`swap_coordinator` / `swap_session`), money
  -path gated by the `SwapSigner` seam (`MockSigner` in sim).

## The gates, in order

Every inbound `SwapIntent` runs this gauntlet; each gate has a metric (G42). In order:

0. **Per-link rate limit (G12/G53)** — `process_inbound` charges the `PeerRateLimiter` for the `src`
   peer BEFORE decode, so a flooding neighbour is bounded no matter how many origins it spoofs
   (`PEER_BUCKET_CAPACITY = 256`). `rate_limited` counter.
1. **First-seen dedup** — `relay_seen.insert(relay_key)`; a re-flood neither re-matches nor re-relays.
2. **Freshness (G35)** — `intent.is_fresh(head)`; once the chain head passes `expiry_height` the ad is
   dropped at every node (no match, no relay). `note_dropped_expiry`.
3. **Authenticity (G41)** — `incoming.verify_authentic()`: `nim_pubkey` must hash to `nim_address` AND
   the signature must `verify_strict` over the signed content. `note_dropped_signature`.
4. **Rate + amount (G40)** — `standing.would_initiate_against(incoming)` (NIM-giver, networks match,
   rates cross, cross-multiplied) AND `standing.amount_compatible(incoming)` (both sizes in both
   bands). `note_dropped_rate`.
5. **Concurrency cap** — `session.len() >= identity.max_concurrent_swaps`.
6. **Per-sender throttle (G36)** — `intents.throttle.admit(sender)` caps each origin `sender_id` at
   `DEFAULT_INTENT_MATCH_CAP_PER_SENDER = 4` admitted candidates. `note_dropped_throttle`.
7. **Best-rate window (G39)** — survivors are buffered (`matcher.add_candidate`, bounded by
   `MAX_INTENT_CANDIDATES = 64`); on the `INTENT_MATCH_WINDOW_TICKS = 2`-tick window close, the best
   (highest BTC-per-NIM, tie → smaller swap_id) is initiated. `note_matched`.

Tests: `swap_discovery_tests.rs` (one per gate — `a_complementary_intent_kicks_off_a_swap_that_settles`,
`an_incompatible_rate_intent_is_not_matched`, `an_expired_intent_does_not_match…`, the throttle +
best-rate + amount-band cases), `swap_discovery_ratelimit_tests.rs` (per-link), property tests in
`tests/swap_intent_proptests.rs` (decode never panics, round-trip, forgery resistance).

## Re-advertise + reconnect (G37/G47/G51)

`readvertise_intent` (in `gc_tick`) re-floods an unmatched node's standing intent on a bounded,
exponentially-backing-off schedule (`DEFAULT_MAX_INTENT_READVERTS = 5`, ticks ~1/3/6/11/20), so a
counterparty arriving later can still find it. It resets once a swap forms — and (G51) when the peer
set GROWS (`peer_degree()` up = a reconnected link), so a node silenced while cut off advertises again.
The by-design limit: a **delivery-only** outage that never drops the peer (e.g. `ether.partition`)
keeps the bounded budget and can go silent — proven alongside the reconnect recovery in
`swap_discovery_stress_tests.rs`.

## Security properties

- **Anti-spam, two layers** — per-link (G12/G53, bounds a flooding neighbour) + per-sender (G36, bounds
  a single origin's match attempts). With best-rate selection a flooder gets at most one swap per
  window.
- **Forgery resistance (G41)** — only an authentically-signed intent can make a matcher initiate;
  wrong-key / tampered-field / junk-signature all fail `verify_authentic` (property-tested).
- **Bounded re-advertise** — a node can't flood the mesh with its own ad forever (G37); the reconnect
  reset (G51) lifts the silence on an actual reconnect.
- **Worst case is a refund** — discovery hands off to the unchanged swap protocol; a discovered swap is
  a normal coordinator with the same Δ_safe timelock refund safety.

## Observability + health (G42/G55/G57)

`IntentMetrics` (`swap_node.rs`) — `AtomicUsize` counters: `seen`, `matched`, `dropped_rate/expiry/
throttle/signature`, `readvertised`, bumped at each gate. `swap_health::discovery_health(…)` derives a
read-only `DiscoveryHealth` (match rate, dominant drop, a `status`: Idle / NoCounterpartiesYet /
PossiblyUnderAttack / Healthy). Tests: `swap_metrics_tests.rs`, `swap_health_tests.rs`.

## Privacy (G45)

The intent floods identity material in cleartext, so discovery is poor on unlinkability by default. The
shipped mitigation is **ephemeral per-advertisement keys** (`sign_intent_ephemeral`): advertise under a
throwaway NIM key (+ rotated BTC fields), sweep proceeds after. Full threat model + residual leaks (the
BTC-pubkey↔HTLC link, amount fingerprinting) + the deeper gated mitigations:
[`DISCOVERY-PRIVACY.md`](DISCOVERY-PRIVACY.md). Tests: `swap_privacy_tests.rs`.

## Crash recovery (G43)

A node's standing intent rides `NodeIdentity`, so a restored node (`new_participant_restored`) resumes
re-advertising. A swap DISCOVERED before the crash is a normal coordinator, so G33 snapshot/restore
carries it — it comes back funds-locked and the restored node's tick still refunds it. Tests:
`swap_resume_tests.rs`. Buffered (not-yet-matched) candidates are intentionally NOT persisted (they
re-arrive via re-advertise).

## The demo (G38/G46/G54/G57)

`webui/swap/intents.html` — a read-only "open intents on the mesh" view (G38) on the real vendored
nimiq-ui: the open intents with give/take/rate/freshness, a discovery-stats strip (G46), and a one-line
health summary (G57). Fixture-fed by default; behind `swap_demo_server` the `loadIntents`/`loadStats`/
`loadHealth` seams upgrade it LIVE from `GET /api/intents` + `/api/stats` + `/api/health` (G54/G57,
served by `demo_http`), falling back to inline fixtures offline. The static-serving + JSON bodies are
smoke-tested in `tests/swap_demo_http_tests.rs`; the OWNER-GATED ledger is rot-guarded by
`tests/owner_gated_doclint.rs`.

## What's gated

Everything past sim/testnet — the live native WebView↔Rust bridge that feeds real data into the demo,
the real NIM/BTC money-path signer, a CSPRNG swap secret, mainnet/real funds, deeper privacy
(commit-reveal), and exposing discovery over UniFFI — is human-gated and tracked, with the exact code
seam each plugs into, in [`OWNER-GATED.md`](OWNER-GATED.md) (OG-1…OG-6).
