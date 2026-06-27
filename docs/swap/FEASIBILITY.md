# Mesh Swap — feasibility verdict

> **The question:** can you do an HTLC atomic swap over a Bluetooth / BLE mesh? (A second
> opinion said "the HTLC will not work over Bluetooth or a mesh network.")
> **The verdict:** *Yes — as long as you're precise about what "over Bluetooth" means.* The
> second opinion is **half right**, and the half it's right about is the half nobody is
> proposing to do.

## The one distinction that resolves it

> **You don't settle the swap over Bluetooth. You *create and relay an offline transaction*
> over Bluetooth, and it settles on-chain at the first internet hop.**
> — exactly the model nimmesh already ships for payments, extended from one step to a few.

| | "Over Bluetooth" meaning | Possible? |
| --- | --- | --- |
| ❌ | **Settle** the HTLC purely over Bluetooth — lock, claim, refund, with the internet *never* involved | **No — and this is physically impossible.** An HTLC locks funds on two real blockchains. Locking / claiming / refunding are on-chain actions; each needs a transaction broadcast = internet. Bluetooth carries bytes; it cannot settle a blockchain transaction. |
| ✅ | **Coordinate + transport** the swap over Bluetooth — negotiate, exchange *signed* funding/claim txs and the secret over the mesh; **gateways** broadcast on-chain and beacon confirmations back | **Yes.** The two people need no internet *between* them — only for the mesh to eventually reach one node that has a connection. |

A Nimiq HTLC funding tx and an HTLC claim tx are **self-contained, self-authenticating
~150–250 B blobs** — the same kind of blob nimmesh already floods, relays, store-and-forwards,
and broadcasts at a gateway. The swap just adds a state machine + a few message types on top.

## What the feasibility test actually proved (`scripts/fixtures/feasibility-test.mjs`)

| Test | Result |
| --- | --- |
| Build a real Nimiq **HTLC funding tx** → run `@nimiq/core`'s **own validator** on it | **✅ ACCEPTED** (248 B, real Ed25519 signature) |
| Nimiq has a **native HTLC account type** + all redeem-proof variants in the official lib | ✅ first-class protocol feature (not an add-on) |
| Nimiq ships **NIM↔BTC atomic swaps in its production wallet** today | ✅ the on-chain swap mechanism is already proven on mainnet |
| `@nimiq/core` **JS** can sign/verify an HTLC **redeem** (claim) | ❌ **deliberately not** — `sign()` throws, `fromPlain` needs the raw bytes, the deserializer rejects hand-built proofs |

The last row is a **JS-binding limitation, not a protocol limitation** (Albatross validators
enforce HTLC redemption — that's why the chain has the proof types). It only changes *how we
test* the claim path: byte-verify it against Nimiq's **Rust** reference (`core-rs-albatross`)
or a **live testnet broadcast**, not the JS lib. See [F0-HTLC-FINDINGS.md](./F0-HTLC-FINDINGS.md).

## The real hard part (this is probably what the second opinion sensed)

The risk was never "does the HTLC work" — it does. It's **"is it safe and useful over a
high-latency, partition-prone mesh?"** Two honest issues, both designed for in [SWAP.md](./SWAP.md):

1. **The timelock safety race.** Whoever reveals the secret must do it with enough margin for
   the counterparty to claim before *their* timelock expires. On the internet that margin is
   minutes; over a store-and-forward BLE mesh (catch-up window up to ~15 min, multi-hop,
   possible partitions) it must be **hours**. Too tight → a theft window. Mitigations:
   - the **claim tx itself puts the secret on-chain**, so the counterparty can learn it from
     *any* gateway, not only the mesh (the safety anchor is the chain, not Bluetooth);
   - a **reachability gate** (`Δ_safe`) that **refuses to fund** a swap that can't be made
     safe offline right now — an honest "not safe to swap here" beats a silent loss.
   - Net: **worst case is always a refund, never a theft.**
2. **Liveness is the real cost, not atomicity.** Done right nobody ever loses funds, but many
   swaps will be *slow* or *fail-and-refund* when connectivity is poor. Safe ≠ always fast.

## Honest scope

- This is a **real, buildable feature**, but **narrower in value than offline *payments*.** The
  sweet spot: two parties with no direct internet but a mesh-reachable gateway, who'd accept a
  slow, conservative-timelock, trustless swap rather than use an exchange/custodian.
- The autonomous foundation (Nimiq HTLC serialization + the swap protocol + a mock end-to-end
  test whose **whole job is to prove no one-sided settlement is ever possible**) validates the
  design safely. The **real cross-chain (BTC) leg and any real funds stay gated** to Andjroo.

**Bottom line:** if "over Bluetooth" means *fully-offline settlement*, that's impossible and the
second opinion is right. If it means *create an offline transaction and let the mesh carry it to
a gateway* — which is the actual design — it works, it's the same transport nimmesh already
proved for payments, and the on-chain HTLC primitive is verified above.
