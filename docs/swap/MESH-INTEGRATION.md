# Mesh swap: node-side integration (G8–G24)

How a cross-chain HTLC swap (`SWAP.md`, `FEASIBILITY.md`, `BTC-LEG.md`) is driven over the real
`MeshNode` BLE-flood loop. The swap *protocol* (the `Swap` state machine, the HTLC escrows, the
`swap_wire` codec) is covered elsewhere; this maps the **node integration** built in goals G8–G24:
the layers that turn "a node receives a packet" into "a swap negotiates, funds, settles, refunds,
tears down, and survives a hostile / lossy / partitioned mesh."

Everything here is **sim / testnet**. The on-chain funding and claim transactions are opaque
stand-in bytes; building + signing real txs is the gated money-path (see "Sim vs money-path" below).

## The layers

```
                       a swap packet arrives on the radio (0x40-0x44)
                                          |
   engine::dispatch_packet  ── routes SwapPropose..SwapAbort ─►  swap_node::handle_swap_packet
                                          |
   swap_node (the hook + driver)   blind-relay onward (always)  +  if participant:
                                          |                          route to our session,
                                          |                          take the next chain action,
                                          v                          flood the reply
   swap_session::SwapSession   one SwapCoordinator per swap_id; routes by id; retransmit buffer
                                          |
   swap_coordinator::SwapCoordinator   one side of one swap: the protocol brain
                                          |
   swap::Swap (+ assess_ladder)   the clock-free, height-anchored state machine + Δ_safe gate
```

Each layer is a separate module with one job:

| Module | Type(s) | Responsibility |
|--------|---------|----------------|
| `swap_coordinator` | `SwapCoordinator`, `SwapContext`, `CoordError` | One side of one swap. Turns the message protocol into method calls (`recv_propose` / `recv_accept` / `fund` / `recv_funding_proof` / `claim_and_reveal` / `recv_reveal` / `settle` / `refund_after_timeout`) over a `Swap` state machine. Owns coordination only: no keys, no bitcoin, no tx bytes (those are opaque blobs it carries). |
| `swap_session` | `SwapSession`, `NodeIdentity`, `SessionError` | One node's view of all its in-flight swaps. Owns a `HashMap<swap_id, SwapCoordinator>`, routes each incoming packet to the right coordinator (spinning up a responder on a fresh `Propose`), and holds the per-swap retransmit buffer. Pure routing: no radio, no keys. |
| `swap_node` | free fns over `WorkerCtx` / `WorkerState` | The glue to the BLE worker. `handle_swap_packet` (blind-relay + participant route), `drive_swap` (the phase-driven chain-action driver), `gc_tick` (the maintenance tick), `flood_swap_reply`, `sync_swap_phases`. |
| `engine` / `node` | `WorkerCtx`, `WorkerState`, `MeshNode` | The worker thread. `WorkerState.swap: Option<SwapSession>` makes a node a swap **participant**; `WorkerCtx.swaps` is the FFI-observable phase mirror. `MeshNode::new_participant` builds a participant; `dispatch_packet` routes swap packets; the `SyncTick` job runs `maintenance_tick` + `swap_node::gc_tick`. |

## Participant vs relay (the privacy line, core value #3)

A swap rides the mesh as five opaque packet types: `SwapPropose` (0x40), `SwapAccept` (0x41),
`SwapFundingProof` (0x42), `SwapPreimageReveal` (0x43), `SwapAbort` (0x44).

- **Every** node **blind-relays** a swap packet onward exactly like a `nimiqTx`: dedup, remember (for
  G7 store-and-forward), degree-adaptive TTL relay. The relay **never parses** the swap, so terms,
  addresses, and the signed funding/claim blobs stay opaque to it.
- A node that also carries a `SwapSession` (`WorkerState.swap` is `Some`) is a **participant**. On top
  of relaying, it decodes its own `swap_id` off that otherwise-opaque stream, advances the matching
  `SwapCoordinator`, and floods any reply. A pure relay (`None`) does neither.

