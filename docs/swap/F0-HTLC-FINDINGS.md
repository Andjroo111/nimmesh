# F0 spike — Albatross HTLC byte layout (the F1 spec)

> Empirically nailed against **`@nimiq/core` 2.7.0** (probes: `scripts/fixtures/probe-htlc.mjs`,
> `probe-htlc2.mjs`). This is the byte-exact target F1 (`nimiq/htlc.rs`) must reproduce.

## The pivotal finding — `@nimiq/core` is generator AND verifier, asymmetrically

- It **can fully build the HTLC *creation* tx** (the `Transaction` constructor with
  `recipient_type=2`, `flags=0b1`) → we get byte-exact `rawHex` / `serializeContent` / `hash`
  / contract address as committed fixtures.
- It **can build an unsigned *redeem* tx** (`sender_type=2`) → byte-exact `serializeContent`
  (the signing payload) as a fixture.
- It **cannot sign an HTLC redemption** — `redeem.sign(...)` throws *"HTLC redemption
  transactions are not supported."* So F1 builds the resolve **proof** + assembles the full
  redeem wire **in Rust**, and the gate is feeding that wire back through
  `Transaction.fromAny(rawHex).verify(networkId)` and asserting `@nimiq/core` **accepts** it.

## HTLC creation **data** (recipient_data, 82 bytes)

```
sender(20)        the refunder (HTLC creator) address
recipient(20)     the claimant (can redeem with preimage) address
hashAlgorithm(1)  1 = Blake2b, 3 = SHA-256  ← use 3 (sha256) for BTC cross-chain
hashRoot(32)      H = hash^hashCount(preimage); for hashCount=1, H = SHA-256(S)
hashCount(1)      number of times the preimage is hashed (1 for a simple swap)
timeout(8)        u64 BIG-ENDIAN block height  ← height-based (the head-beacon is the clock)
```

Confirmed: `timeoutBytes=4` makes `@nimiq/core` throw; `timeoutBytes=8` builds → **timeout is
a u64 block height**. (The old "Unix-ms timeout" is Nimiq 1.0; Albatross is block-height.)

## HTLC creation **transaction** (Extended format)

`new Transaction(sender, 0, null, contractAddr, 2 /*HTLC*/, htlcData, value, fee, 0b1
/*creation*/, vsh, networkId)`, where **`contractAddr = tx.getContractCreationAddress()`**
(build once with a dummy recipient to compute it, then rebuild with it as recipient — the
validator requires `recipient == creation address`).

Example (alice→HTLC, value 1000 luna, vsh 100, testnet, sha256, timeout 12345):
`contractAddr = NQ37 BXEK 30R7 R60T 8G3D NAEQ 8JHE 9Y3S S6H7`, rawHex 150 B, content 149 B.

## HTLC **redeem / claim** tx — `serializeContent` (67 B)

Identical to a basic-transfer content **except `sender_type = 2` (HTLC)** and sender = the
contract address. Decoded:
`dataLen u16=0 ‖ sender(20)=contractAddr ‖ sender_type=0x02 ‖ recipient(20) ‖ recipient_type=0x00
‖ value u64 ‖ fee u64 ‖ vsh u32 ‖ network u8 ‖ flags u8=0 ‖ sender_data_len u8=0`.

→ F1 reuses the existing `serialize_content` with a parameterized `sender_type` byte.

## ⚠️ Correction (feasibility test): `@nimiq/core` JS canNOT verify a redeem proof

A feasibility test (`scripts/fixtures/feasibility-test.mjs`) proved:

- **HTLC funding/creation tx → ACCEPTED ✅** by `@nimiq/core`'s own `verify(networkId)` (248 B,
  real signature). The funding leg is concretely real.
- **HTLC redeem (claim-with-preimage) → `@nimiq/core` JS cannot help.** It refuses to `sign()`
  HTLC redemptions, `fromPlain` won't construct a proof without the `raw` bytes, and the WASM
  deserializer rejects hand-built proofs. This is a deliberate JS-binding limitation, **not** a
  protocol limitation (Albatross validators enforce HTLC redemption; the `PlainHtlc*Proof` types
  exist precisely because the chain verifies them; Nimiq ships NIM↔BTC atomic swaps in its wallet).

**→ F1 gate correction:** the redeem **proof** is verified against the authoritative
**core-rs-albatross** Rust crate (or a **live Nimiq testnet broadcast** of a real HTLC redeem —
the repo already has the G8 `live_testnet_broadcast` tool), **not** `@nimiq/core` JS. The funding
tx + all `serializeContent` payloads stay byte-exact-gated against `@nimiq/core` (it handles those).

## HTLC resolve **proof** (built in Rust, gated by core-rs-albatross / live testnet)

From the `@nimiq/core` `PlainHtlc*Proof` field sets; exact variant discriminants + order to be
confirmed in F1 via `.verify()` (the Albatross `htlc_contract.rs` proof encoding):

- **RegularTransfer** (claim with preimage):
  `proofType(1) ‖ hashAlgorithm(1) ‖ hashDepth(1) ‖ hashRoot(32) ‖ preImage(32) ‖ signatureProof`
  — the claimant signs; `preImage = S`; revealing this on-chain reveals S.
- **TimeoutResolve** (refund after timeout): `proofType(1) ‖ signatureProof` — the creator signs.
- **EarlyResolve** (both sign): `proofType(1) ‖ signatureProof(recipient) ‖ signatureProof(creator)`.

`signatureProof` = the existing 98-byte single-sig blob already built by
`tx::signature_proof_single_sig` (`type(1)=0 ‖ pubkey(32) ‖ merklePathLen(1)=0 ‖ sig(64)`).

## F1 fixture plan (`scripts/fixtures/gen-fixtures.mjs`, new HTLC cases)

1. **Creation fixtures** — N cases: emit `htlcDataHex`, `contractAddrUser/Raw`, `rawHex`,
   `serializeContentHex`, `txHash`. Rust asserts byte-exact.
2. **Redeem-content fixtures** — emit the unsigned redeem `serializeContentHex`. Rust asserts
   byte-exact.
3. **Redeem-acceptance** — Rust builds the full signed redeem wire (RegularTransfer +
   TimeoutResolve); a `verify-htlc.mjs` companion loads each via `Transaction.fromAny(rawHex)`
   + `.verify(5)` and asserts `@nimiq/core` accepts it (the proof-correctness gate, since
   `@nimiq/core` won't emit a reference redeem wire).

## Safety note carried into F3

`timeout` being a u64 **block height** means the swap timelock ladder (`T_A > T_B + Δ_safe`)
is expressed directly in head-beacon heights — no wall clock, consistent with G9. Δ_safe must
dominate the worst-case mesh-propagation window (store-and-forward up to ~15 min) → set
conservatively in the hundreds-to-thousands of blocks (1 block ≈ 1 s).
