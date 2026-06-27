# nimiq.nimmesh — Mesh Swap build loop

> The standing contract the **mesh-swap** loop reads at the start of every cycle. The
> feature spec is [SWAP.md](./SWAP.md). This loop runs on the **`feat/mesh-swap`** branch
> only; it never edits the base `docs/LOOP.md` / `docs/GOAL.md` / `docs/PROTOCOL.md`
> (another chat owns `main`) — all swap work is **additive** (new modules, new docs) so
> the branch rebases cleanly.

## North star

Sign and complete a **trustless cross-chain HTLC atomic swap with no internet between the
two parties** — the negotiation, the signed funding txs, and the secret reveal all ride the
nimmesh BLE mesh; each leg settles on-chain at the first internet hop. First real pair:
**NIM ⇄ BTC** (both native HTLC). Testnet-first; the real BTC leg + mainnet are gated.

## Operating model — autonomy & gating

Inherits the base project's updated rule: **TESTNET is full-speed, auto-merge on green
(including keygen / signing / NIM-testnet broadcast).** The gates are:

- **The real Bitcoin leg** (a real BTC node + real-fund signing/watching) → `needs:owner`.
- **Mainnet** anything (NIM or BTC), pointing at a mainnet RPC, real funds → `needs:owner`,
  never authored by the loop.
- **Real devices** (on-device BLE swap interop) → `needs:owner` (the base G5 shim gate).
- **One invariant regardless:** no seed/preimage-before-reveal/secret material ever crosses
  the FFI boundary, the mesh, or a log unless it is broadcast-safe by construction. The
  preimage S is revealed only inside a signed claim tx that is *meant* to be public.

Because this is a long-lived feature branch (not merging to `main` mid-flight while the other
chat finishes C1), cycles **commit to `feat/mesh-swap`** and verify locally; they do **not**
auto-merge to `main`. Merge to `main` is a single reviewed step once the foundation is proven
and `main` is at a clean point — proposed to Andjroo, not taken by the loop.

## Goals — one PR-sized commit per goal, files < 800 lines

| # | Goal | gated? | deps | status |
| --- | --- | --- | --- | --- |
| **F0** | Spec + design spike — SWAP.md / SWAP-LOOP.md + confirm the exact Albatross HTLC byte layout from `@nimiq/core` 2.7.0 (timeout units, hash-algo enum, contract-data + creation-tx + resolve-proof layouts) | no | — | 🟡 docs done · byte-layout spike next |
| **F1** | Nimiq **HTLC tx serialization** — extend the signer from `Basic` to: HTLC **creation** (Extended format), `regular-transfer` (claim-with-preimage) and `timeout-resolve` (refund) proofs; **byte-exact vs `@nimiq/core` 2.7.0** with committed fixtures (`nimiq/htlc.rs` + `scripts/fixtures` HTLC cases) | no (testnet) | F0 | 🟡 funding tx + redeem content byte-exact (6 tests green); resolve **proof** = next sub-cycle (core-rs-albatross gate) |
| **F2** | Swap **wire messages + codec** — MessageType `0x40–0x44` + the swap TLV envelope; encode/decode + proptests (`swap_wire.rs`, extend `packet.rs`/`envelope.rs`) | no | — | ✅ done (11 tests; `SwapEnvelope` TLV codec + per-kind required-field enforcement) |
| **F3** | Swap **state machine** — `swap.rs`: roles (initiator/responder), lifecycle (`Proposed→Accepted→Funded→Revealed→Settled` / `Aborted`/`Refunded`), **height-anchored clock-free timelock ladder** + the `Δ_safe` safety gate (refuse unsafe-offline) | no | F2 | todo |
| **F4** | **Mesh integration + mock-counterparty e2e** — engine glue to flood/relay/store-forward swap msgs over the existing mesh; the `SwapLeg` trait + `NimiqLeg` + a mock `BitcoinLeg`; `swap_e2e_tests.rs` proving the happy path **+ all 4 adversarial paths** (no one-sided settlement) | no | F1, F3 | todo |
| **F5** | **Real Bitcoin leg seam + stub** — the `BitcoinLeg` P2WSH-HTLC trait surface + a documented stub + a "what Andjroo must provide" note (BTC node, funds). Real signer/watcher = **gated** | stub: no · real: **yes** | F4 | todo |
| **F6** | **Swap UI** (nimiq-ui, screenshot-verified) — propose/scan/fund/reveal/status sheets, "this swap is safe offline / not safe now" honesty line. Built against real nimiq-ui refs | no | F4 | todo |

**MVP of the feature (autonomous):** F0–F4 + the F5 stub. F5-real + F6-on-device are gated.

## Per-cycle workflow

1. Re-read SWAP.md + this file + the base `docs/RISKS.md`. Pick the top open goal whose deps
   are done. Confirm you are on `feat/mesh-swap` and rebased on the latest `origin/main`.