`MeshNode::new_participant(sender_id, radio, policy, identity, ladder)` builds a participant node;
`MeshNode::new` builds a plain relay.

## The happy-path flow (driven, no hand orchestration)

`handle_swap_packet` routes the incoming packet through `SwapSession::on_message`, then
`swap_node::drive_swap` takes this node's next phase-driven chain action and floods it. Each inbound
message triggers at most one action; the coordinator's `SwapPhase` is the idempotency guard, so a
replayed/duplicate message drives nothing twice.

```
  initiator (alice)                          responder (bob)
  ----------------                           ---------------
  start_swap → flood Propose  ───────────►   recv_propose → Accepted, flood Accept
  recv_accept → Accepted                ◄─────────────────────────────────────────
  drive: fund NIM → SelfFunded, flood FundingProof ─►  recv_funding_proof → InitiatorFunded
                                                       drive: fund BTC → BothFunded, flood FundingProof
  recv_funding_proof → BothFunded       ◄──────────────────────────────────────────────────
  drive: claim_and_reveal_sim → Revealed, flood PreimageReveal ─►  recv_reveal (extract S) → Revealed
  drive: settle → Settled                                          drive: settle → Settled
```

`SwapPhase` ladder: `Proposed → Accepted → {SelfFunded | InitiatorFunded} → BothFunded → Revealed →
Settled`, with `Aborted` and `Refunded` as the other two terminal exits. `has_funds_locked()` is true
for `SelfFunded | BothFunded | Revealed` (this node has funds in an HTLC, so a refund path must stay
open); `is_terminal()` is `Settled | Aborted | Refunded`.

## Sim vs money-path (what is gated)

The coordinator treats every on-chain tx as an opaque `Vec<u8>` (`tx_wire`) plus a 32-byte `tx_id`.

- **Sim (built):** `drive_swap` feeds stand-in bytes (`vec![0x11; 248]` for a NIM funding,
  `vec![0x22; 120]` for a BTC funding) and a deterministic `sim_tx_id`. The initiator reveals via
  `claim_and_reveal_sim`, which carries the secret `S` itself as the stand-in claim `tx_wire`; the
  responder extracts `S` from the first 32 bytes of the incoming `PreimageReveal`. The state
  transitions are real; the bytes are placeholders.
