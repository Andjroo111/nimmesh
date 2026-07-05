# ADR-0009 — Mesh relay incentives: reward availability (uptime) + utility (throughput)

**Status:** proposed (2026-07-05) · **Context:** Andjroo wants people to self-host home mesh
nodes, with **two** incentives — one for *having a node up* (uptime) and one for *a
transaction passing through it* (usage). This ADR fixes the model so we build the measurement
and settlement primitives correctly. Builds on [RELAY-INCENTIVES.md](../RELAY-INCENTIVES.md).

## Decision

Model relay rewards as **two independent components**, because they reward two different scarce
resources and have different anti-cheat problems:

1. **Availability reward (uptime)** — pay a node for being present and reachable over time.
   Rewards the cost of leaving hardware on. Scarce resource: *committed uptime*.
2. **Utility reward (throughput)** — pay a node when a real payment passes through it. Rewards
   actually carrying traffic. Scarce resource: *useful relay work*, especially *gatewaying*
   (reaching the internet to broadcast — the expensive, rate-limited step that lands a payment
   on-chain).

Both settle **on-chain in NIM, non-custodially** (HTLC / fee outputs) — never through a
custodian, never a new token, and relaying stays **free-by-default** (tips are opt-in on top).
The `mac-node` already measures both dimensions locally (uptime + `relayStats().payments_relayed`);
this ADR specifies how to make them *trustlessly attestable* and how they settle.

## The two hard problems (and the honest state of each)

Rewarding a permissionless mesh invites **lying** (claim work you didn't do) and **Sybil**
(spin up many fake nodes). The design must make honest contribution cheaper to prove than to fake.

### Availability — proof-of-availability via witnessed heartbeats

- Each node emits a periodic **signed heartbeat** (node key + monotonic counter + coarse
  epoch). Nearby peers that receive it record a **witness attestation** (they sign "I heard
  node X at epoch T"). Accumulated attestations **from distinct, established peers** = proof the
  node was up and reachable during those epochs.
- **Anti-Sybil:** attestations only count from peers that are themselves attested and distinct
  (a clique of fake nodes vouching for each other is bounded by needing to reach *real* peers /
  the gateway). Optionally **stake-weight** witnesses (a node bonds a small NIM stake, slashable
  for provably false attestations) so fabricating uptime costs real capital.
- **Settlement:** a **community/foundation availability pool** pays pro-rata by attested
  availability-epochs per settlement window. (Precedent: nimiq.cool's pro-rata staking pool, but
  the weight is attested uptime, not stake.) This is the part that needs the most anti-Sybil
  care and ships **after** utility.

### Utility — proof-of-relay anchored at the gateway

- The **gateway** (the node that broadcasts the tx to the Nimiq chain) is the only node in a
  *trusted* position: it sees the real, valid tx and its broadcast is publicly verifiable
  on-chain. So anchor utility rewards there.
- **v1 (tractable, ship first):** the **sender optionally attaches a mesh tip** to a payment.
  Whichever gateway broadcasts it **claims the tip on-chain** (a normal fee/HTLC output keyed to
  a preimage only the broadcasting gateway can complete). Non-custodial, decentralized (any
  gateway can win), Sybil-resistant on the gateway side (you must actually reach the chain).
  Fair-race rule: first-confirmed broadcast wins; the loser's claim is a chain-level no-op.
- **v2 (path split):** intermediate relays collect **signed relay receipts** (each hop signs
  "I forwarded packet P from prev→next"). A valid receipt chain lets the gateway split the tip
  along the proven path. Heavier (crypto+bandwidth per hop) and collusion-prone, so it's a
  follow-on, not v1.

## Why two components, not one blended score

A single blended "contribution score" is farmable (idle Sybil nodes inflate uptime with no real
traffic). Keeping them separate lets each carry its own anti-cheat: **utility** is anchored by
an on-chain broadcast (hard to fake, ships first, self-funding via tips); **availability** needs
the heavier attestation+stake machinery and a funded pool (ships later). A node that's merely up
but carries nothing earns only the (smaller, pool-capped) availability reward; a node on a busy
path earns real utility tips. That matches Andjroo's intent: *pay for uptime AND pay for use*,
weighted so use pays more.

## Scoring against core values

| Value | How this model holds it |
| --- | --- |
| Non-custodial | Rewards settle on-chain (fee/HTLC outputs); we never hold funds |
| Decentralized | Any gateway can claim a tip; no central paymaster |
| Censorship-resistant | Relaying stays free-by-default; tips are opt-in, never required to send |
| Financially inclusive | Free offline pay preserved; anyone can run a node and earn |
| Sustainable | Utility is self-funded by senders; the pool is bounded, not inflationary |

## What to build, in order

1. **[shipped] Local measurement** — `mac-node` tracks uptime + payments-relayed + a projected
   reward (illustrative rates), persisted in `~/.nimmesh-relay/stats.json`. Makes both
   dimensions concrete before any economics exist.
2. **Utility v1** — the sender-attached mesh tip + gateway-claims-on-broadcast HTLC output.
   Prototype on testnet against the Mac gateway. **Reuses the swap money-path (slice 2/3)** —
   same HTLC settlement, so build the swap first, then generalize its settlement into the tip.
3. **Availability** — the heartbeat + witness-attestation format (a mesh protocol change, so
   coordinate with the core/swap session), the stake-bond, and the pro-rata pool.
4. **Utility v2** — signed relay-receipt path split, once availability's attestation layer exists
   to build on.

## Consequences

- The heartbeat/attestation format is a **mesh protocol addition** — it belongs in the Rust core
  (owned by the swap/core track), so availability work must be coordinated, not built solo in
  the app or mac-node.
- The illustrative reward rates in `mac-node` are placeholders; they carry a loud "pending
  ADR-0009" label and must not be read as promised value until this ADR's economics are ratified.
- Utility-first ordering means the **swap money-path is the critical path for incentives too** —
  finishing it unlocks the tip mechanism.
