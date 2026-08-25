# Trustless offline balance verification

How a phone with no internet checks "this address held X NIM as of block H" without
trusting the gateway that answered or any relay in between. Issue #5, the third part
of G15 (balance over the mesh).

## The critique this answers

A technically sharp community question, paraphrased: *why should a vendor trust an
arbitrary sub-protocol instead of chain primitives?* The answer is to agree with it.
The mesh protocol is only the **envelope**. The payload that makes a balance claim
trustworthy must be something the chain itself defines and any client can verify:
an Albatross **accounts proof**.

## What ships today, honestly

A `nimiqBalanceQuery` (`0x33`) floods an address; any internet-bearing gateway answers
with a `nimiqBalanceResponse` (`0x34`): the balance *it claims* to have read at a head
height. The app labels it **unverified / last-known** (`crates/nimmesh-core/src/balance.rs`),
because that is exactly what it is — a relay is untrusted, and so is a gateway.

## The chain primitives

Everything needed already exists in [`core-rs-albatross`]; nothing here invents new
cryptography.

- **`TrieProof`** ([`primitives/src/trie/trie_proof.rs`]) — a Merkle proof of inclusion
  in the accounts trie (a Merkle Radix Trie). Nodes are carried in post-order; branch
  nodes embed their children's hashes, so no adjacent-sibling padding is needed. Its
  `verify(root_hash)` checks the sub-trie hashes up to the root.
- **The block header** — every Albatross header carries `state_root`, the hash of the
  accounts-trie root at that block. The **block hash** is the Blake2b-256 of the
  serialized header.
- **The head beacon** (`0x32`, G9) — a gateway already floods `{height, blockHash,
  networkId}` over the mesh, and every node caches the freshest one (`HeadCache`).

Chain these and a phone that has heard a beacon can verify a balance with three local
checks and zero trust in the messenger:

```
1. Blake2b-256(header)  == beacon.blockHash      → this header IS block H
2. header.state_root    =: R                     → the accounts root at H
3. TrieProof.verify(R) ∧ leaf.key == address     → the account's balance at H
```

