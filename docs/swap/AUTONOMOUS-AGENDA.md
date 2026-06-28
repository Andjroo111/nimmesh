# nimmesh swap — autonomous build loop (agenda + contract)

This is the standing agenda for the self-running swap loop. Each iteration: read this + the off-repo
log (`~/nimmesh-swap-loop/LOG.md`), pick the **next unchecked goal**, build it, gate it, commit on
green, log one line, and schedule the next iteration. No human prompt between goals.

## Goal ladder (do in order; each is one iteration-sized chunk)

- [x] **G1 — Refund / timeout-safety in the sim.** `SwapSim` can drive the stall→refund path (both
      legs refunded past their timeout); a test proves "worst case is a refund, never a loss."
- [x] **G2 — Refund path in the demo.** Surface G1 in the demo server + UI: a stall→refund flow
      ending in "Refunded — your funds are safe" (real `refund_after_timeout`).
- [x] **G3 — swap_wire proptest hardening.** Fuzz `decode_swap` (panic-free on arbitrary bytes) and
      round-trip any valid envelope incl. the new `btc_pubkey`.
- [x] **G4 — Mesh swap message builder.** Build `Propose`/`Accept`/`FundingProof`/`PreimageReveal`
      envelopes from engine state (the engine↔wire bridge), with tests.
- [x] **G5 — Full swap negotiated over the mock mesh.** Drive a complete swap between two MockRadio
      nodes (Propose→Accept→fund→reveal) — swaps actually working over the transport.
- [x] **G6 — Demo polish.** Engine-driven auto-advance + a responder-perspective toggle.
- [x] **G7 — Docs sweep.** Refresh SWAP / FEASIBILITY / BTC-LEG to match the shipped engine+UI+demo.
- [x] **G8 — Swap coordinator (message handler).** A pure `SwapCoordinator` that consumes an incoming
      swap envelope, advances the local swap state, and produces the next outgoing envelope(s) — the
      brain a mesh node runs. Test: drive a full swap between two coordinators exchanging envelopes
      (no hand orchestration).
- [x] **G9 — Adversarial wire/sim paths.** Robustness over a lossy mesh: a wrong-preimage reveal is
      rejected, out-of-order / duplicate messages are idempotent, an unsafe ladder refuses to fund.
- [x] **G10 — Swap fees tooltip (UI).** Port the wallet's real `SwapFeesTooltip` into the Confirm
      screen (fee breakdown) — actual component, no approximation.

When the ladder is exhausted again, re-scan for the next-highest-value work (mesh integration depth,
more hardening, perf) and append goals — keep building.

## Guardrails (hard)

