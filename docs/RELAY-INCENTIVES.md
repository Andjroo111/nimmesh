# Home mesh relays + incentives — design exploration

> Andjroo, 2026-07-05: "Should we turn this into a feature so people could self-host home
> mesh nodes to help the network? Can we incentivize this in the future?"

Short answer: **yes to the feature — it's a small step from the Mac node we just built** —
and **yes-but-carefully to incentives**, because trustlessly paying anonymous multi-hop
relays is one of the genuinely hard problems in mesh networking. This doc lays out the
design space so we build the incentive layer right, not fast.

## Where we already are

- `mac-node/` is a real, always-on mesh peer running the production Rust `MeshNode` over
  CoreBluetooth. A "home relay" is ~90% this, plus an always-on posture and a nicer wrapper.
- The core already tracks relay contribution: `citizen.rs` has `RelayPosture`
  (Full/Reduced/Frugal/Off, battery-aware) and a `payments_relayed` counter per node, surfaced
  as `relayStats()`. So "how much did this node help" is already a first-class, measured value.
- The mesh is **store-and-forward + flood with dedup/TTL** (G6/G7): a signed Nimiq tx is a
  self-contained ~139-byte blob that hops phone-to-phone until a device with internet (a
  "gateway") broadcasts it. Relays never custody funds — they carry opaque bytes.

That last point is the whole reason incentives are hard **and** the reason they can stay true
to Nimiq's values: relays are dumb carriers, which is great for censorship-resistance and
privacy, but it means a relay can't *prove* it carried something, and can *lie* that it did.

## Part 1 — The home-relay feature (buildable now, no economics)

A "Nimiq Mesh Relay" anyone can run to strengthen the network. Three tiers, increasing reach:

1. **Desktop relay (Mac/Windows/Linux).** The `mac-node` generalized: always-on, launches at
   login, BLE + (later) LAN/internet bridging. A Mac/PC left on at home is a 24/7 anchor that
   bridges the local BLE mesh to the internet — i.e. a **gateway**, the most valuable node type
   (it's what actually lands payments on-chain). This is the highest-leverage tier and it's the
   least gated: no App Store, no device limits.
2. **Phone relay (already shipping).** The app itself relays whenever it's open/backgrounded.
   The Network screen already frames this ("you ARE the network") and shows live peers/relayed.
3. **Dedicated hardware (future).** A Raspberry Pi image (we already run a Pi for Firefly) — a
   $35 always-on gateway. The dream: a "Nimiq Mesh Pi" you flash and forget.

**None of this needs an incentive layer to ship** — it ships on the same "help the network,
be a first-class citizen of the blockchain" motivation Nimiq's own network screen uses. Start
here; it's real utility and it makes the mesh denser, which is what makes swaps and offline
pay actually work.

## Part 2 — Incentives (the hard, careful part)

The goal: reward nodes for relaying and (especially) for gatewaying, **without** a custodian,
**without** a central paymaster, and **without** breaking the privacy that makes relays dumb.
Score every option against Nimiq's core values (non-custodial, decentralized,
censorship-resistant, financially inclusive, sustainable).

### The core difficulty

A multi-hop flood has no trusted record of *which* nodes carried a given packet. So a naive
"pay per relay" invites two attacks: **lying** (claim relays you didn't do) and **Sybil**
(spin up 1000 fake nodes to farm rewards). Any credible scheme has to make honest relay
cheaper to prove than to fake.

### Option A — Non-monetary first (reputation / status) ✅ ship this now

Leaderboards, badges, "you relayed N payments this week", the green peer hexes, streaks,
a shareable "network contribution" card. Costs nothing, has zero attack surface (bragging
rights aren't worth Sybiling hard), and it's exactly how Nimiq already frames the network
screen. **Recommendation: build this as the first "incentive" — it's real, safe, and on-brand.**
Score: perfect on all values; sustainable; financially inclusive (anyone can earn status).

### Option B — Gateway fee-sharing (the tractable monetary path) ⭐ most promising

Only the **gateway** (the node that broadcasts to the chain) is in a trusted position — it
sees the real tx and is the natural fee-collection point. Model: the **sender optionally
attaches a small mesh tip** to a payment; whichever gateway broadcasts it claims the tip via a
normal on-chain output. This is non-custodial (it's just a fee output), decentralized (any
gateway can claim), and Sybil-resistant on the *gateway* side (you must actually reach the
chain and broadcast to win). It does **not** pay intermediate relays — but gatewaying is the
scarce, expensive resource (needs internet + uptime), so paying gateways is where the real
economic gap is. **This is the one I'd design toward first.** Open question: race/fairness when
two gateways broadcast the same tx (first-confirmed wins; the loser's claim is a no-op).

### Option C — Proof-of-relay receipts (research-grade, later)

Each hop collects a signed "I received this from you" ack, building a path proof a payer or
pool can reward. Real but heavy: adds crypto + bandwidth to every hop, and collusion
(two nodes vouching for each other) is hard to fully kill. Interesting for a v2; not a v1.

### Option D — Community relay pool / staking (needs anti-Sybil) 

A community-funded pool pays relays pro-rata by `payments_relayed`, like nimiq.cool's staking
pool but for relay work. Elegant, but stands or falls on Sybil resistance — pro-rata by an
un-verifiable counter is farmable. Would need Option C's proofs or a stake-weighting to be safe.

### What to avoid

- **A new token.** Nimiq is NIM; inventing a relay-token would fragment value and read as a
  cash-grab. Pay in NIM (or the swapped asset) or in status. (Rule me out fast if I ever
  propose a token.)
- **Custodial escrow of relay rewards.** Breaks the #1 core value. Whatever we do settles
  on-chain via HTLC/fee outputs, never through us holding funds.
- **Mandatory fees.** Offline pay for the financially excluded is the mission; relaying must
  stay free-by-default. Tips are opt-in, on top.

## Recommended path

1. **Now:** ship the home-relay feature (Part 1, desktop-first) + Option A status/leaderboard.
   Pure upside, no economics, denser mesh. Denser mesh is the prerequisite for everything else.
2. **Next:** design Option B (gateway fee-sharing) as an ADR — the sender-attached mesh tip +
   gateway-claims-on-broadcast. Prototype on testnet against the Mac gateway.
3. **Later / research:** Option C proofs, then Option D pool if proofs hold up.

## Relationship to the swap work

Same primitive, reused: the HTLC that settles a swap is the same on-chain settlement a gateway
tip rides on. Building the swap money-path (slice 2/3) builds most of what Option B needs. So
the sequence is: finish the swap → generalize its settlement into the gateway-tip → then the
richer relay economy. The Mac node is the shared test rig for both.
