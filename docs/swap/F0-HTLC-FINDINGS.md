# F0 spike — Albatross HTLC byte layout (the F1 spec)

> Empirically nailed against **`@nimiq/core` 2.7.0** (probes: `scripts/fixtures/probe-htlc.mjs`,
> `probe-htlc2.mjs`). This is the byte-exact target F1 (`nimiq/htlc.rs`) must reproduce.

## ✅ LIVE-PROVEN ON TESTNET (the whole HTLC lifecycle)

Validated end to end against the real Nimiq Albatross **testnet** via
`cargo run --example live_testnet_htlc_swap --features gateway-rpc`:

| Operation | Proof |
| --- | --- |
| HTLC **funding** (creation) | confirmed in block **4515174** (contract type `htlc`, 200000 luna) |
| HTLC **claim** (RegularTransfer, reveal preimage) | confirmed in block **4515177** — contract drained to 0 |
| HTLC **refund** (TimeoutResolve) | a real contract drained back to the funder past its timeout |

**Two corrections this surfaced (both now fixed in code + below):**

1. **The HTLC `timeout` is a Unix-MILLISECOND TIMESTAMP, not a block height.** The validator
   compares it to `block_state.time` (`core-rs-albatross` htlc_contract.rs). A height-valued
   timeout (`head + N`) is ~`1.7e12` smaller than the block time → the contract reads as
   **expired-at-birth**, so the *refund* path works but the *claim* path is always rejected. This
   is exactly the symptom we hit. (The early "Unix-ms" hint was right; my "block height" reading
   from `timeoutBytes=8` was about the *width*, not the *semantics*.)
2. **`@nimiq/core` 2.7.0 CAN validate a redeem proof after all** — its `.verify()` accepts our
   full signed redeem wire. The earlier "JS can't verify redeems" was *our wrong PreImage tag*:
   `PreImage::PreImage32` serializes as the **length byte `0x20` (= 32)**, not an enum index.

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
timeout(8)        u64 BIG-ENDIAN **Unix-MS timestamp** ← compared to block time, NOT height
```

Confirmed: `timeoutBytes=4` throws, `timeoutBytes=8` builds → the field is a **u64** (8 bytes).
**Its SEMANTICS are a Unix-millisecond timestamp** (proven live — see the banner): the validator
checks `timeout < block_state.time`. Set it well in the future (e.g. now + 1 h) or the claim path
is dead on arrival.

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

## Feasibility test: funding accepted; redeem proof needed the exact PreImage tag

A feasibility test (`scripts/fixtures/feasibility-test.mjs`) proved the **funding/creation tx is
ACCEPTED** by `@nimiq/core`'s own `verify(networkId)` (248 B, real signature). The redeem proof
initially failed to parse — **because the `PreImage` tag was wrong, not because JS can't verify
redeems.** Once built correctly (`PreImage32 = 0x20`, AnyHash sha256 = `03`, variant `0`),
`Transaction.fromAny(redeemWire).verify(5)` **ACCEPTS** it (`proof.type = "regular-transfer"`),
and the **live testnet confirmed it** (block 4515177). So both `@nimiq/core` 2.7.0 **and** the live
network validate the proof — the funding/redeem are byte-exact end to end.

## HTLC resolve **proof** (byte-exact; LIVE-confirmed)

Authoritative structure (from `core-rs-albatross`
`primitives/transaction/src/account/htlc_contract.rs`, `OutgoingHTLCTransactionProof`,
**PoS variant ids start at 0**; `AnyHash`/`PreImage` are tagged enums = `discriminant ‖ bytes`,
where the sha256 discriminant is **3** — same as in the creation data, i.e. `03 ‖ 32`):

- **RegularTransfer** (claim with preimage, variant `0`):
  `0x00 ‖ hashDepth(1)=01 ‖ hashRoot:AnyHash(0x03‖32) ‖ preImage:PreImage(0x20‖32) ‖ sigProof(98)`
  = **166 B**, LEB128 length-prefixed (`a6 01`). The claimant signs; revealing it on-chain reveals S.
  **The `PreImage32` discriminant is `0x20` (= the length 32), NOT an enum index** — the bit that
  blocked it for hours (`core-rs-albatross` `impl Serialize for PreImage`).
- **TimeoutResolve** (refund after timeout, variant `2`): `0x02 ‖ sigProof(98)` = 99 B — creator signs.
- **EarlyResolve** (both sign, variant `1`): `0x01 ‖ sigProof(recipient) ‖ sigProof(creator)`.

Both `RegularTransfer` (claim) and `TimeoutResolve` (refund) are implemented in `nimiq::htlc`
(`regular_transfer_proof` / `timeout_resolve_proof`) and **confirmed on the live testnet**.

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
