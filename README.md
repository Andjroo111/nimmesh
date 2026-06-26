# nimiq.bitmesh

**Pay with NIM when there is no internet.** A native, true multi-hop **Bluetooth-LE
mesh** wallet for Nimiq: sign a transaction on your phone with zero connectivity and let
it hop device-to-device through other phones until one of them — *anyone* with a
connection — broadcasts it to the Nimiq network for you.

A Nimiq take on Jack Dorsey's [Bitchat](https://github.com/permissionlesstech/bitchat):
same BLE mesh + store-and-forward, but the payload is a **signed Albatross transaction**
instead of a chat message. The whole idea rests on one fact — a signed Nimiq tx is a
self-contained ~139-byte blob that needs the internet only at the **final hop**.

> **Status:** bootstrapping (testnet-first). Built by an autonomous loop — see
> [`docs/LOOP.md`](docs/LOOP.md). **No mainnet / real funds** until explicitly enabled.

## How it works

```
 Alice (airplane mode)          Bob (offline relay)         Carol (has data = gateway)
 ───────────────────────        ───────────────────         ──────────────────────────
 sign tx offline (139 B)  ──▶   dedup · TTL-1 · relay  ──▶   sendRawTransaction(rawHex)
 wrap nimiqTx 0x30, TTL=7       (never reads payload)        → Nimiq mempool → validators
 flood over BLE                                              emit nimiqTxReceipt 0x31 ──┐
        ▲                                                                               │
        └──────────────── "pending → settled" (store-and-forward back to Alice) ◀───────┘
```

Only Carol's one egress hop was ever online.

## Architecture (see [ADR-0001](docs/adr/0001-native-rust-core-uniffi-stack.md))

- **Shared Rust core** (UniFFI) — Nimiq signing (Ed25519), packet codec, TTL/hop relay,
  LRU dedup, GCS store-and-forward, Noise sessions, gateway broadcast. No WASM, no
  consensus client. Headless-tested in CI.
- **iOS** — Swift + CoreBluetooth (central + peripheral concurrently).
- **Android** — Kotlin + `android.bluetooth.le` (scanner/GATT + advertiser/GATT-server).
- **Protocol** — ported from Bitchat (The Unlicense / public domain). See
  [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

## Docs

| Doc | What |
| --- | ---- |
| [docs/GOAL.md](docs/GOAL.md) | North star, the magic moment, the demo loop, core values |
| [docs/LOOP.md](docs/LOOP.md) | The autonomous build contract — goals G1–G13, gating, guardrails |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | The bitmesh wire format |
| [docs/RISKS.md](docs/RISKS.md) | Offline-payment hazard analysis + mitigations |
| [docs/adr/](docs/adr/) | Architecture decisions |

## Safety

Non-custodial by construction (the seed never leaves the secure enclave; only public,
broadcast-safe bytes ride the mesh). **Unconfirmed-until-inclusion** — never shows
"paid" before on-chain confirmation. **Testnet-by-default**; all signing, key handling,
and broadcasting is gated behind the owner; mainnet is a manual switch.