- **Money-path (gated):** in production a signer builds + signs real funding / claim txs (the NIM
  HTLC and the BTC P2WSH, see `BTC-LEG.md` / `BTC-KEY-SEAM.md`) and feeds them to the **same**
  `fund` / `claim_and_reveal` calls. No mainnet, no real funds, no live broadcast until that seam is
  wired and reviewed. `S` never crosses the FFI boundary; it leaves a coordinator only embedded in
  the reveal it floods, which is its entire purpose (it unlocks the counterparty's claim).

## The maintenance tick: GC + safety exit + retransmit

`MeshNode::poll_sync` enqueues a `SyncTick`; the worker runs `maintenance_tick` (G7 gossip-sync) then
`swap_node::gc_tick`. In production this fires on the periodic maintenance cadence. `gc_tick`, in
order:

1. **Retransmit (G20).** Re-flood each live swap's last-emitted action (`SwapSession::pending_retransmits`),
   so a dropped `Propose` / `Accept` / `FundingProof` / `PreimageReveal` is recovered by its emitter.
   The buffer is TTL-bounded (`RETRANSMIT_TTL = 32` ticks) and **decoupled from coordinator lifetime**,
   so a just-Settled initiator keeps re-flooding its `PreimageReveal` for a bounded window after it is
   reaped (the mesh's stand-in for the responder reading `S` off the on-chain claim).
2. **Refund safety exit (G18).** Any funds-locked swap whose own timelock has passed refunds itself
   (`refund_after_timeout` → `Refunded`). Worst case is always getting your own funds back.
3. **GC (G16).** Drop every coordinator that is terminal or **stale** (`is_stale`: non-terminal,
   no funds locked, and `head` past its `T_A` timelock). The dropped stale ids are returned so the
   node can flood a teardown `SwapAbort` for each (G19), freeing the counterparty's slot too.
4. **Mirror (`sync_swap_phases`).** Rebuild `WorkerCtx.swaps` from the session so the FFI-observable
   `swap_phase` reflects the live set (a GC'd swap vanishes from it).

The deadline throughout is the swap's own timelock (`SwapContext.terms.nim_timeout`, i.e. `T_A`); no
wall clock is consulted. With no head beacon the head stays 0, so nothing ever goes stale or refunds
prematurely.

## Teardown rules

- An inbound `SwapAbort` frees the slot **only if this node has no funds locked**. A counterparty
  abort must never drop our funded coordinator: that leg still has to refund via the timeout path.
- `SwapSession::cancel` returns an abort envelope (and drops the swap) only for an un-funded swap;
  a funded swap must refund, never abort.
- A replayed `Propose` for a `swap_id` we already track is dropped (never clobbers the live
  coordinator); a malformed payload is dropped without creating or corrupting any coordinator.

## Resilience properties and where they are proven

All node-level proofs live in `crates/nimmesh-core/src/swap_node_e2e_tests.rs`, driving real
`MeshNode` worker loops over the `mock_radio` virtual mesh. Hostile-input + retransmit-budget unit
proofs live in `swap_session.rs`; a fuzz proof lives in `tests/swap_session_proptests.rs`.

| Property | Proving test (`swap_node_e2e_tests.rs`) |
|----------|------------------------------------------|
| Participant hook: a Propose injected at a node is accepted + the Accept flooded | `a_responder_node_accepts_a_proposed_swap_injected_over_the_mesh` |
| Full lifecycle to Settled over the node loop, no hand orchestration | `two_participant_nodes_drive_a_full_swap_to_settled_over_the_real_mesh` |
| GC sheds a stale un-funded swap once the head passes its timelock | `the_worker_gc_tick_sheds_a_stale_swap_once_the_head_passes_its_timelock` |
| Node-level refund: a stalled funds-locked swap refunds itself via the tick | `a_stalled_funds_locked_swap_refunds_itself_via_the_worker_tick` |
| Abort emission: a GC'd swap tears down the counterparty over the mesh | `a_gc_abort_tears_down_the_swap_on_the_counterparty_over_the_mesh` |
| Lossy mesh: retransmit carries a swap through 30% packet loss | `a_swap_survives_a_lossy_mesh_via_retransmits` |
| Concurrency: N swaps over a lossy mesh, no one-sided settlement | `many_concurrent_swaps_all_settle_over_a_lossy_mesh` |
| Store-and-forward: a late-joining node catches up via gossip-sync | `a_rejoining_participant_catches_up_a_swap_via_store_and_forward` |
| Depth: a whole swap over a 5-hop blind relay line | `a_full_swap_rides_a_multi_hop_relay_line_end_to_end` |
| Mid-swap outage: partition while funds-locked, recover after heal | `a_swap_partitioned_mid_flight_recovers_after_the_heal` |

The recurring no-one-sided check: with no head beacon the head stays 0, so a coordinator can only
leave the session via `Settled → reap` (never stale, never refund). So "both sides go empty" proves
"both settled with each leg claimed." A swap wedged half-done would linger forever and fail the test.

## Not yet built (gated or future)

- **Money-path tx signing** (real NIM/BTC funding + claim) and any mainnet / real-fund / live-broadcast
  action: gated on human review.
- **The native WebView↔Rust bridge** and an FFI surface for app-driven swap origination/observation:
  `start_swap` / `swap_phase` / `cancel` are crate-internal test hooks today; the real money-path
  origination API is a later, gated goal.
- **Reveal delivery hardening beyond retransmit:** a real HTLC swap learns `S` from the on-chain
  claim; the mesh `PreimageReveal` plus its bounded retransmit is the offline stand-in.