2. Build it in a new module; keep every file **< 800 lines** (the repo's `size-guard.sh`).
   Record any real decision in `docs/adr/` (additive ADR numbers only).
3. **Verify locally:** `cargo test` + `cargo clippy -D warnings` + `cargo fmt --check` +
   `scripts/size-guard.sh`, all green on the Mini. For F1, regenerate + diff the
   `@nimiq/core` fixtures (the byte-exact gate). Add tests that prove the goal.
4. Commit to `feat/mesh-swap` with a clear message; append a one-line entry to the cycle log
   below. Push the branch (visible, isolated — never touches `main`).
5. **Money-path test reality:** per the inherited rule, testnet signing/broadcast is *not*
   gated, but anything touching **real BTC / real funds / mainnet** stops and is reported
   under `needs:owner`.

## Guardrails — the never list (swap-specific, on top of the base list)

- **Never** produce a swap path that can settle **one-sided** — the timelock ladder
  (`T_A > T_B + Δ_safe`) is mandatory; the e2e suite must prove no one-sided outcome exists.
- **Never** reveal the preimage S except inside a signed, broadcast-safe claim tx. S never
  rides the mesh as bare bytes, never hits a log.
- **Never** enter `Funded` when the reachability/`Δ_safe` gate says the swap can't be made
  safe offline — refuse and surface it.
- **Never** build or broadcast a **real BTC** tx or any **mainnet** tx in the loop — the real
  BTC leg and mainnet are `needs:owner`.
- **Never** let a relay parse a swap to act on it — relays carry swap bytes blind, same as txs.
- **Never** edit the base `docs/LOOP.md`/`GOAL.md`/`PROTOCOL.md` or `main` from this loop —
  additive only, rebase clean, merge is one reviewed step later.

## Reserved for Andjroo (`needs:owner`)

- The **real Bitcoin leg**: a testnet-BTC (then mainnet) node/RPC + the P2WSH-HTLC signer +
  real funds to swap. (The loop ships the trait + stub + the spec.)
- **The first real cross-chain testnet swap** authorization, and the mainnet switch.
- **Branding / UX** of the swap screens (F6 mirrors nimiq-ui; final identity is Andjroo's).
- **When to merge `feat/mesh-swap` → `main`** (after the other chat's C1 lands and main is clean).

## Cycle log

- **2026-06-27** — Loop initialized on `feat/mesh-swap` (isolated clone
  `~/projects/nimiq.nimmesh-htlc`, branched off `origin/main` @ `b5f7cfa`, so the other
  chat's C1 work on `main` is untouched). Researched Bitchat (BLE mesh, 7-hop TTL, Noise_XX,
  store-and-forward — all already mirrored in nimmesh) and confirmed **Nimiq's native HTLC
  account type** supports atomic swaps (timeout-resolve / regular-transfer / early-resolve).
  Wrote SWAP.md (feature spec) + this loop. First real pair = **NIM ⇄ BTC** (Andjroo's pick).
  **F0 docs done.** Next: the F0 byte-layout spike → F1 Nimiq HTLC serialization.
- **2026-06-27** — **F0 spike done + feasibility documented.** Nailed the Albatross HTLC byte
  layout vs `@nimiq/core` 2.7.0 (creation data 82 B; sha256 algo = 3; timeout = u64 block
  height; contract address = `Blake2b256(content w/ recipient zeroed)[..20]`). Proved a real
  HTLC **funding tx is ACCEPTED** by `@nimiq/core`'s own validator (`feasibility-test.mjs`);
  found `@nimiq/core` JS can't sign/verify HTLC **redeems** → that path is gated against
  `core-rs-albatross`/live-testnet, not JS. Documented the verdict in `FEASIBILITY.md`
  (settlement-vs-transport: you create+relay an *offline transaction*, it settles at the gateway).
- **2026-06-27** — **F1 (1/2): funding leg byte-exact.** `nimiq/htlc.rs` (397 lines):
  `HtlcCreationData` (82 B), `HtlcCreation` (contract-address derivation + content + hash +
  extended wire, signed shape = the 248 B verified tx) and `HtlcRedeem` content — all asserted
  **byte-exact against the `@nimiq/core` fixtures** (`swap_htlc_fixtures.json`). 6 new tests,
  **207 lib tests green**, fmt/clippy/size-guard clean. Next sub-cycle: the resolve **proof**
  (RegularTransfer preimage + TimeoutResolve), gated against core-rs-albatross / a testnet redeem.
- **2026-06-27** — **F2 done: swap wire messages + codec.** Added MessageType `0x40`–`0x44`
  (`SwapPropose`/`Accept`/`FundingProof`/`PreimageReveal`/`Abort`) to `packet.rs` + the new
  `swap_wire.rs` — a `SwapEnvelope` TLV codec (mirrors `envelope.rs`) carrying swap_id, hashlock,
  amounts, block-height timeouts, NIM + chain-agnostic counterparty addresses, leg, **opaque** signed
  tx blob + txId, networkId, abort reason. `decode_swap(kind, ..)` enforces the per-kind required
  fields + strict bounds/length/enum checks; unknown TLVs skipped (forward-compat). **11 tests,
  218 lib tests green**, fmt/clippy/size-guard clean. Public broadcast-safe data only — no keys,
  no preimage-before-reveal. Next: **F3** (swap state machine + the `Δ_safe` timelock-safety gate).
