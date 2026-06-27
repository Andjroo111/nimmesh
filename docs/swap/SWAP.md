# nimiq.nimmesh — Mesh Swap (offline cross-chain atomic swaps over Bluetooth)

> **Feature branch:** `feat/mesh-swap`. A future feature layered on the shipped nimmesh
> mesh stack. The spec; the build contract is [SWAP-LOOP.md](./SWAP-LOOP.md); the protocol
> base is [../PROTOCOL.md](../PROTOCOL.md); the offline-payment hazards are
> [../RISKS.md](../RISKS.md). This document does **not** edit the base GOAL/LOOP/PROTOCOL
> files — it is purely additive so it never collides with concurrent work on `main`.

## One line

**Swap coins with a stranger when neither of you has internet.** Two phones meet in a dead
zone — Alice has NIM and wants BTC, Bob has BTC and wants NIM. They negotiate a **trustless
HTLC atomic swap** entirely over the Bluetooth mesh; the signed funding and claim
transactions ride the mesh and each leg settles **on its own chain at the first internet
hop**. No exchange, no custodian, no internet between them. If either side walks away, the
hash-timelocks refund both — nobody can be cheated.

## Why this is a natural fit for nimmesh (not a bolt-on)

nimmesh already ships the entire transport this needs:

| Swap needs | nimmesh already has |
| --- | --- |
| A self-contained, broadcast-safe signed tx that travels any transport | G3 byte-exact Albatross signer + the ~139 B self-authenticating blob |
| Flood / multi-hop relay / dedup of those blobs | G4 codec · G6 relay · TTL/hop · LRU dedup |
| Delivery to an offline peer who rejoins | G7 GCS store-and-forward (15 min catch-up, inside the validity window) |
| The one online hop that puts a tx on-chain | G8 gateway (`sendRawTransaction`) |
| A height clock to drive timelocks without a wall clock | G9 head-beacon + `HeadCache` |
| "pending → settled" closure for **both** parties | G17 settlement ledger (`Outgoing` + `Incoming`) |
| A private 1:1 channel for the negotiation | G11 Noise_XX (mutual-auth, identity-hiding) |

The **only new primitives** are: (1) Nimiq **HTLC** transaction serialization (today the
signer only builds `Basic` transfers), (2) a handful of **swap wire messages**, and (3) a
**swap state machine**. Everything else is reuse.

### The Nimiq edge (load-bearing, same as the base project)

- **Nimiq has a native HTLC account type** — basic / vesting / **HTLC** / staking. No smart
  contract to deploy, no relayer to run. The contract itself is the escrow, with three
  unlock paths: `timeout-resolve` (refund to the funder after the timeout), `regular-transfer`
  (recipient claims with the hash preimage), and `early-resolve` (both parties sign). The
  `regular-transfer` + `timeout-resolve` pair **is** an atomic-swap HTLC.
- A signed Nimiq tx is **self-contained and self-authenticating**, so an HTLC funding tx and
  an HTLC claim tx are just more ~139–250 B blobs the existing mesh already knows how to carry.
- **1-second blocks** → once a gateway is reached, "settled" comes back over the mesh fast,
  and block-height timelocks have fine granularity.

## The protocol — a standard HTLC atomic swap, mesh-transported

Notation: secret **S** (32 random bytes, known only to the initiator), hashlock
**H = SHA-256(S)**. Alice is the **initiator** (NIM → wants BTC); Bob is the **responder**
(BTC → wants NIM).

```
        Alice (NIM, knows S)                 mesh (BLE, offline)            Bob (BTC)
  1. propose: X NIM ⇄ Y BTC, H, addrs, T  ──swapPropose(Noise 1:1)──▶  accept / counter
  2. accept                                ◀──swapAccept(Noise 1:1)───
  3a. sign NIM HTLC-create  (lock X NIM    ──swapFundingProof─────────▶  (relays onward,
      to H, refund Alice, timeout T_A)        flood → NIM gateway broadcasts   a gateway
  3b. (Bob) sign BTC HTLC fund (lock Y BTC ◀──swapFundingProof─────────  broadcasts BTC)
      to H, refund Bob, timeout T_B<T_A)      flood → BTC gateway broadcasts
  4. Alice claims Bob's BTC HTLC by         ──swapPreimageReveal───────▶  Bob reads S,
     signing a BTC claim tx revealing S        (the claim tx, broadcast-   claims NIM HTLC
     (before T_B) → floods → BTC gateway       safe, reveals S on-chain     with S (before
     broadcasts → S is now public            ─ AND in the mesh message)     T_A) → NIM gw
  5. both legs settle  ──nimiqTxReceipt / chain confirmation──▶  pending → settled (G17)
```

