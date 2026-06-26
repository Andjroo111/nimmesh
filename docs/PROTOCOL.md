# nimiq.bitmesh — Wire Protocol

> Adapted from **Bitchat** (`permissionlesstech/bitchat`), whose `LICENSE` is **The
> Unlicense** (public-domain dedication) — free to copy, modify, fork, and embed with
> zero attribution or copyleft obligation. Build against the shipping `BitFoundation`
> code, **not** the stale WHITEPAPER. The Rust core is the canonical implementation.

## Packet format (big-endian, header v1 = 14 bytes)

```
version(1)=1 | type(1) MessageType | ttl(1) default 7 | timestamp(8, ms)
            | flags(1) | payloadLength(2)
then, in order:
senderID(8, always) | recipientID(8, if hasRecipient) | payload(payloadLength)
            | signature(64, if hasSignature, Ed25519)
```

**Flag bits:** `0x01` hasRecipient · `0x02` hasSignature · `0x04` isCompressed (zlib)
· `0x08` hasRoute (v2) · `0x10` isRSR (gossip-sync reply).
Whole packet is PKCS#7-style **padded up to the smallest of [256, 512, 1024, 2048]**
for traffic-analysis resistance.

## New message types (from the free 0x23–0xFF range; reserved in our fork)

| type | name              | direction        | purpose                                              |
| ---- | ----------------- | ---------------- | ---------------------------------------------------- |
| 0x30 | `nimiqTx`         | flood            | a signed Nimiq tx (public, broadcast-safe)           |
| 0x31 | `nimiqTxReceipt`  | gateway → mesh   | accepted / expired / failed ack, keyed by txId       |
| 0x32 | `nimiqHeadBeacon` | gateway → mesh   | `{height u32, blockHash, networkId}` to anchor validity |

Inner `NoisePayloadType` for targeted/encrypted (`noiseEncrypted = 0x11`):
`nimiqTx = 0x04`, `nimiqTxReceipt = 0x05`.

## Nimiq TLV envelope (1B type · 1B len · value — Bitchat style)

| T    | field        | req | notes                                                             |
| ---- | ------------ | :-: | ----------------------------------------------------------------- |
| 0x01 | `txWire`     | yes | canonical signed Nimiq bytes (~139 B basic, ~205 B w/ memo) — broadcast verbatim |
| 0x02 | `networkId`  | yes | 1 B; gateway drops on mismatch; **default testnet = 5**           |
| 0x03 | `validUntil` | opt | u32 BE block height; gateway drops if `head > validUntil`         |
| 0x04 | `txId`       | opt | 32 B hash; dedup key                                              |
| 0x05 | `encMemo`    | opt | Noise/NIP-44 encrypted blob                                       |
| 0x06 | `wantReceipt`| opt | 1 B flag                                                          |

## How a tx rides the mesh (default = public flood)

The Nimiq tx is **self-authenticating** (embeds its own Ed25519 proof the chain
verifies), so packet-level signature and Noise are **not** needed: `type=0x30`,
`recipientID = FF×8` (broadcast), `ttl=7`, `hasSignature=0` (saves 64 B),
`hasRecipient=1`. **~205 B padded to the 256 block is well under the 469-B fragment
chunk → zero fragmentation, one BLE packet, one hop of airtime.**

Option B (private/targeted): wrap as `noiseEncrypted=0x11` + inner `0x04` to a chosen
gateway when a memo must stay private. **Default to flood** (maximizes gateway reach).

## TTL / hop cap & relay

Default and cap **TTL = 7**. On relay: `ttlLimit = min(ttl, 7)`, `newTTL = ttlLimit - 1`,
drop if `ttlCap <= 1`. **Degree-adaptive probabilistic relay** for broadcasts
(high-degree threshold 6) with **relay jitter 10–220 ms** to avoid collisions; the
source link is excluded.

## Dedup

Generic O(1) **LRU** keyed by `(senderHex, timestamp, type)`, age bound **300 s** —
*not* a bloom filter. Plus Nimiq mempool's own tx-hash idempotency.

## Fragmentation (defined but unused for our payload)

`fragment = 0x20`; header 8 B fragmentID + 2 B index + 2 B total + 1 B originalType;
`bleDefaultFragmentSize = 469`; reassembly capped at 128 in-flight, 30 s lifetime;
reassembled TTL zeroed. Our ~205-B tx never triggers it.

## Store-and-forward = GCS gossip-sync (the key offline-origination piece)

Each node keeps recent packets in typed caches (messages **1000 / 15 s / 900 s**) and
advertises what it **has** as a compact **Golomb-Coded-Set** filter (fpr 0.01, ≤ 400 B)
inside a `requestSync` (`type 0x21`, ttl 0, local-only); peers unicast back anything
**not** in the filter, flagged `isRSR`. An offline gateway or originator rejoining
within **15 min** (`maxMessageAgeSeconds = 900`) auto-catches-up — comfortably inside
Nimiq's ~2 h validity window.

## Encryption (optional memo / chat)

`Noise_XX_25519_ChaChaPoly_SHA256` — mutual-auth, identity-hiding handshake; two
ChaChaPoly cipher states with a 1024-msg sliding-window replay guard; identity =
long-term Curve25519 static key, fingerprint = its SHA-256. A Nostr **NIP-17 gift-wrap**
bridge (kind 14 → 13 → 1059) is an alternative internet escape hatch for targeted gateways.

## Gateway broadcast (the one online hop)

A **gateway** = a bitmesh node that also has internet + a Nimiq RPC client. On
`type == 0x30` it:

1. **relays anyway** (TTL decrement) so other gateways can also try — mempool dedups on
   tx hash, so double-submit is harmless;
2. **dedups** on `txId` (LRU) so it submits once;
3. **guards** `networkId`;
4. **checks** `head` vs `validUntil` / validity window; drops + optional `0x31` "expired"
   receipt if past;
5. calls **`sendRawTransaction(rawHex)`** against a public Albatross node (e.g.
   `rpc.testnet.nimiqwatch.com`) → mempool → validators (plain JSON-RPC; **never**
   instantiate the `@nimiq/core` consensus client);
6. optionally emits **`nimiqTxReceipt = 0x31`** keyed by `txId` back into the mesh, which
   GCS store-and-forward delivers to the (possibly offline) originator within 15 min —
   closing the loop without the sender ever holding an internet link.

**Service UUID is separated mainnet vs testnet** (as Bitchat separates networks); the
characteristic follows the spec.

## Validity window — the relay budget

Albatross `TRANSACTION_VALIDITY_WINDOW = 120 batches × 60 blocks = 7,200 blocks` at
1 s blocks ≈ **~2 hours**. A tx is valid only for `[validityStartHeight,
validityStartHeight + 7200)`. **This is the entire mesh relay budget for a signed tx.**
Set `validityStartHeight = latest known head` at sign time (never pre-date); beacon
fresh heads (`0x32`); set packet TTL/`validUntilMs = min(remaining window, hop budget)`;
GC expired packets so the mesh never carries dead txs.
