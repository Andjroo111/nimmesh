# Overnight 2026-07-13 — phone-responder, BTC verifier, mainnet guard-lift

Three sequential PRs toward the straight-to-mainnet, phone-to-phone, self-swap plan (NIM + USDC +
BTC, small hard-capped amounts). The non-negotiable floor held: **the agent moved no real funds,
lifted no mainnet guard on a merged branch, and did not merge the guard-lift.**

Execution order was A → C → B (not A → B → C) so the version bumps stay contiguous and the dangerous
guard-lift (B) was prepared against the most complete `main`. Nothing depended on the reorder.

## The three PRs

| PR | What | Version | State |
|----|------|---------|-------|
| **#208** | Phone-as-responder swap mode (the missing half of a phone→phone swap) | 0.73.0 | ✅ **MERGED** (squash) |
| **#209** | BTC-leg funding verifier (#72 tail, the third chain) | 0.74.0 | ✅ **MERGED** (squash, auto-merge on green) |
| **#210** | GUARD-LIFT: mainnet swap, off by default | 0.75.0 | ⛔ **OPEN — Andjroo-merge only, DO NOT auto-merge** (labels: `needs:owner`, `money-path`) |

### #208 — phone-as-responder mode (merged)
The app could only be the swap initiator (gives NIM, receives USDC); the responder (gives USDC,
receives NIM) lived only in the Mac rig. #208 adds it to the phone via the same testnet/Amoy-pinned,
C1-asserted FFI ctor (`MeshNode.newLiveSwapResponder`), so two phones can swap with no Mac in the
loop. A "Respond to swaps" toggle in the Swap sheet (bridge-gated, mutually exclusive with the "real"
toggle, honest LIVE-testnet labels), wallet-derived recoverable escrow/claim accounts, i18n in all 5
langs. App wiring only — no core change, testnet-inert until the guard-lift. Playwright mock-bridge:
16/16 checks. **The OTA/ipa rebuild is NOT done here — the interactive session handles the device
build.**

### #209 — BTC-leg funding verifier (merged)
`btc_verifier::BtcHtlcVerifier` — the BTC sibling of `nim_verifier`/`polygon_verifier`, against the
same `require_funded` gate. Locates the P2WSH HTLC by its script-derived address, binds it by
recomputing the exact scriptPubKey, reports depth (tip − block + 1), M5 cross-read (mempool.space +
blockstream.info), fail-closed everywhere. Pure logic + 14 offline reads-fake tests are
default-feature; the `BtcHtlcParams` derivation is behind `bitcoin-leg`, the live HTTP reads behind
`bitcoin-gateway`. `chain_backed = false` (raw CLTV seconds, like `polygon_verifier`) — **testnet-inert;
live on-chain proof is GATED (the BTC wallet is empty).**

### #210 — the mainnet guard-lift (OPEN, Andjroo-only)
Lifts exactly the §8.4 points behind ONE off-by-default master switch, `mainnet_swap::
MAINNET_SWAP_ENABLED = false`. While false every gated guard still refuses mainnet, so the merged
branch is byte-identical to testnet-only (465-test default suite unchanged). Contents:
- Lifts (flag-gated): `fund_nim` network refusal, `MeshNode::build` live-signer assertion.
- Polygon mirror: `guard_polygon_mainnet` + `HttpPolygonRpc::new_mainnet` (allow-list drpc +
  publicnode), `NimHtlcVerifier::new_mainnet`, `LegacyTx::polygon_mainnet` (chainId 137).
- Config: `ConfirmationPolicy::mainnet_defaults()` NIM 10 / USDC 64 / BTC 2 (ADR-0003 addendum);
  native USDC `0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359` (Circle docs, NOT bridged USDC.e).
- Hard per-swap caps in code: `SwapCaps::mainnet_first_swap()` ≤ 50 NIM / ≤ 5 USDC / ≤ 20 000 sat,
  enforced at the coordinator gate (responder-accept + initiator-propose), tested.

## What Andjroo must review + do (PR #210)

1. **Review the guard-lift diff** (the §8.4 checklist is in the PR body).
2. **Deploy** a source-verified `NimmeshForwarder` + `NimmeshHtlc` on Polygon mainnet (commands ready
   in the PR body; the agent did NOT deploy) and record the HTLC in `mainnet_swap::MAINNET_HTLC_ADDRESS`.
3. **Wire** the mainnet swap constructors (§8.3 first-run) — the flag alone does nothing without a
   caller selecting the `new_mainnet` RPCs + `mainnet_defaults` + caps + `NetworkId::Mainnet`.
4. **Flip** `MAINNET_SWAP_ENABLED = true` and give written go for the ≤ $5 first self-swap.
5. **Accept the NIM single-RPC residual risk** (no second public Nimiq mainnet RPC exists — only
   `rpc.nimiqwatch.com`; the USDC leg's two-RPC cross-read IS wired) — or stand up a self-hosted NIM
   node as the M5 secondary first.

## Funding addresses he needs

- **Responder EVM (USDC escrow + gas):** derived in-app from the wallet (HKDF label
  `nimmesh-swap-evm-fund-v1`), so it is per-wallet and **shown in the app's responder panel**
  (tap-to-copy) when "Respond to swaps" is on — fund it with USDC + POL. It is NOT computable
  offline (it needs the phone wallet's entropy).
- **BTC test wallet** (`docs/swap/WALLETS.md`): `tb1q4n9al5rnhtfgpg4sd5qlpayc77qkf8hs026cjj`
  (testnet3/signet; seed `NIMMESH_BTC_SEED` in `~/secrets/nimmesh-swap-wallets.env`). The
  **mainnet** BTC leg has no wallet yet — the BTC verifier's live proof is gated on an empty wallet.

## Blockers / not done (all expected)

- The **OTA/ipa rebuild** for #208's responder mode — the interactive session owns device builds.
- **Mainnet contract deploy + flag flip + first-run wiring** — all Andjroo-gated (PR #210).
- **NIM mainnet second RPC** — none exists publicly; Andjroo's call.
- No live mainnet swap was run; no real funds moved.
