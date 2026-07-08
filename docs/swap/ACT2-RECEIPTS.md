# Act 2 receipts — a REAL NIM⇄USDC atomic swap over the REAL protocol (2026-07-08)

Two participant `MeshNode`s (the shipping Rust core, deterministic-harness radio) drove
complete cross-chain atomic swaps with **real testnet money on both chains** — Albatross
TESTNET on the NIM leg, Polygon Amoy on the USDC leg — through the full protocol with zero
manual steps: standing-intent discovery → signed `Propose`/`Accept` → verifier-gated NIM HTLC
→ verifier-gated USDC escrow (deployed `NimmeshHtlc` v2) → `withdraw(S)` (the on-chain reveal)
→ NIM claim with the mesh-carried `S` → chain-truth settlement + treasury sweep.

Tool: `cargo run --example live_mesh_swap_nim_usdc --features "polygon-gateway gateway-rpc"`
(A2b core: PR #187 · A2c example: PR #188 · design: `docs/adr/0010`).

## The actors

| Role | Address |
|---|---|
| Treasury (funds alice's NIM leg; every refund/sweep destination) | [`NQ92 VGEX VYH9 KHP0 Y00L DAQM 32N2 8H12 H9F7`](https://nimiq-testnet.observer/#NQ92+VGEX+VYH9+KHP0+Y00L+DAQM+32N2+8H12+H9F7) |
| Bob's NIM claim address (session identity, derived label off the treasury seed) | [`NQ24 SSBE 32R4 TCXG JRG2 NDJY T695 KN6U E289`](https://nimiq-testnet.observer/#NQ24+SSBE+32R4+TCXG+JRG2+NDJY+T695+KN6U+E289) |
| Funded Amoy wallet (bob's escrow funder + alice's claim gas payer) | [`0xA7bB819Ba03743643249dFCCa7508280eCE059b1`](https://amoy.polygonscan.com/address/0xA7bB819Ba03743643249dFCCa7508280eCE059b1) |
| Alice's EVM claim address (distinct derived payout account) | [`0x7edbd52b702133b9707dfabeba64af4c4fbbf3de`](https://amoy.polygonscan.com/address/0x7edbd52b702133b9707dfabeba64af4c4fbbf3de) |
| `NimmeshHtlc` v2 (forwarder-bound, `docs/swap/AMOY.md`) | [`0xb3B3703E07AC897B7E3e864C113a2Fa547D76736`](https://amoy.polygonscan.com/address/0xb3B3703E07AC897B7E3e864C113a2Fa547D76736) |

Trade both runs: **5 tNIM (500 000 luna) ⇄ 1 USDC (1 000 000 µUSDC)**.

## Run 2 — the clean, fully-autonomous green run (exit 0)

Discovery → both `Settled` in ~6 protocol ticks (~20 s of protocol time; chain waits dominate).
`H = SHA-256(S) = 0xdcb2d5fb517ef394bffb1be6f41e1d8dfa23d9c7d15a052fd1ac28d0ed2f3644`.

| Step | Chain | Tx |
|---|---|---|
| 1. Alice funds the NIM HTLC `NQ53 QMAX JK4H EXYL BXBM CY5R FS46 1A3X FYGA` (500 000 luna, recipient = bob's claim address, timeout = ms-mapped `T_A`) | NIM testnet | [`019c1afb…1190`](https://nimiq-testnet.observer/transactions/019c1afb21c5bdfb5ab67a9871f10ddef67854c0e8cf9b3b2b0abec607cb1190) |
| 2. Bob's REAL `NimHtlcVerifier` confirms it on-chain (depth ≥ 2) — only then `approve` | Amoy | [`0x0289c642…329c`](https://amoy.polygonscan.com/tx/0x0289c642832ec4e1dff31a74f4545c347d6cbb77679b9c90d9f0af138b45329c) (block 41 720 215) |
| 3. …then `newSwap` — escrow `swapId 0x3f8cfdc67baf031f146749507ca3a18df04120dd54348e65c8eb653a632ea3c5`, 1 000 000 µUSDC, receiver = alice's claim address, timelock 1 783 543 611 (s-mapped `T_B`) | Amoy | [`0x43189eaa…49ee`](https://amoy.polygonscan.com/tx/0x43189eaa9111b3bf922f3de6b90e4329c863beae3c2c1e69e1890f6d51f649ee) (block 41 720 219) |
| 4. Alice's `AmoyHtlcSwapVerifier` confirms the escrow (depth ≥ 5, anchored at tx 3's receipt) — only then `withdraw(S)`: **the on-chain reveal** | Amoy | [`0x3e3fad37…f4e4`](https://amoy.polygonscan.com/tx/0x3e3fad371869e66ddbc61b2ba06de060742608d8d38efcf357d14e01b91df4e4) (block 41 720 224) |
| 5. Bob reads `S` off the mesh `PreimageReveal` (`S ‖ raw-tx` wire) and claims the NIM HTLC (RegularTransfer, SHA-256 preimage proof) | NIM testnet | [`d970849c…492d`](https://nimiq-testnet.observer/transactions/d970849cdcd08587f715a447c362494995d34faae536cf3f0b9aeea5417b492d) |
| 6. Settle-up: bob's full claim balance swept home to the treasury (1 500 000 luna — includes run 1's claims) | NIM testnet | [`daaac589…886b`](https://nimiq-testnet.observer/transactions/daaac589e8e7b11a1033a10f400edf18314dcb0a6e8b50a8eff67f75bc88886b) |

Ground truth re-checked by the example before declaring success: NIM HTLC balance 0 · escrow
state `Claimed` · alice's claim address +1 USDC · bob's claim address +500 000 luna.

## Run 1 — the first execution (full atomic swap, then two lessons)

The very first live run of the protocol also completed a **full atomic swap**:

| Step | Chain | Tx |
|---|---|---|
| NIM HTLC `NQ13 82SU YVBH A0P6 NLA1 YUFH 2H0M 29R6 VST1` funded (500 000 luna) | NIM testnet | [`f83f5f22…0376`](https://nimiq-testnet.observer/transactions/f83f5f22c575331c06864a246d19fd8791d75cf812da76c02cace8be82c60376) |
| `approve` | Amoy | [`0x4826c501…5dfd`](https://amoy.polygonscan.com/tx/0x4826c501bcf556fcca3663747e2897c8e9bab9b5da4e9123debbef7a70ad5dfd) (block 41 719 298) |
| `newSwap` — `swapId 0xcff8cc84bdc2db978c3094c55b6b7f542d72160d3f01a5e6e63ef8f76d5c4518`, timelock 1 783 542 693 | Amoy | [`0x4a681981…f4dc`](https://amoy.polygonscan.com/tx/0x4a681981c16ecd38befc17ce8025393177037891fac640b878b8d0e4bd55f4dc) (block 41 719 301) |
| `withdraw(S)` — revealed `S = 0x78d36270fd320928d84518278f6cf7a67e18ee856e10deb7ce37e1130e1e3627`, `H = 0xb7806d2d…8260` | Amoy | [`0xcfe8268a…e33d`](https://amoy.polygonscan.com/tx/0xcfe8268a7c2bc99849f13e7af85e6bc60b2e8a789ea375fbe71008a00c9ce33d) (block 41 719 306) |
| NIM claim by bob | NIM testnet | [`724219a8…5985`](https://nimiq-testnet.observer/transactions/724219a8ced262198cc95574c0c4cc0e3c7267ffcc0ba88225052043c3e05985) |

**Lesson 1 — standing intents re-match (by design).** Three ticks after both sides settled,
discovery matched the SAME counterparty again and alice funded a second real NIM HTLC
([`55bcf9d3…5d44`](https://nimiq-testnet.observer/transactions/55bcf9d3cadc54a0058934d4318cbc67310e755ecfdd1e95820d0c1967795d44)
→ `NQ09 E0FN 25RA GAJ9 LA9R 4KC9 PBFN LTQL LLDX`). The run was killed there; the example now
carries a **one-shot funding latch** (a repeat negotiation stays unfunded), chain-truth
completion detection, and lock persistence + startup refund-recovery.

**Lesson 2 — deterministic swap-ids must never reuse a secret.** The repeat swap derived the
SAME `swap_id` (it is a digest of the two parties' identities), so the per-swap secret source
(`sha256(master ‖ swap_id)`) handed out the SAME `S` — whose value was **already public** in
run 1's `withdraw` calldata. Consequence: the repeat HTLC was claimable the moment it was
funded, with no counterparty escrow — a one-sided-loss shape if the counterparty were
malicious. Here it was benign (the HTLC pays only bob, whose key we hold), and the lock was
recovered by claiming it with the already-public secret
([`b732332e…901f`](https://nimiq-testnet.observer/transactions/b732332e7cf7c17d46807c112a360c27816cadbeee22db3c493ec0719d1b901f)).
Filed as a core follow-up: a secret source must never issue the same `S` twice (mix a
per-issue nonce, or refuse to re-initiate a used `swap_id`) — tracked in the repo issues.

## Money movement (net, across both runs)

| Account | Before run 1 | After run 2 | Net |
|---|---|---|---|
| Treasury NIM | 11 000 000 000 luna | 11 000 000 000 luna | **±0** (3 × 500 000 funded, 1 500 000 swept home; NIM fees are 0) |
| Bob's NIM claim address | 0 | 0 (swept) | ±0 |
| Funded Amoy wallet USDC | 20.000000 | 18.000000 | −2 USDC (two escrows paid out) |
| Alice's EVM claim address USDC | 0 | 2.000000 | +2 USDC (the swap payouts — deliberately on a distinct payout account) |
| Funded Amoy wallet POL | 0.095296 | 0.060132 | −0.035 POL (gas: 2×approve, 2×newSwap, 2×withdraw) |

(The run-2 log line `AFTER treasury 0 luna` was a transient RPC read error rendered as 0 by
the balance helper — the chain shows 11 000 000 000; the helper now retries.)

## Secret-reveal chain of custody

1. `S` is drawn by alice's session from a fresh per-run CSPRNG master (never persisted, never
   printed); only `H = SHA-256(S)` enters the `Propose`.
2. Both HTLCs commit to `H` — the NIM contract's SHA-256 hashlock and the escrow's
   `sha256(secret)` check (the 0x02 precompile) are byte-identical locks.
3. `S` first leaves alice inside the Amoy `withdraw(S)` calldata — **the reveal IS the claim**;
   the signer refuses to flood a reveal for anything but a mined, successful withdraw.
4. The mesh `PreimageReveal` wire carries `S ‖ raw-tx`; bob verifies `SHA-256(S) = H` and
   claims the NIM HTLC with the same preimage. One secret, two chains, no trusted party.

## What is left where (nothing stranded)

- **Nothing is time-locked anywhere.** All three NIM HTLCs and both escrows are resolved.
- **2 USDC** rest on alice's claim address `0x7edbd5…f3de` — deliberate (the payout account
  proves the escrow paid a DISTINCT party) and fully recoverable: its key is
  `sha256(AMOY_TEST_KEY ‖ "nimmesh-act2-alice-claim-v1")`; sweep any time via an EIP-2612
  permit + `transferFrom` (zero POL needed on the account), or fund it with dust POL and
  `transfer`.
- The state file `~/.nimmesh-act2-state.json` is removed on a clean exit; if a run dies, the
  next invocation refunds what expired and refuses to start while anything is still locked.

## Re-running

```
set -a; . ~/.nimmesh-amoy.env; . ~/secrets/nimmesh-swap-wallets.env; set +a
cargo run --example live_mesh_swap_nim_usdc --features "polygon-gateway gateway-rpc"
```

Needs ~0.04 POL headroom and 1 USDC on the funded wallet; the NIM side tops itself up from
the faucet. Every run: recovery pass → one full swap → ground-truth checks → sweep.
