# Mainnet gating — how nimiq.nimmesh stays on testnet until Andjroo says otherwise

> **One sentence:** every build, test, tool, and autonomous loop in this repo targets the
> Nimiq **testnet** (`networkId = 5`); flipping any of it to **mainnet** (`networkId = 24`)
> is a deliberate, human, Andjroo-only act — there is no code path that does it automatically,
> and the autonomous loop never proposes one.

> **2026-07-02 — Andjroo exercised the gate.** For the real-funds phone test he instructed
> "we need to be on mainnet": the **iOS app's network toggle now defaults to mainnet**
> (`NimiqRpc.isMainnet`, v0.34.0). Everything else in this contract stands: the app never
> auto-sends (a send is a user tap), the Send sheet warns on mainnet, testnet is one tap
> away, the faucet is testnet-only, and the Rust core / tests / tools / loop remain
> testnet-pinned (`default_network()` is still testnet).

This is the G13 safety contract. It documents (1) the invariants that keep us on testnet, (2)
exactly what a future mainnet switch would require, and (3) the checklist that must be green
**before** that switch is even considered. It is paired with `RISKS.md` (the hazard list) and
the `docs/adr/` decisions.

## 1. Why this document exists

The whole point of nimmesh is to move **real value** with no internet. That makes the
money-path the highest-stakes surface in the codebase: a signing bug, a replayed tx, or a
premature "paid" can cost a user funds that cannot be clawed back. Testnet gives us a network
that behaves byte-identically to mainnet (same Albatross consensus, same tx format, proven by
the G3 fixtures + the live G8 broadcast in block 4428402) but where mistakes cost nothing.
So we do **all** development, every autonomous cycle, and the entire demo on testnet, and we
treat the mainnet switch as a separate, manual, reviewed event.

## 2. The invariants that keep us on testnet (enforced in code)

These are the guard rails already in the codebase. They are load-bearing — do not weaken any
of them without Andjroo's explicit sign-off.

| # | Invariant | Where it lives |
|---|-----------|----------------|
| 1 | **Default network is testnet.** `default_network()` returns `NetworkId::Testnet`; a unit test asserts it can never silently become mainnet. | `lib.rs` (`default_network`, `default_network_is_testnet`) |
| 2 | **The RPC client refuses any non-testnet host.** Every gateway-RPC constructor is testnet-guarded (`rpc::guard_testnet`); there is no constructor that points at a mainnet endpoint. | `rpc.rs`, `gateway.rs` |
| 3 | **Broadcast is feature- + intent-gated.** The only code that calls `sendRawTransaction` is behind the `gateway-rpc` Cargo feature and a `NetworkId::Testnet` intent; a plain `cargo test` never compiles it. | `rpc.rs`, `examples/live_testnet_broadcast.rs` (`required-features = ["gateway-rpc"]`) |
| 4 | **The seed never crosses FFI, the mesh, or a log.** Only a public key + a signature leave the enclave seam; the mesh carries only broadcast-safe signed bytes. | `nimiq/signer.rs` (`EnclaveKey`), `engine.rs` (opaque `txWire`) |
| 5 | **Unconfirmed until inclusion.** A payment is `Settled` only on a gateway `Accepted` receipt — never on relay/optimism. | `settlement.rs` (`PaymentStatus`), `engine.rs` (`handle_receipt`) |
| 6 | **Money-path PRs never auto-merge.** Anything that signs, handles keys/seed, or broadcasts is PR-only behind Andjroo (`money-path` + `needs:owner` labels). | `LOOP.md` operating model |

`network_is_loop_safe(NetworkId)` returns `true` only for testnet — a UI / automation can call
it to refuse to arm any auto-broadcast path against mainnet and surface the gate instead.

## 3. What a mainnet switch would actually require (and who does it)

A real mainnet enablement is **not** a config toggle the loop can flip. It is a deliberate
change, authored + reviewed + merged + operated by Andjroo, touching all of:

1. **Code** — introduce a mainnet `NetworkId::Mainnet` path through the intent + the RPC guard,
   gated behind an explicit, off-by-default opt-in (never the default). This is a `money-path`
   + `needs:owner` PR; it does **not** auto-merge.
2. **Decision** — Andjroo authorizes the first mainnet broadcast in writing (the
   `needs:owner` decisions issue), having reviewed the diff and the checklist below.
3. **Keys** — the key origin is settled (enclave-stored seed vs Nimiq Pay / Hub delegate) and
   the chosen path has been verified on a real device (this is the C1 + Phase-D work).
4. **Ops** — a funded mainnet gateway endpoint + the operational runbook for it.

The loop's job stops at "build the testnet-proven thing and hand Andjroo a reviewed PR." It
never authors the mainnet path, never funds a mainnet account, and never broadcasts real value.

## 4. Pre-mainnet checklist — all must be green first

Do **not** consider mainnet until every box is checked. Most of these are still open.

- [ ] **C1 money-path shipped + reviewed** — keygen/import + Send→sign→queue, seed behind the
      `EnclaveKey` enclave seam, byte-exact vs `@nimiq/core` (the G3 fixtures already prove the
      serializer; C1 wires the real send).
- [ ] **G12 hardening live in production** — verify-before-relay, per-peer rate limits,
      stop-after-ACK, NACK on reject. ✅ *built + merged (v0.18.0); enabled by default in prod.*
- [ ] **On-device proof** — the native BLE shim (G5) runs a real iOS↔iOS / iOS↔Android mesh and
      a tx signed offline on one phone is broadcast by another, on **testnet**, end to end.
- [ ] **Key origin decided + device-verified** — enclave vs delegate, with a verified
      "sign-but-DON'T-broadcast" path on a real device.
- [ ] **Replay / double-spend discipline reviewed** — validity-window GC (G9) + dedup + the
      gateway mempool dedup behave correctly under partition + rejoin + duplicate floods.
- [ ] **Fee + dust policy** — the basic transfer is fee-0 today; confirm mainnet fee behaviour.
- [ ] **"Paid" honesty audited** — no surface shows `Settled`/received before an `Accepted`
      receipt; expired/failed surface as such (G17 closes this for both parties).
- [ ] **Independent review of the money-path diff** — not just the loop's own verification.
- [ ] **Andjroo's explicit written authorization** for the first mainnet broadcast.

## 5. Irreversible / gated actions — never taken by the loop

- Broadcasting a transaction on **mainnet** / pointing at a mainnet RPC / moving real funds.
- Tapping a real (non-faucet) funding source.
- App Store / Play **store distribution** or TestFlight-beyond-internal release.
- Publishing keys, seeds, or any secret to a repo, log, or external service.

All of these wait for Andjroo. The autonomous loop's standing guardrails (`LOOP.md` "the never
list") encode the same boundary.

## 6. Until then: the testnet demo

The full offline-origination → online-broadcast path is already proven on the real testnet by
`examples/live_testnet_broadcast.rs` (the G8 tool; settled in block 4428402). The whole mesh
pay loop — `submit → flood → TTL relay → dedup → gateway → receipt → settled` — also runs
headless with **no network** (mock gateway) via `examples/mesh_demo.rs`:

```text
cargo run -p nimmesh-core --example mesh_demo            # headless mock loop, no network
cargo run -p nimmesh-core --example live_testnet_broadcast --features gateway-rpc   # real testnet
```
