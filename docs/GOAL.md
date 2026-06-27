# nimiq.nimmesh — Goal

> **Pay with NIM when there is no internet.** A native, true multi-hop Bluetooth-LE
> mesh wallet: sign a Nimiq transaction on your phone with zero connectivity, and
> let it physically hop device-to-device through strangers' phones until it reaches
> *anyone* with a connection — who broadcasts it to the Nimiq network for you.

## North star

A signed Albatross transaction is a **self-contained ~139-byte blob** that carries
its own Ed25519 proof. The chain does not care how those bytes reached a node — only
that they are valid and arrive within their validity window. **That is the whole
idea:** the internet is needed only at the *final hop*. Everything before it can be a
mesh of phones passing bytes over Bluetooth.

This is, structurally, the fleet's `sendhome` two-layer provider — **sign offline
(`@nimiq/core` / pure Ed25519)** then **broadcast online (`sendRawTransaction` JSON-RPC)** —
with the wire between those two steps run across *physical devices over a
Bitchat-derived BLE mesh* instead of across two function calls.

## The magic moment

A person in **airplane mode** taps **Send**. The payment ripples silently across a
room — a protest, a disaster zone, a dead-coverage village — through other people's
phones acting as blind relays. Minutes later their screen flips from **"pending"** to
**"settled."** They paid on a public blockchain without ever touching the internet.

## The demo loop (the one thing a judge/tester completes)

1. **Alice (offline, Bluetooth only)** builds + signs a Nimiq **testnet** transfer
   on-device, anchoring `validityStartHeight` to the most recent head she knows
   (cached, or beaconed over the mesh). Output: a ~139-byte raw-hex blob.
2. Her app wraps it in a `nimiqTx` (0x30) packet, pads to 256 B, floods it at TTL=7.
3. **Bob (also offline)** receives it, dedups, decrements TTL, **relays it onward** —
   he is just a relay and never inspects the payload.
4. **Carol (has cellular data)** is a **gateway**: she validates (networkId + validity
   window), calls `sendRawTransaction(rawHex)` against a public Albatross testnet RPC,
   the tx enters the mempool, and she emits a `nimiqTxReceipt` (0x31) back into the mesh.
5. The receipt store-and-forwards back to **Alice**; her UI flips **pending → settled**.

Only Carol's single egress hop was ever online. In CI the same loop runs end-to-end
against a `MockMeshTransport` (no radio) + mock RPC, so every layer except the
physical radio is testable headless.

## Core values (every decision scored against these)

1. **Non-custodial by construction** — the seed never leaves the secure enclave; only
   public, broadcast-safe signed bytes ride the mesh. The signer takes content bytes,
   returns a proof, never a key.
2. **Censorship-resistant / internet-optional** — a payment originates and propagates
   with no internet; it needs connectivity only at the final gateway hop. A relay can
   forward a signed tx but cannot alter or block it.
3. **Trustless relay** — every hop forwards opaque, self-authenticating bytes. The tx
   carries its own Ed25519 proof; relays and gateways are stateless and untrusted.
4. **Offline-first accessibility** — built for dead-coverage, disaster, protest, and
   unbanked contexts on commodity phones, no infrastructure.
5. **Unconfirmed-until-inclusion honesty** — never show "paid" until a gateway confirms
   on-chain inclusion. Deferred failures (expiry, insufficient funds) surface as
   explicit pending/failed states with signed NACKs.
6. **Privacy-aware** — treat mesh input as hostile; optional Noise-encrypted memos;
   never put PII in cleartext on the air or on-chain.
7. **Testnet-by-default, money-path-gated** — defaults to Nimiq testnet; all
   broadcasting and key/seed handling is PR-only behind Andjroo; no mainnet or
   real-fund action without explicit approval.

## The Nimiq edge (must stay load-bearing, never bolted on)

- A signed Albatross tx is **self-contained and self-authenticating** → it can travel
  over any transport and only needs the internet once.
- **Account model + nonce + validity window** make offline double-spend impossible to
  finalize on-chain (the validator rejects the second tx). The hazard is a *deferred*
  failure shown to a receiver — handled by unconfirmed-until-inclusion UX.
- **1-second blocks**, so "settled" can come back over the mesh fast.
- Single-signer Nimiq signing is **plain RFC-8032 Ed25519** over `serializeContent()`,
  so it runs in a portable Rust core with no WASM and no consensus client.

## What this is NOT

- Not custodial, not a new chain, not a payment processor.
- Not a web app — a true BLE mesh needs native (web Bluetooth can't advertise/peer).
- Not mainnet yet — testnet-first; real funds are explicitly gated behind Andjroo.

## Success criteria

- **MVP (testnet):** the demo loop completes on real devices over real BLE — airplane-mode
  origin → multi-hop relay → gateway broadcast → receipt → settled UX.
- **CI:** the full sign → relay → gateway → receipt loop passes headless against the
  mock transport on every PR.
- **Honesty:** no "paid" without inclusion; expiry and insufficient-funds paths are
  surfaced, not swallowed.

See [LOOP.md](./LOOP.md) for the build contract, [PROTOCOL.md](./PROTOCOL.md) for the
wire format, and [RISKS.md](./RISKS.md) for the offline-payment hazard analysis.