The relay never needs to be honest. A forged header misses the beacon hash; a forged
proof misses the state root; a replayed old proof fails the height check. The only
trusted input is the beacon — and the plan for hardening that anchor further (checking
the header's inclusion against an election head) is noted under "Later" below.

## Wire design: `nimiqBalanceProof` (`0x37`)

```text
address(20) | headHeight(4) | networkId(1)
           | headerLen(2)  | header(…)    — serialized Albatross block header
           | proofLen(2)   | proof(…)     — serialized Albatross TrieProof
```

- `BalanceResponse` already carries `head_height` precisely so a proof could bind to
  it; `0x37` is the payload that does.
- The payload exceeds one 256-byte BLE frame, so it rides the proven G6 fragment path,
  same as a `0x36` history response. The round-trip is asserted in
  `balance_proof.rs::a_realistic_proof_rides_the_fragment_path_intact`.
- Decoding is length-exact, cap-checked (`header ≤ 1 KB`, `proof ≤ 8 KB`) and
  panic-free — mesh input is hostile.
- `0x34` keeps flowing unchanged. `0x37` is additive: a proof-capable gateway answers a
  query with both, and an old app that only understands `0x34` behaves exactly as today.
  Registering the type now means every shipped relay carries proofs blindly the day
  gateways start emitting them (an unknown type byte is dropped, not relayed).

## Verification on the phone, staged

| step | what | status |
| ---- | ---- | ------ |
| 1 | wire envelope codec, exact + panic-free | **landed** (`balance_proof.rs`) |
| 2 | `Blake2b-256(header) == beacon.blockHash` binding, all six verdicts | **landed** (`bind_to_beacon`) |
| 3 | `state_root` extraction from the serialized header | staged |
| 4 | the `TrieProof` post-order walk against `state_root` | staged |

Steps 3–4 are deliberately not faked. Both parse `nimiq-serde` (postcard-style)
serializations, and a hand-rolled parser that has never been checked against bytes from
a real node would be **self-consistent rather than chain-faithful** — it could pass its
own tests and still accept a proof no Albatross node would. They land together with
fixture vectors captured from a live testnet node, the same way the G3 signer was
asserted byte-exact against `@nimiq/core` fixtures. Until then, nothing upgrades a
cached balance to verified: `BindVerdict::Bound` is a necessary gate, not yet a
sufficient one, and the app keeps labeling every balance untrusted.

The phone side stays dependency-light throughout: step 2 needs only `blake2` (already a
core dependency), and steps 3–4 need a small parser, not a consensus client.

## The two gaps on the gateway side

### 1. ~~The beacon's block hash is still zeroed~~ — resolved

The G8 seam now reads `getLatestBlock` (`GatewayRpc::latest_block`), the gateway beacon
carries the real head hash, and the node's `HeadCache` retains the whole beacon so
`bind_to_beacon` has its anchor (`latest_beacon()`). An RPC client without the
capability still serves a zeroed hash, which reads as honestly unbindable
(`BeaconUnhashed`) — never as a wrong hash. The mock-to-bind chain is asserted end to
end in `balance_proof.rs::a_gateway_sourced_beacon_binds_a_proof_end_to_end`.

### 2. JSON-RPC has no accounts-proof method

Verified against the [`rpc-interface`] source: the Albatross JSON-RPC surface has
`getAccountByAddress` but **no method that returns a `TrieProof`**. The proof machinery
lives in the light-client network protocol instead: `RequestTrieProof { keys } →
ResponseTrieProof { proof, block_hash }` (request type 215, [`consensus/src/messages`]),
served by full nodes over libp2p.

Options, in preference order:

1. **Upstream a `getTrieProof` RPC method** to `core-rs-albatross`. The node already
   has the machinery (the network handler builds proofs from the same blockchain
   state); the dispatcher addition is small. Best for the ecosystem — any project gets
   trustless reads over plain HTTP. This is worth an upstream issue before any
   workaround is built.
2. **A gateway-local full node.** A gateway operator who runs their own node can build
   proofs in-process (the blockchain crate exposes the proof constructor). Fits the
   self-hosted gateway story; useless for phone gateways on public RPC.
3. **Embed the light-client network stack in the gateway.** Heaviest option (libp2p),
   listed for completeness only.

Until one of these lands, `0x37` has a verifier waiting and no emitter — which is the
honest order. The wire format and phone-side checks do not depend on which option wins.

## What this buys, and what it cannot

With a bound, verified proof plus a valid tx signature, an offline vendor knows:

- the payer's address **really held X NIM as of block H** — not "a gateway said so";
- H is the freshest head any nearby gateway has beaconed, and the proof is exactly as
  stale as the beacon they can already see.

What remains open is a **double spend inside the freshness window**: the payer could
have emptied the account after block H via another path. No mesh can close that window
while offline — closing it *is* consensus, and the mesh does not replace consensus; it
delivers transactions to it. The proof shrinks offline acceptance risk to that one
well-defined window; the existing mitigations (small amounts, the `0x31` receipt
round-trip whenever any gateway is reachable) are unchanged. See `docs/RISKS.md`.

Two related honesty notes, so no doc drifts into overclaiming:

- A verified balance is **not** offline finality, and must never be labeled as such.
- The beacon itself is today taken from the first gateway heard (freshest height wins).
  A dishonest beacon is limited by monotonicity and the network-id pin, but "verify the
  beacon header against an election head" is the right later hardening, noted below.

## Later

- **Beacon hardening:** carry the serialized head *header* in the beacon (not just the
  hash) and check its `block_number`/`network` fields; longer-term, verify inclusion
  against a macro/election block via `RequestBlocksProof`, which would anchor the whole
  chain of trust in the validator set rather than in "first gateway heard".
- **Proof freshness UX:** the app should show "verified as of block H, N minutes ago"
  — verified and stale are orthogonal, and the UI must show both.
- **History proofs:** the same envelope pattern extends to `0x36` history responses
  (Albatross `history_root` + history-tree proofs) if history ever needs to be
  trustless; out of scope here.

[`core-rs-albatross`]: https://github.com/nimiq/core-rs-albatross
[`primitives/src/trie/trie_proof.rs`]: https://github.com/nimiq/core-rs-albatross/blob/albatross/primitives/src/trie/trie_proof.rs
[`rpc-interface`]: https://github.com/nimiq/core-rs-albatross/blob/albatross/rpc-interface/src/blockchain.rs
[`consensus/src/messages`]: https://github.com/nimiq/core-rs-albatross/blob/albatross/consensus/src/messages/mod.rs
