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

## 7. 2026-07-06 — Andjroo exercised the gate: the mainnet mesh DELIVERY role

Andjroo explicitly requested a real-funds mesh payment ("I wanna do a real wallet
transaction. I will be providing the NIM and sending it to another address that I
control."). Per section 3, that authorization is his to give, and this is the record.

What was opened (v0.53.0), and exactly how far it goes:

- `HttpGatewayRpc::new_mainnet(url)` + `RpcGateway::new_mainnet(rpc)` +
  `MeshNode::new_gateway_mainnet(...)` — a mainnet GATEWAY that broadcasts
  **already-signed** txs it hears over the mesh and enforces `networkId = 24`.
  It holds no keys and signs nothing. A testnet-looking URL is refused (the mirror
  of `guard_testnet`).
- `MeshNode::new_on_network(...)` — the phone app's node pinned to mainnet so it
  anchors to mainnet head beacons and stamps mainnet envelopes. Constructing a node
  signs nothing; only the wallet owner's explicit Send signs a tx.
- The Mac node broadcasts on mainnet ONLY when launched with the explicit
  `--mainnet` flag. Default stays testnet.

What did NOT change: every default constructor and the autonomous loop remain
testnet-only; `guard_testnet` still refuses mainnet hosts everywhere else; the agent
still never signs or initiates a mainnet transfer — the human sender does, on their
own device, exactly like the online Send path.

## 8. The mainnet SWAP gate — first real-funds cross-chain atomic swap

A swap is a bigger, more dangerous surface than the mesh payment (§7): **two chains, two
funding transactions, a secret reveal, timelocks, and a responder that funds a leg
automatically.** That last point is the crux — unlike a payment, where the human signs the
one and only transaction, a swap has a counterparty leg that a *node* funds on-chain. On
mainnet that is real value moving without a per-transaction human tap. So the swap gate is
stricter than the payment gate, and it is exercised in this order.

### 8.0 What is already true (testnet-proven, in code)

- The **real money-path is proven end to end on testnet + Amoy** (Act 2, `docs/swap/ACT2-RECEIPTS.md`;
  G10, `docs/swap/G10-RECEIPTS.md`): a real NIM HTLC on Albatross testnet ⇄ a real USDC HTLC on
  Polygon Amoy, secret revealed on-chain, both legs settled, nothing stranded.
- **Atomicity holds**: the S1 fund-on-message vector is closed (on-chain funding verification
  before either side funds — `swap_funding_verify.rs`), and the #189 secret-reuse vector is
  closed (the session tombstones every `swap_id` it initiates — `swap_session.rs`). The Δ_safe
  timelock ladder (ADR-0004) refuses to fund a swap whose timeouts can't be made safe.
- Every constructor is **testnet/Amoy-guarded** (`guard_testnet` / `guard_amoy`); the app node,
  the Mac responder, and the loop are all testnet by default.

### 8.1 The invariants a mainnet swap must NOT break

1. **The agent never autonomously moves mainnet funds.** Same floor as §3/§5. A mainnet swap
   leg is only ever funded by (a) Andjroo signing on his own device, or (b) a rig whose mainnet
   funding is **explicitly launched by Andjroo with a hard per-swap cap** — never the loop, never
   a default constructor.
2. **The secret leaves the initiator only inside a public claim tx.** No seed / no
   preimage-before-reveal crosses FFI, the mesh, or a log (audited in G8).
3. **The timelock refund is the safety floor.** Worst case is always: each side reclaims its own
   funds after its timeout. The confirmation-depth policy (ADR-0003) must be re-tuned for mainnet
   finality before any real leg is funded — Amoy's testnet depths are NOT safe against a Polygon
   or Bitcoin mainnet reorg.

### 8.2 Pre-mainnet-swap checklist (all green first)

- [x] **G8 independent contract + money-path review DONE (2026-07-09)** — the Solidity contracts
      and pure decision layers (ladder, `require_funded`, coordinator gates, checks-effects order)
      are SOLID; every finding is in the *wiring* / *live driver*. Verdict: **NO-GO until the below
      land** — but the code correctly hard-refuses mainnet today (Amoy chain-id pin, `fund_nim`
      testnet check, NIM-verifier pin), so this is a "what must be true when guards lift" verdict,
      not a live exploit.
  - [ ] **C1 (CRIT)** — one audited live-participant constructor that bakes in + ASSERTS the real
        gateway verifier (never `AcceptAllVerifier` — its default reopens S1), a CSPRNG secret
        source (never the public-derivable `sim_secret`), mainnet confirmation depths, and
        `term_anchor`. *(folded into the G10 build.)*
  - [ ] **H2 (HIGH)** — persist the #189 `initiated_ever` tombstone across restart (settled+reaped
        swaps currently lose it → the deterministic secret source could reissue a public `S`).
        *(folded into the G10 build.)*
  - [ ] **M3 (MED)** — evaluate the reveal-deadline BEFORE the on-chain `withdraw`; don't flood `S`
        until the reveal is buried beyond 1 confirmation. *(folded into the G10 build.)*
  - [ ] **M4 (MED)** — wire `term_anchor = head` on both signers + both verifiers; add an
        absolute-timelock sanity bound. *(folded into the G10 build.)*
  - [x] **M5 — RPC-trust hardening, testnet parts BUILT (2026-07-09, ADR-0011)** — the verifier
        cross-read seam (`with_secondary` on all three funding verifiers: NIM inclusion-block +
        conservative head; Amoy/Polygon head-within-tolerance + escrow re-read), the NIM
        content-hash bind (recompute `Blake2b(content)`), the `word_u64` overflow guard, and the
        independent reveal confirmation (the initiator re-reads the escrow `CLAIMED` on-chain
        before treating the swap settled — never the withdraw receipt alone; M5 × M3). Proven by
        lying-RPC unit tests + a live read-only cross-read on two independent Amoy endpoints
        (`docs/swap/M5-RECEIPTS.md`).
  - [ ] **M5 — mainnet operational remainder (needs:owner)** — actually WIRE a trusted /
        self-hosted secondary endpoint on the live path (a second INDEPENDENT NIM *testnet* RPC is
        already a self-hosted node — only `rpc.testnet.nimiqwatch.com` is public), and the
        guard-lift review. On mainnet the secondary should be a node the operator controls.
  - [ ] **M6 (mainnet-only)** — mainnet confirmation-depth retune (ADR-0003); Amoy/testnet depths
        are NOT reorg-safe on a mainnet chain (a secondary agreeing on a *shallow* head is still
        shallow). The guard-lift (evm_rlp Amoy pin, `fund_nim`, NIM verifier pin) is its OWN
        dedicated review, not a config flip.