- **Branch `feat/mesh-swap` only. NEVER touch `main`** (another chat owns it; merge is Andjroo's call).
- **Sim / testnet only. NO mainnet, NO real funds, NO live broadcast.** The money-path stays gated.
- **Gate every goal before commit:** `cargo test -p nimmesh-core` (default) + `--features bitcoin-leg`
  green · `cargo clippy --features bitcoin-leg --all-targets` (no warnings) · `cargo fmt --check` ·
  `scripts/size-guard.sh` · `nq lint` for any UI page. Auto-commit on green; push to `feat/mesh-swap`.
- **Presentation/UI is Andjroo's domain** — match the real Nimiq references, never approximate
  (see the [[feedback_nimiq_ui_use_real_components]] lesson).
- A goal needing a human decision (merge / mainnet / real funds / the native WebView↔Rust bridge) →
  log it **BLOCKED**, skip to the next. If all remaining are blocked → stop the loop and surface.

## Loop mechanics

Self-paced via `ScheduleWakeup` with the `<<autonomous-loop-dynamic>>` sentinel. Durable state is on
disk (this file + the off-repo log + the commits), so the loop survives context compaction.
- [x] **G11 — Swap-in-progress toast.** When a running swap is minimized, surface it as the real
      `toast-notification` "Performing swap X/5" info state (hexagon spinner); clear on done/refund.
- [x] **G12 — Setup amount validation.** The Swap Currencies screen disables Confirm + shows an
      inline message on invalid amounts (zero / over balance / below a min), like the real swap UI.
- [x] **G13 — SwapSession router.** A node-side `swap_session` that routes incoming swap packets by
      `swap_id` to the right `SwapCoordinator` and collects its outgoing envelopes — the glue between
      `MeshNode` packet receipt and the coordinator. Tested by feeding it packets directly.

When the ladder is exhausted again, re-scan and append. (Re-scan after G13: the coordinator + router
now exist but nothing wires them to the actual `MeshNode` packet loop, and the router has no expiry
for abandoned half-negotiated swaps. The next-highest-value work is closing that integration gap and
hardening the router against a hostile/lossy mesh.)

- [x] **G14 — MeshNode swap hook.** Wire `SwapSession` into the `MeshNode` receive path: on a swap
      `MessageType`, hand the packet to the session and flood the returned envelopes back over the
      radio. Tested over the `MockRadio`/`MeshHarness` — a swap message injected at one node produces
      the right flooded reply, end to end through the real node loop (not the session in isolation).
- [x] **G15 — Router robustness over a hostile mesh.** Harden `SwapSession.on_message`: a replayed
      `Propose` for a live `swap_id` must not clobber the in-flight coordinator, a malformed payload
      is dropped (never panics, never half-creates a coordinator), and an `Abort`/settle frees the
      slot. Proptest: arbitrary packet streams never panic and never strand a coordinator mid-state.
- [x] **G16 — Session expiry / GC.** Give `SwapSession` a `tick(head)` that drops coordinators whose
      swaps are terminal (Settled/Refunded) or whose negotiation stalled past a deadline, so a node
      can't be memory-exhausted by half-opened swaps. Tested by advancing head past the timeouts.

When the ladder is exhausted again, re-scan and append. (Re-scan after G16: the protocol negotiation
now runs end to end through the node loop, but the node still can't *drive its own chain legs* —
fund/claim/refund are manual coordinator calls, no FundingProof/Reveal/Abort is emitted from a phase
change, and nothing drives the safety refund at the node level. The next-highest-value work closes
that node-side driver gap, all sim/stand-in tx bytes — real tx signing stays money-path-gated.)

- [x] **G17 — Full swap lifecycle over the real mesh node loop.** Extend G14: a node-side driver
      that reacts to phase changes — fund its own leg + flood the `FundingProof`, and on `BothFunded`
      the initiator claims + floods the `PreimageReveal` (responder extracts `S`, claims its leg) —
      using sim / stand-in tx bytes (no real signing). Two participant nodes drive a whole swap to
      `Settled` purely through the node receive/flood loop; assert no one-sided settlement.
- [x] **G18 — Node-level refund tick (the safety exit over the mesh).** For a funds-locked swap whose
      own timelock has passed, the worker tick drives `refund_after_timeout` (sim) → `Refunded`, then
      GC reaps it — proving "worst case is a refund" holds at the node level, not just in the model.
- [x] **G19 — Abort emission + symmetric teardown.** When a node locally cancels an un-funded swap
      (or GCs a stale proposal it originated), it floods a `SwapAbort` so the counterparty frees its
      slot too. Tested: an abort from one participant clears the swap on the other over the mesh.

When the ladder is exhausted again, re-scan and append. (Re-scan after G19: the whole swap protocol
now runs + tears down + refunds end to end through the node loop on a *reliable* mesh. The remaining
frontier is **resilience under loss** — each phase action floods exactly once, so a single dropped
`FundingProof`/`PreimageReveal` stalls a swap, and a lost reveal could even strand the responder
without `S`. The next work makes swaps survive a real lossy/partitioned BLE mesh. Still sim/testnet,
money-path gated.)

- [x] **G20 — Pending-action retransmit (don't let a lost message strand a swap).** The node caches
      each swap's last-emitted action envelope and re-floods it on the maintenance tick while the
      swap is non-terminal and unadvanced, so a dropped `FundingProof`/`PreimageReveal`/`Accept` is
      recovered (idempotent — the coordinator's phase absorbs duplicates). Tested over a lossy mesh.
- [x] **G21 — Many concurrent swaps over a lossy mesh.** With retransmit in place, drive N
      participant-pair swaps to `Settled` (or clean refund) through two+ `MeshNode` loops over a
      `MockEther` with loss + latency, asserting no one-sided settlement under adversarial conditions.
- [x] **G22 — Swap catch-up via store-and-forward on rejoin.** A participant that was out of range
      when a swap message flooded catches it up via the G7 gossip-sync on rejoin, and the swap still
      completes — proving the swap inherits the mesh's offline resilience.

When the ladder is exhausted again, re-scan and append. (Re-scan after G22: the swap now runs, tears
down, refunds, retransmits, GCs, handles concurrency, and catches up offline — all over the REAL node
loop, all proven so far over a *direct* link or a *single* relay. The remaining frontier is **depth +
mid-flight resilience + a written map**: prove a swap over many relay hops, prove it survives a
partition that strikes *mid-swap*, and document the G8–G22 node-integration architecture so the human
reviewer has a guide. Still sim/testnet, money-path gated.)

- [x] **G23 — Full swap over a multi-hop mesh.** Two participants separated by N blind relay hops
      (a line topology) drive a whole swap to `Settled` — proving swaps ride a *deep* mesh end to end,
      not just a direct link or a single relay. Assert no one-sided settlement.
- [x] **G24 — Mid-swap partition + heal.** A swap that is mid-flight (e.g. one leg funded) when the
      mesh partitions recovers and completes after the heal, via retransmit + store-and-forward —
      proving resilience to a transient outage that strikes during the swap, not just before it.
- [x] **G25 — Node-integration architecture docs.** Write `docs/swap/MESH-INTEGRATION.md` mapping the
      G8–G24 build: `SwapCoordinator` → `SwapSession` router → `swap_node` hook/driver → retransmit →
      GC/refund/abort ticks → the resilience properties, so the human reviewer + future work have a
      guide to the (now large) node-side swap stack. Docs only, no code risk.

When the ladder is exhausted again, re-scan and append. (Re-scan after G25: the node-side swap stack
is complete, hardened, resilient, and documented over the sim mesh. What remains is **breadth +
real-money readiness**, all still sim/testnet-or-gated: a sweep of the older docs (SWAP / FEASIBILITY
/ SWAP-ENGINE) to fold in the G8–G24 reality + link MESH-INTEGRATION; an explicit, typed **signer
seam** so the gated money-path drops in cleanly (the shape, exercised with a mock signer — no real
keys); and a node-level **adversarial-relay** proof that a blind relay seeing every packet cannot
steal `S`, forge a settlement, or force a one-sided loss. The native bridge + real signing stay
human-gated.)

- [x] **G26 — Signer seam (mock-exercised, real signing still gated).** Define the explicit trait the
      node calls to turn a swap action into signed funding/claim `tx_wire` bytes (today `drive_swap`
      inlines stand-ins), and drive a full swap through a deterministic **mock** signer so the seam is
      proven the right shape for the real NIM/BTC signer to drop into later. No real keys, no broadcast.
- [x] **G27 — Adversarial blind relay cannot break a swap.** A node-level proof that a relay carrying
      every swap packet (it sees the whole flooded stream) still cannot extract `S` before the reveal,
      cannot forge a `FundingProof`/settlement the participants accept, and cannot force a one-sided
      loss by selectively dropping (the timeout refund + retransmit protect). Builds on the protocol
      `a_relay_with_the_wrong_secret_cannot_steal_a_leg`, raised to the node loop.
- [x] **G28 — Older swap docs sweep.** Fold the G8–G24 node-integration reality into `SWAP.md` /
      `FEASIBILITY.md` / `SWAP-ENGINE.md` (status blocks + a link to `MESH-INTEGRATION.md`) so the doc
      set is internally consistent and no longer describes only the pre-node-loop protocol. Docs only.

When the ladder is exhausted again, re-scan and append. (Re-scan after G28: the swap stack is built,
hardened, resilient, documented, and the docs are consistent. What's left, all sim/testnet and
non-gated, is **protocol completeness + safety hardening**: the responder accepts ANY amounts today
(no rate check); a participant has no cap on concurrent swaps (a Propose-spam DoS could outrun GC);
and an in-flight swap lives only in memory, so a crash with funds locked loses the refund path. These
close real gaps a real deployment needs, none needing a human decision.)

- [x] **G29 — Swap-rate acceptance policy.** A responder should not blindly accept any `give/take`
      amounts: give `SwapCoordinator`/`recv_propose` an acceptance policy (a min acceptable rate /
      tolerance band) and reject a lopsided proposal before accepting. Node-level test: a fair-rate
      proposal is accepted, a bad-rate one is rejected (no coordinator created), both over the loop.
- [x] **G30 — Concurrent-swap cap (anti-DoS).** A participant caps how many in-flight swaps it will
      hold, dropping new `Propose`s beyond the cap so a Propose-spammer cannot exhaust memory faster
      than the GC reaps. Test: past the cap, a fresh `Propose` is dropped (no new coordinator); a slot
      freed by GC/teardown lets a later one in again.
- [x] **G31 — Crash recovery of in-flight swaps (refund safety across restart).** `SwapSession` gains
      a `snapshot()` / `restore()` of its coordinators' essential state (swap_id, role, terms, phase,
      hashlock, initiator secret) so a node that restarts with funds locked can resume the refund
      tick. Test: snapshot a funds-locked swap, restore into a fresh session, and the refund tick
      still fires past the timeout — proving "worst case is a refund" survives a crash.

When the ladder is exhausted again, re-scan and append. (Re-scan after G31: the swap stack is built,
hardened, resilient, documented, rate/DoS/crash-safe — over the SIM mesh. The remaining sim/testnet,
non-gated work: G31's snapshot is in-memory structs (a node needs a BYTE codec to persist to disk);
crash recovery is proven at the session level but not yet wired into the `MeshNode` worker; and a
swap still assumes you already KNOW your counterparty (no over-mesh discovery). Native bridge + real
signing stay human-gated.)

- [x] **G32 — Snapshot byte codec.** Give `CoordinatorSnapshot` (and the session snapshot) a compact
      byte `encode`/`decode` so a node can persist recovery state to disk, with a round-trip proptest
      (any snapshot survives encode → decode) and panic-free decode of arbitrary bytes. Completes the
      G31 persistence story (structs → bytes). Mind the secret stays opaque, never logged.
- [x] **G33 — Node-level crash recovery.** Wire snapshot/restore into the `MeshNode` worker: a way to
      snapshot a participant's live session and a constructor that restores a participant from a
      snapshot. Node e2e: a participant funds a leg, is snapshotted, "restarts" from the snapshot, and
      its worker refund tick fires past the timeout — G31 proven over the real node loop, not just the
      session.
- [x] **G34 — Swap intent broadcast + match (discovery).** Today a swap assumes you already know your
      counterparty. Add a swap-intent message a node floods (give/take/rate it wants) and a matcher
      that, on a compatible complementary intent, kicks off a `Propose` — so two strangers in a dead
      zone can FIND each other before swapping. Sim/testnet; respects the rate policy + concurrency cap.
      Done: `swap_intent` module (`Asset`/`SwapIntent`/`would_initiate_against` one-sided NIM-giver
      rule + bounds-checked codec), `MessageType::SwapIntent = 0x45` blind-relayed, `handle_intent`
      in `swap_node` (decode → match standing intent → derive swap_id → initiate `Propose`), and
      `swap_discovery_tests` proving a complementary intent settles a swap while an incompatible-rate
      intent starts nothing. 5 unit + 2 node tests, 5/5 stable.

- [x] **G35 — Intent expiry / freshness.** A flooded `SwapIntent` lives forever today — a stale ad keeps
      matching long after the advertiser went offline or repriced. Add an `expiry_ms` (or `valid_for`)
      to the intent wire + a freshness check in `handle_intent` so an expired intent is decoded but NOT
      matched, and dropped from relay. Deterministic clock (pass the time in, like the chain-time grace
      seam) — never wall-clock in a test. Sim/testnet.
      Done: `SwapIntent.expiry_height: u64` (a chain height, the same deterministic clock the timelocks
      use via `ctx.cached_head()`) + `is_fresh(head)` + wire codec field. A freshness gate at the top of
      `handle_swap_packet` decodes a `SwapIntent` only to read its expiry — if `head > expiry_height` it
      returns before matching AND before relay, so a stale ad dies at every node (participant or pure
      relay). Tests: `is_fresh` boundary + round-trip; an expired-but-rate-crossing intent does not match
      (head pushed past expiry via a beacon); a relay forwards a fresh intent but drops an expired one
      (SpyRadio). 6 unit + 4 node tests, 8/8 stable.
- [x] **G36 — Intent anti-spam throttle.** A node can flood thousands of distinct intents to DoS matchers
      into spinning up coordinators (the concurrency cap protects swaps, not the match step). Add a
      per-sender intent rate/dedup gate (bounded recent-intent set keyed by sender, drop on overflow)
      so a hostile flooder can't exhaust a matcher. Mirror the existing relay-cache discipline; prove a
      flood of N intents yields at most the cap's worth of match attempts.
      Done: `IntentThrottle` in `swap_node` (per-sender admitted-match-attempt counter, cap
      `DEFAULT_INTENT_MATCH_CAP_PER_SENDER = 4` ≪ the 16-slot concurrency cap, with oldest-sender
      eviction bounding the table — purely count-based, no clock). `handle_intent` charges the
      flood's `sender_id` only for a genuinely-new matching intent (after the rate/dedup/cap checks),
      dropping once the budget is spent. Tests: pure throttle caps per sender independently; one
      flooder fills at most the cap while a DIFFERENT sender still matches. 8/10 discovery/intent
      tests green (added pure + node flood test), 10/10 stable.
- [x] **G37 — Intent re-advertise on no-match.** When a node floods an intent and nobody bites within a
      window, it should re-advertise (bounded retries, backoff) rather than going silent — the dead-zone
      case where the counterparty arrives later. Add a driver hook that re-emits the standing intent on a
      tick if still unmatched and not expired; cap the retransmits. Deterministic; sim/testnet.
      Done: `IntentAdvertiser` in `swap_node` (bounded exponentially-backing-off schedule — re-floods
      at ticks ~1/3/6/11/20, `DEFAULT_MAX_INTENT_READVERTS = 5`, gap capped, purely tick-counted so no
      clock). `readvertise_intent` in `gc_tick` re-floods the standing intent only while the node is
      unmatched (no coordinator) AND the intent is fresh (G35), resetting the budget once a swap forms.
      Tests (drive the real worker maintenance tick over `poll_sync`, count `SwapIntent` frames via a
      bytes-capturing `SpyRadio`): an unmatched node re-advertises exactly the cap then stops; a node
      in a swap re-advertises 0×; an expired standing intent re-advertises 0×. 12/12 stable.
- [ ] **G38 — Surface live intents in the demo UI.** The discovery layer is invisible. Add a read-only
      "open intents seen on the mesh" view to the demo (drive the real nimiq-ui registry components, vendor
      real assets per the branding rules) listing each peer's give/take/rate + freshness — no new money
      path, just a window onto what G34–G37 already flood. UI-gated only if it needs the native bridge.