**Refund / abort:** if Bob never funds, Alice's NIM HTLC `timeout-resolve` refunds her after
`T_A` (she lost nothing but time). If Alice funds and then vanishes without revealing S, Bob's
BTC `timeout-resolve` refunds him after `T_B`, and Alice's own NIM HTLC refunds her after
`T_A`. **No counterparty risk beyond the timelock wait** — that is the whole point of an HTLC
swap, preserved verbatim over the mesh.

### The critical safety invariant — timelock laddering vs. the mesh budget

The initiator's timeout must exceed the responder's by a margin that **dominates the
worst-case mesh-propagation + on-chain-confirmation time**:

```
T_A  −  T_B   ≥   Δ_safe   =   max-mesh-propagation + counterparty-claim-confirmation + slack
```

Why: Bob only learns **S** when Alice claims his BTC HTLC (which she must do before `T_B`).
Bob then needs enough time to claim the NIM HTLC before `T_A`. If `T_A ≤ T_B`, Alice could
claim BTC at the last second before `T_B`, revealing S too late for Bob to claim NIM before
`T_A` — Alice keeps both. So `T_A > T_B` with a buffer.

**This is where the offline/mesh context changes the numbers, and it is the headline design
risk.** On the open internet Δ_safe is minutes; over a store-and-forward BLE mesh, **S can
take up to the ~15-minute S&F catch-up window (plus multi-hop airtime) to reach the
counterparty or a gateway.** So nimmesh sets Δ_safe **conservatively in hours, not minutes**,
and:

- ties both timeouts to **block height** (the G9 head-beacon is the mesh's clock — no wall
  clock is trusted),
- keeps the whole swap inside Nimiq's ~2 h validity window per funding tx (re-sign/re-anchor
  if the window lapses before broadcast),
- makes the **preimage-reveal message carry the signed claim tx itself** (§ below) so that
  revealing S *is* broadcasting the claim — any gateway that sees it both settles Alice's BTC
  leg and propagates S to Bob in one shot, minimizing the window where S is "revealed but
  unclaimable."

`swap.rs` computes the timeout ladder from the head height + a configurable, conservative
`Δ_safe` and **refuses to enter `Funded` if the ladder is unsafe** for the current mesh
reachability (G16) — an honest "this swap can't be made safe offline right now" beats a
silent loss.

### What rides the mesh (new message types, swap range `0x40–0x4F`)

Reserved from PROTOCOL.md's free `0x23–0xFF` range, distinct from the base `nimiq*`
(`0x30–0x34`) and standard (`0x11/0x20/0x21`) types:

| type | name | transport | privacy | payload |
| --- | --- | --- | --- | --- |
| `0x40` | `swapPropose` | 1:1 | **Noise XX** | terms: amounts, H, both chains' addresses, proposed `T_A`/`T_B`, networkIds |
| `0x41` | `swapAccept` | 1:1 | **Noise XX** | accept / counter-terms + responder addresses |
| `0x42` | `swapFundingProof` | flood | public | the signed HTLC **funding** tx blob (broadcast-safe) + which leg + txId |
| `0x43` | `swapPreimageReveal` | flood | public | the signed **claim** tx blob that reveals S (broadcast-safe) + txId; S derivable from it |
| `0x44` | `swapAbort` | 1:1 | **Noise XX** | signed intent-to-abort (pre-funding) — courtesy only; the timelock is the real guarantee |