- [ ] **Mainnet confirmation depths tuned** (ADR-0003 revisited) — NIM / Polygon / BTC each get a
      finality-safe depth, not the Amoy testnet values. Reorg re-verification path exercised.
- [ ] **Mainnet HTLC contract deployed + verified** on the counterparty chain (a fresh
      `NimmeshHtlc` on Polygon **mainnet**, forwarder-bound, source-verified on the explorer), if
      the counterparty asset is USDC. Its address recorded here.
- [ ] **Gas + dust reviewed for the real chain** — Polygon mainnet fee market (not Amoy), or BTC
      dust/fee; the responder's refund-reserving preflight re-checked against real gas.
- [ ] **Refund-recovery rehearsed on mainnet-shaped values** — a deliberately-abandoned swap
      refunds cleanly on both legs.
- [ ] **Hard per-swap cap wired** — the responder rig refuses any swap above a small cap
      (≤ the agreed test size, e.g. 1–5 USDC / a few NIM), enforced in code, not just config.
- [ ] **Andjroo's explicit written authorization** for the first mainnet swap, naming the asset,
      the cap, and which side he controls.

### 8.3 The chosen shape of the first mainnet swap (fill in once Andjroo picks the asset)

**Andjroo chose USDC on Polygon (2026-07-09)** — the path of least new risk: the HTLC contract
stack (`NimmeshHtlc` v2 + forwarder) is already deployed and live-proven on Amoy, so the only new
on-chain step is a **verified mainnet deploy of `NimmeshHtlc` on Polygon PoS** (source-verified on
polygonscan) plus a gated guard-lift. USDC on Polygon PoS = **verify the current canonical address
from Circle's docs before use** (native USDC vs bridged USDC.e differ — do NOT hardcode from
memory). Andjroo provides a small amount of mainnet USDC + a few POL for gas.

The safest first mainnet run keeps **both sides under Andjroo's control**: he funds the NIM leg
from his phone (signing on-device, exactly like the §7 payment) AND he controls the counterparty
responder — either running it himself, or launching the capped rig with an explicit go and
watching each broadcast. This is a swap *with himself* across two chains: it proves the mainnet
money-path with zero counterparty-trust exposure and a self-refund floor. Only after that proves
clean is a swap with a real third-party counterparty considered.

### 8.4 What lifts, exactly, and who does it (against the merged G10 code, v0.68.0)

The testnet swap runs entirely through two FFI constructors — `MeshNode::new_live_swap_initiator`
(the phone) and `new_live_swap_responder` (the Mac), both **hard-pinned to testnet/Amoy and
C1-asserted at the door** (a live signer cannot be built with `AcceptAllVerifier` or `sim_secret`,
enforced in `MeshNode::build`). A mainnet swap is a *new, off-by-default, `money-path` +
`needs:owner` PR* (never auto-merged) that lifts exactly these points — each a deliberate change,
not a config flip:

1. `rpc::HttpGatewayRpc::new_mainnet` + the `polygon_gateway` mainnet host allow-list.
2. The `guard_testnet` / `guard_amoy` constructor guards.
3. `fund_nim`'s testnet-network assertion (the NIM leg's network pin).
4. `MeshNode::build`'s testnet-only live-signer assertion (so a live signer may ride a mainnet node).
5. The `ConfirmationPolicy` testnet depths → mainnet-finality depths (M6 / ADR-0003).

And it requires, per the G8 review, **M6** (the depth retune above) plus the **mainnet operational
remainder of M5**. M5's *testnet-buildable* half is DONE (ADR-0011): the verifier cross-read seam,
the NIM content-hash bind, the `word_u64` guard, and the independent reveal confirmation (the
initiator re-reads the escrow `CLAIMED` on-chain before treating a swap settled — never the
withdraw receipt alone). What remains for mainnet is wiring a **trusted / self-hosted** secondary
endpoint on the live path (not two third-party public endpoints) and the depth retune. Plus a
fresh, source-verified `NimmeshHtlc` deployed on Polygon mainnet, a hard per-swap cap enforced in
code, the §8.2 checklist all green, an independent review of the guard-lift diff, and Andjroo's
written authorization.

Who does what on the first run: **Andjroo signs the NIM leg on his phone** (on-device, exactly like
§7 — no new autonomous surface) and **controls the USDC responder** (running it himself, or
launching the capped rig with an explicit go and watching each broadcast). The responder's `newSwap`
is the one real-value action a *node* takes — which is why it is capped, explicitly launched, and
his to trigger, never the loop's. Everything else stays testnet; no default constructor changes.

*(The cap value + the mainnet HTLC address get filled in with the guard-lift PR; the go/no-go from
G8 review get filled in before it is proposed as a PR.)*
