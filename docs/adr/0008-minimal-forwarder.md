# ADR-0008 — The minimal forwarder: relayer-sponsored FUNDING, caller-open CLAIMS

**Status:** accepted (2026-07-04) · **Context:** implements the decision half recorded in
**ADR-0006** (G7, #78): relayer-sponsored gas via EIP-2771 with a mandatory self-funded
fallback. Building it surfaced one clarifying insight and a handful of contract-shape
decisions; this ADR records them. The live proof ran 2026-07-04 (`docs/swap/AMOY.md`).

## The clarifying insight: the forwarder is for FUNDING, not claiming

ADR-0007 made `withdraw`/`refund` **caller-open with fixed payouts** — any gas-paying party can
land the recipient's claim directly, so "relayer-sponsored claims" need NO meta-tx machinery at
all: the recipient hands `(swapId, S)` to whoever pays gas (subject to ADR-0006's handing-S-is-
a-reveal failover rule). What a plain relayer CANNOT do is create the escrow on the user's
behalf: `newSwap*` binds the funder to `_msgSender()`, and the EIP-2612 permit verifies against
that same identity — a relayer calling directly would try `permit(owner=relayer, …)` against
the USER's signature and fail. **The forwarder exists to attribute the FUNDER**: relayer
submits, `_msgSender()` recovers the signer, the user's tokens fund the escrow. Gasless both
directions, one small contract.

## Decisions

1. **Hand-rolled minimal forwarder, no OZ** (`contracts/src/NimmeshForwarder.sol`) — the repo's
   no-framework rule. One EIP-712 struct (`ForwardRequest{from,to,value,gas,nonce,deadline,
   data}`), strictly sequential per-signer nonces, a required deadline, `DOMAIN_SEPARATOR`
   public for off-chain reads.
2. **Struct calldata, not flat args.** Ten flat parameters blew the EVM stack ("stack too deep");
   the calldata struct is one stack slot and is the canonical minimal-forwarder shape anyway.
   The Rust side hand-builds the one dynamic-tuple encoding (`evm_forward::forwarder_execute`),
   byte-anchored against `cast calldata`.
3. **Target failures are REPORTED, not bubbled.** `execute` returns `(success, ret)` and emits
   `Forwarded(from, to, nonce, success)`; the nonce burns either way. A relayer should not lose
   its whole transaction to a target-side revert — and observers must check effects
   (`getSwap`) or the event, never assume.
4. **EIP-150 under-gas guard.** After the inner call, `execute` requires `gasleft() ≥ gas/63` —
   a relayer that under-provisions the outer transaction cannot disguise the resulting inner
   OOG as a target failure; the whole tx reverts loudly (`InsufficientRelayGas`).
5. **A fresh HTLC deployment bound to the forwarder.** `NimmeshHtlc.trustedForwarder` is
   immutable by design (ADR-0007) — G6's deployment stays forwarder-less; G7 deployed the
   forwarder plus a second HTLC pointing at it. Both remain live on Amoy; the app targets the
   forwarder-bound one.

## Proof

- **Foundry** (8 tests): gasless funding attributes the funder to the SIGNER (never the
  relayer/forwarder); the escrow settles via the caller-open claim; replay (`WrongNonce`),
  expiry, tampered-calldata and forged-`from` all rejected (`BadSignature` — the signature
  covers every field including the calldata); a target failure is reported with the nonce
  burned and no escrow created; `verify` is an honest preview.
- **Rust offline**: `ForwardRequest` typehash, request digest (binds the calldata), and the
  full `execute` calldata byte-anchored against cast-derived vectors.
- **Live (2026-07-04, Amoy)**: an in-process-derived user with **zero POL and account nonce 0**
  signed a permit + forward request; the relayer submitted through the forwarder; `getSwap`
  read `funder == user`; the relayer landed the caller-open `withdraw(S)`; the user's nonce and
  POL were still **0** at the end. Receipts: `docs/swap/AMOY.md`.

## What stays open / gated

The relayer *economics* (fee skim, who runs the service) are product decisions deferred with
the rest of the productionization; the S-handoff failover constant (submit-self if not mined
within k blocks) gets wired when the engine drives this path (G9-s3/G10 co-development).
Mainnet remains gated behind G8 review.