Rationale for the split: the **negotiation** (`0x40/0x41/0x44`) is a private deal between two
people → it rides the existing G11 **Noise_XX** 1:1 channel (identity-hiding, mutual-auth).
The **funding and claim tx blobs** (`0x42/0x43`) are inherently public (they go on-chain), are
self-authenticating, and must reach *any* gateway → they **flood** exactly like a `nimiqTx
0x30`, reusing G6 relay + G7 store-and-forward + G8 gateway. A relay still carries everything
**blind**: it never parses a swap, never learns the deal, just forwards opaque bytes (core
value #3).

A swap TLV envelope (Bitchat-style `type|len|value`, mirroring the existing Nimiq TLV in
`envelope.rs`) carries: `swapId(16)`, `role`, `H(32)`, `leg(NIM|BTC)`, `txWire`, `txId(32)`,
`timeoutHeight(u32)`, `counterpartyAddr`, `networkId`.

## Chain-agnostic by construction — the `SwapLeg` seam

Mirroring the existing pluggable `KeyOrigin` / `EnclaveKey` pattern, each chain is a
`SwapLeg` implementation behind one trait, so `swap.rs` (the state machine) and the mesh
never learn which chains are involved:

```rust
/// One side of a swap on one chain. NimiqLeg is built now (native HTLC); BitcoinLeg is a
/// documented stub whose real impl is gated (real BTC node + real funds → needs:owner).
trait SwapLeg {
    fn fund_htlc(&self, terms: &SwapTerms, h: Hashlock) -> Result<SignedTx, SwapError>;
    fn claim_with_preimage(&self, htlc: &FundedHtlc, s: Preimage) -> Result<SignedTx, SwapError>;
    fn refund_after_timeout(&self, htlc: &FundedHtlc) -> Result<SignedTx, SwapError>;
    fn extract_preimage(&self, claim_tx: &SignedTx) -> Option<Preimage>; // learn S from the air/chain
    fn settlement_txid(&self, tx: &SignedTx) -> TxId;
}
```

- **`NimiqLeg`** — built on the byte-exact Albatross signer extended from `Basic` to `HTLC`
  (creation tx + the `regular-transfer` and `timeout-resolve` resolve proofs). Testnet,
  headless, fully tested.
- **`BitcoinLeg`** — a **stub** in this loop: the trait surface + a mock implementation good
  enough to drive the end-to-end test. The **real** P2WSH-HTLC signer + a BTC gateway/watcher
  handle real funds → **gated `needs:owner`** (Phase G-real).

## Demo loop (the one thing that proves it — headless, in CI)

Two `MeshNode`s in the `MockRadio` virtual mesh with **no link between them except other
nodes** (force multi-hop + store-and-forward), a `MockGateway` for NIM and a mock BTC
gateway. The test drives `propose → accept → both-fund → reveal-over-mesh → both-settle` and
asserts **atomicity**: either *both* legs settle or *both* refund, never one-sided. Plus the
adversarial paths:

1. **Happy path** — full swap settles both ways; S propagates only over the mesh.
2. **Responder never funds** — Alice's NIM HTLC refunds after `T_A`; Bob is untouched.
3. **Initiator vanishes after funding** — both timeout-refund; no one-sided loss.
4. **Unsafe ladder** — mesh reachability (G16) too poor for a safe `Δ_safe` → `swap.rs`
   refuses to fund and surfaces an honest "can't be made safe offline now."
5. **Blind relay preserved** — the relay node in the middle settles nothing, parses nothing.

In CI this runs against `MockMeshTransport` + mock gateways, so every layer except the
physical radio and the real BTC chain is testable headless — identical to how the base send
loop is proven.

## Core values (inherited; every swap decision scored against these)

1. **Non-custodial by construction** — seeds never cross FFI/mesh; only signed, broadcast-safe
   blobs and public hashlocks ride the air. The HTLC contract is the only escrow.
2. **Trustless relay** — relays forward opaque swap bytes; they cannot alter, front-run, or
   block a deal, and never learn its terms.
3. **No counterparty risk beyond the timelock** — the HTLC refund path is the guarantee; the
   UI never shows a swap as done before on-chain claim/settlement (G17 honesty).
4. **Offline-first** — the whole negotiation + funding + reveal happens with no internet
   between the parties; connectivity is needed only at each leg's gateway hop.
5. **Honest about the offline cost** — `Δ_safe` is conservative and reachability-gated; an
   unmakeable-safely swap is refused, not silently risked.
6. **Testnet-by-default, mainnet/real-funds gated** — the NIM leg defaults to testnet
   (networkId 5); the real BTC leg and any mainnet/real-fund action are `needs:owner`.

## What this is NOT

- Not a custodial swap, not an order book, not a DEX with a matching engine (a sibling
  project — `nimiq.dex` — owns on-internet cross-chain matching; this is the **offline,
  in-person, single-counterparty** case).
- Not a claim that funds settle without ever touching the internet — each chain still needs
  one gateway hop. The novelty is that **everything between two people is offline**.
- Not mainnet, and not the real BTC leg, in this autonomous loop — those are gated.

## Success criteria

- **CI / headless:** the full propose→fund→reveal→settle loop + all four adversarial paths
  pass against the mock mesh + mock gateways on every PR.
- **Byte-exactness:** every Nimiq HTLC funding/claim/refund tx is asserted equal to
  `@nimiq/core` 2.7.0 output, byte-for-byte, via committed fixtures (the F1 gate).
- **Safety:** no test ever produces a one-sided settlement; the unsafe-ladder path is proven
  to refuse.
- **Honesty:** a swap is never shown "done" before its on-chain claim; refunds are surfaced.
