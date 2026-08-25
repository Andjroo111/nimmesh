# A `getTrieProof` RPC for a NIMmesh gateway

What a NIMmesh gateway needs from a Nimiq node to answer a balance query *trustlessly*,
and how to get it without waiting on anyone.

## The problem

`docs/BALANCE-PROOF.md` gets as far as: bind the proof to the head beacon, parse the
header, verify the trie proof. The last step needs proof bytes, and no node will hand
them over — verified against the upstream sources:

- The Albatross **JSON-RPC has no accounts-proof method**. The primitive exists only in
  the light-client network protocol: `RequestTrieProof { keys } → ResponseTrieProof
  { proof, block_hash }` (request type 215, `consensus/src/messages/mod.rs`).
- The JSON `Block` type **omits `diffRoot`** (`rpc-interface/src/types.rs`), so a header
  can never be reconstructed byte-exactly from RPC JSON — not by a gateway, and not by a
  test trying to capture fixtures.

So even a node operator running their own node cannot serve NIMmesh proofs over HTTP.

## The fix, which is smaller than it sounds

A **full** node already holds the entire accounts trie, and the blockchain crate already
exposes the constructor:

```rust
// blockchain/src/blockchain/accounts.rs
pub fn get_accounts_proof(&self, keys: Vec<&KeyNibbles>) -> Result<TrieProof, IncompleteTrie>
```

`apply-gettrieproof.py` surfaces it through the node's own JSON-RPC, alongside the raw
header bytes the JSON type drops. No consensus change, no new capability — it reads
public state the node already has, and adds one read method.

```
getTrieProof(addresses: [Address]) -> {
    blockNumber, blockHash, blockType: "micro" | "macro",
    header: hex,   // canonical nimiq-serde bytes; Blake2b256(header) == blockHash
    proof:  hex,   // canonical nimiq-serde bytes of the TrieProof
}
```

Both blobs are **raw serializations, not JSON re-renderings**. That is the whole point: a
phone verifies `Blake2b256(header) == blockHash` against its head beacon, reads
`state_root` out of the header, and checks the proof against it — trusting neither the
gateway nor any relay. It is also the source of the real-node fixture vectors that step 4
of the verify path has been waiting on.

## Status

**The patch has not been compiled.** It was written against the sources on the pool
validator's checkout (`bb3aec298` = v2.0.0 + 23 CERT commits), and the build machine was
not reachable when it was written. Treat the first `cargo build` as part of applying it.

The applier itself is tested: it validates all 8 anchors before writing anything, so a
version drift refuses cleanly instead of half-patching, and re-running on a patched tree
refuses rather than duplicating.

## Applying it

```bash
python3 apply-gettrieproof.py --check /path/to/core-rs-albatross   # anchors present?
python3 apply-gettrieproof.py         /path/to/core-rs-albatross
cd /path/to/core-rs-albatross && cargo build --release -p nimiq-client
```

Then a smoke test against the built binary's RPC:

```bash
curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"getTrieProof","params":[["NQ.. some address .."]],"id":1}' \
  http://127.0.0.1:<rpc-port> | jq '.result.data | {blockNumber, blockType, headerLen: (.header|length/2)}'
```

A micro-block head should return a header of ~261-276 bytes, which is what
`crates/nimmesh-core/src/nimiq/header.rs` parses. Capture one full response and commit it
as the fixture vector; that is what turns the header codec from
differential-tested-against-postcard into proven-against-the-chain.

## Where to run it — not on the validator

Run the patched binary as a **second, non-validator node** on the same host, not as the
block producer:

- It carries no validator identity, so there is **no double-signing surface** — that risk
  requires one validator identity running twice, and a plain node is not a validator.
- The validator never restarts to deploy a NIMmesh change.
- Cost is small: the pool validator's own full-node database is ~529 MB with ~600 MB
  resident, on a box with 22 GB RAM and 185 GB free.

Give it its own user, its own data directory, and its own ports (p2p, RPC, metrics all
differ from the validator's `8443` / `8648` / `9100`), and **no `[validator]` section** in
its `client.toml`. Never copy the validator's `signing_key.dat` / `voting_key.dat` /
`fee_key.dat` — a plain node generates its own keys when none exist, and it needs no
validator keys at all.

## Why full and not history

A **full** node is sufficient here: it holds the complete current accounts trie, which is
exactly what a proof is built from. Per the upstream README, a **history** node needs a
minimum of 1 TB (2 TB with indexing) against a full node's 60 GB, and sync mode is part of
the database directory name (`{network}-{sync_mode}-consensus`) — so switching modes means
a resync from genesis, not a config toggle. History buys transaction history
(`getTransactionsByAddress`, which a full node refuses); it buys nothing for proofs.

## Upstreaming

The clean long-term answer is a `getTrieProof` method in core-rs-albatross itself, so any
gateway operator gets trustless reads over plain HTTP. This patch is deliberately shaped
like something upstreamable — same bounds as the network handler (non-empty, ≤ 255 keys),
same error style — but proposing it is a separate decision and is **not** a prerequisite
for NIMmesh. Our own gateway never needed permission.
