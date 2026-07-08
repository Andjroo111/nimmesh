# ADR-0010 — The live NIM⇄USDC signer: term↔wall-clock mapping, peer addressing, and funding-wire hints

- **Status:** accepted (A2b, 2026-07-08)
- **Context:** Act 2 — the first REAL money-path `SwapSigner` (Albatross TESTNET ⇄ Polygon Amoy),
  wired through the seams `swap_signer` v2 opened (0.62.0). Everything here is testnet-only;
  mainnet and real funds stay behind `docs/swap/OWNER-GATED.md` / `docs/MAINNET-GATING.md`.

## 1. Who does what on-chain (the NIM⇄USDC role map)

| Protocol phase | Initiator (NIM-giver) | Responder (USDC-giver) |
|---|---|---|
| `Accepted` | funds the **NIM HTLC**: `htlc_sender` = its funded NIM wallet (the refund party), `htlc_recipient` = the **responder's** NIM claim address (from its `Accept`), `H = SHA-256(S)`, timeout = ms-mapped `T_A`, value = `give_amount` **luna** | — |
| `InitiatorFunded` | — | funds the **Amoy escrow**: `approve` + `newSwap(receiver = the initiator's EVM claim address from its `Propose`, amount = `take_amount` **micro-USDC**, `H`, timelock = s-mapped `T_B`)` |
| `BothFunded` | claims USDC: `withdraw(S)` — the ON-CHAIN reveal; the mesh `PreimageReveal` wire is `S ‖ raw tx` | — |
| `Revealed` | — | claims the NIM HTLC with `S` (RegularTransfer) to its own NIM claim address |
| timeout | refunds the NIM HTLC after `T_A` (TimeoutResolve) | refunds the escrow after `T_B` (`refund`) |

Units are fixed by convention for this pair: `give_amount` is luna, `take_amount` is micro-USDC
(`SwapContext.btc_address` carries the initiator's 20-byte EVM claim address — the chain-agnostic
field doing exactly what 0.62.0 documented).

The initiator's `withdraw(S)` may be **paid by a different key than the payout address**
(`NimmeshHtlc`'s claim is caller-open, ADR-0007): the rig uses the funded Amoy key as the gas
payer while the escrow pays the initiator's distinct claim address — the same relayer property
the G7 gasless proof exercised.

## 2. Term ↔ on-chain timeout mapping (the decided fork)

Session terms `T_A`/`T_B` are **mesh-anchored, second-granularity units** (the sim ladder's
"block-ish" numbers; 1 unit ≈ 1 s ≈ 1 Albatross block). The chains want absolute timestamps in
**different units**: the NIM HTLC `timeout` is Unix **milliseconds**; the Amoy `timelock` is Unix
**seconds**. Decision: each side maps INDEPENDENTLY at act time, anchored on its own wall clock —

- funder side: `nim_timeout_ms = now_ms + (T_A − anchor)·1000` · `usdc_timelock_s = now_s + (T_B − anchor)`
- verifier side: re-express the on-chain value in term units **plus a slack**
  (`(on_chain − now)/unit + slack + anchor`) and let the ONE existing gate
  (`require_funded`'s `timeout ≥ min_timeout`) enforce the wall-clock floor
  `on_chain ≥ now + (T − anchor − slack)·unit`.

`anchor` is the mesh head the terms were built against; `slack` (default **900 s** both sides)
absorbs the fund→verify wall-clock gap and stays far under the ladder's hour-scale margins. This
derives everything from the agreed terms (no side-channel), needs no shared clock, and keeps
`require_funded` the single go/no-go.

**Known limitation (deliberate):** the rig e2e runs beacon-silent, so the mesh anchor is 0 and
the sim terms (`T_A=10 000`, `T_B=5 000`) read as ~2.8 h / ~1.4 h windows — the ladder gates stay
exact in term units. A beacon-anchored deployment MUST pass the anchoring head into the signer +
verifier configs (`term_anchor`), or a head in the millions would map to absurd timeouts. A
head beacon arriving mid-swap would also flip `is_stale`/refund heuristics that compare term
units to the head — the rig keeps gateways (and therefore beacons) off the harness.

## 3. Peer addressing: `SwapSigner::note_peer` (default no-op)

`SwapContext` deliberately carries only **this** node's identity; the counterparty's payout
addressing rides the protocol (`Propose` → the initiator's NIM refund + EVM claim address;
`Accept` → the responder's NIM claim + chain refund address) but was retained nowhere a signer
could reach. Rather than widening `SwapContext` (which is persisted in crash-recovery snapshots
and constructed in dozens of places), the driver now REPORTS the addressing to the signer seam:
`drive_swap` calls `signer.note_peer(swap_id, nim_address, chain_address)` on a `Propose`/`Accept`
**only for a swap this node holds a coordinator for**. The live signers store it in a bounded,
first-report-wins `PeerBook`.

Trust note: the `Propose` is signature-verified before a responder coordinator exists, so the
responder's note is authenticated. The `Accept` is **not** authenticated (the known S2 scope
bound) — first-report-wins mirrors the coordinator's own first-`Accept`-wins for
`peer_btc_pubkey`. A raced forged Accept can redirect the NIM HTLC's recipient, but never steal:
the initiator only reveals `S` after verifying an escrow paying ITSELF, so a phantom counterparty
just stalls the swap into the refund path (or, if the forger actually funds the escrow, it simply
IS the counterparty). Authenticating `Accept`/`FundingProof` end-to-end remains follow-up work.

## 4. Funding-wire hints: `FundingVerifier::note_funding_wire` (default no-op)

The session now feeds every `FundingProof`'s `tx_wire` (leg-tagged, only for tracked swaps) to
its verifier as an **untrusted locating hint**:

- **NIM leg** (`nim_verifier::NimHtlcVerifier`): the decoded creation's params are checked
  against the expectation FIRST, then bound to chain truth by construction — the canonical tx
  hash and the contract address are both collision-resistant digests of the full creation
  content, so `getTransactionByHash` (inclusion + depth) and `getAccountByAddress` (a live HTLC
  holding ≥ the agreed amount) can only confirm the EXACT decoded tx. A forged wire makes the
  verifier look somewhere empty → `Absent` → fail-closed.
- **USDC leg** (`live_swap_signer::AmoyHtlcSwapVerifier`): the public Amoy RPC caps
  `eth_getLogs` at ~50 blocks, so a blind lookback cannot work; the named funding tx's RECEIPT
  anchors the scan window instead, then the deployed-contract scan (recipient-indexed `NewSwap`
  → hashlock match → `getSwap` still `Live` → real depth) decides, and the found `swapId` is
  recorded for the claim. No receipt yet → `Absent`. If a swap outlives the log-range cap
  (funding buried > ~45 blocks while still unverified) the scan errors → `Absent` → the swap
  stalls to its refund — safe, documented, and irrelevant at the rig's poll cadence.

Both hints ride shared stores (`NimFundingStore` / `PolygonFundingStore`) so the same node's
claim reuses exactly the contract/escrow its verifier confirmed.

## 5. What this does NOT change

- No wire-format, coordinator, or state-machine changes; both new trait methods default to
  no-ops, so every existing signer/verifier (sim, ledger, Polygon) is untouched.
- `guard_testnet`/`guard_amoy` remain the only chain doors; `fund_nim` additionally refuses a
  context not stamped `NetworkId::Testnet`.
- The phone keeps `MockSigner`; the live signers enter through the non-FFI rig door
  (`MeshNode::new_session_participant`) only.
