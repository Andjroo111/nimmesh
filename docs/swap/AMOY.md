# Polygon Amoy — the deployed USDC HTLC (G6, #77)

The G5 contract (`contracts/src/NimmeshHtlc.sol`, ADR-0007) is **live on the Polygon Amoy
testnet**, escrowing Circle's canonical test USDC. This file is the deployment record and the
operations notes for the live round-trip.

| What | Value |
|---|---|
| `NimmeshHtlc` | [`0xaaCa309B5EF3e57D3f206220F230F5cB2562F7f3`](https://amoy.polygonscan.com/address/0xaaCa309B5EF3e57D3f206220F230F5cB2562F7f3) |
| Deploy tx | [`0x882c1bd7…dc5b`](https://amoy.polygonscan.com/tx/0x882c1bd71b88e1cde23947a1970ecc9d67468a1c72d23da5b1e3aa76daf3dc5b) |
| `token()` | Circle Amoy USDC [`0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582`](https://amoy.polygonscan.com/address/0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582) (6 decimals, EIP-712 domain version "2") |
| `trustedForwarder()` | `address(0)` — ERC-2771 **disabled** for G6; the G7 gas-abstraction implementation deploys a forwarder and a fresh HTLC bound to it (the field is immutable by design, ADR-0007) |
| Chain | Amoy, chainId **80002** (`evm_rlp::POLYGON_AMOY_CHAIN_ID`) |
| Deployer | a throwaway TESTNET key (address `0xA7bB…59b1`) — see "The env contract" below |

## The G7 gas-abstraction deployments (2026-07-04)

| What | Value |
|---|---|
| `NimmeshForwarder` | [`0x94618C9429BA431d69dA1762b5ABd3AaaA0267e1`](https://amoy.polygonscan.com/address/0x94618C9429BA431d69dA1762b5ABd3AaaA0267e1) ([deploy tx](https://amoy.polygonscan.com/tx/0xe07c30e0cd677d1ed5d89b30fce40879facc46861f765a2fa9eba1ef3a575648)) |
| `NimmeshHtlc` v2 (forwarder-bound) | [`0xb3B3703E07AC897B7E3e864C113a2Fa547D76736`](https://amoy.polygonscan.com/address/0xb3B3703E07AC897B7E3e864C113a2Fa547D76736) ([deploy tx](https://amoy.polygonscan.com/tx/0x82a0861df0e71c03c06bcad7a5af171f701f270ff5a63dd5752dedb40ea16a99)) — same USDC, `trustedForwarder()` = the forwarder (the v1 HTLC's forwarder is immutable `0x0`, so G7 deployed fresh; both stay live, the app targets v2) |

**Gasless proof (`live_amoy_gasless_swap`, 2026-07-04, exit 0):** a user derived in-process with
**zero POL and account nonce 0** funded a real escrow and settled it — the relayer paid every
drop of gas: [stake transfer](https://amoy.polygonscan.com/tx/0xdfff227b5ca39a9d4343e5252e480009c4a41db2e3390cfa7c80e60b06668788) →
user signs permit + `ForwardRequest` (0 txs) → [forwarder `execute`](https://amoy.polygonscan.com/tx/0x537eb61d0b101f31fa310c7671327ccd185161108b8d24aedf8c3bd91742c16b)
→ `getSwap`: Live, **funder == the user** (ERC-2771 attribution) → [caller-open `withdraw(S)` by the relayer](https://amoy.polygonscan.com/tx/0xe61122f7ec24df227e461401bbf97ddddb24fbc52b21ac7630527b8ad63c8e4e)
→ Claimed → **user nonce still 0, POL still 0**. Run:
`cargo run --example live_amoy_gasless_swap --features polygon-gateway` (env additionally:
`AMOY_FORWARDER_ADDRESS`, `AMOY_HTLC2_ADDRESS`). Design: ADR-0008.

## The live round-trip

```
cargo run --example live_amoy_usdc_htlc --features polygon-gateway
```

drives the deployed contract with OUR core for every byte — `evm_abi` calldata,
`evm_rlp` EIP-155 legacy tx, `evm_signer` k256 signing, `polygon_gateway` JSON-RPC — and proves,
on the real chain: the swap-id byte-match (`swapIdFor` == `usdc_swap_id`), escrow → `Live` with
exact terms, `withdraw(S)` under the **SHA-256 precompile** → `Claimed`, premature `refund`
reverts, post-timelock `refund` → `Refunded`, and the USDC balance round-trips exactly.

## Proven on-chain (2026-07-04, run of the example above)

Every byte from our own stack; gas 30 gwei (clamped), starting nonce 5:

- swap-id byte-match vs the live contract: `0x9f1ada06ee547072d7dd441af922abdcd18fb85577b8c3adda1242f542e4e509`
- claim path (1 USDC, timelock +1h): [approve](https://amoy.polygonscan.com/tx/0x33553ec66969e1b5d9fc19dcb4bf92dbc41c9043cc6044d620a8969f6cb8a4f6) → [newSwap](https://amoy.polygonscan.com/tx/0x9f5cd8e63f7c9b528b6540206110c32afa42d6558e9f42e79bb66946231f8e46) → `getSwap` Live, terms match → [withdraw(S)](https://amoy.polygonscan.com/tx/0xc7538a9022cd5800ae86acca358332f48b4c933d69a9662732c7aaa96c1c1951) (SHA-256 precompile) → **Claimed**
- refund path (0.5 USDC, timelock +75s): [approve](https://amoy.polygonscan.com/tx/0x85898a28ca1c66e18d77e8ee9eed407606a02c199a0e1b2792b78c522f0f7d25) → [newSwap](https://amoy.polygonscan.com/tx/0x53bb94574ccd65210fadf4573e6e8cc21dcc129b384218d90e91d7521f294a29) → [premature refund **REVERTED**](https://amoy.polygonscan.com/tx/0xe78278ce6fc3ff216201b4c9e1ac4be44da74ba5c7124200c02d98300d7a3f8d) (`TimeoutNotReached` — the boundary second belongs to the claimer) → [post-timelock refund](https://amoy.polygonscan.com/tx/0xfb1ac8a46c83f8210d4a7b57413d8d8464f4f03dfc464898a389bd649c9f3f91) → **Refunded**
- USDC balance round-tripped exactly: 20.000000 → 20.000000

## The env contract

The example reads (never commits) these variables — on the operations box they live in
`~/.nimmesh-amoy.env` (mode 0600, testnet-only):

- `AMOY_TEST_KEY` — 32-byte hex private key. **Throwaway, testnet-only, never reused anywhere.**
- `AMOY_HTLC_ADDRESS` — the deployed contract (table above).
- `AMOY_RPC_URL` — optional; defaults to the public `https://rpc-amoy.polygon.technology`.
- `AMOY_USDC_ADDRESS` — optional; defaults to Circle's Amoy USDC.

Funding: POL at <https://faucet.polygon.technology> (network Amoy), USDC at
<https://faucet.circle.com> (Polygon PoS Amoy). A full run needs ~0.04 POL headroom (the preflight budget at 30 gwei) and 1.5 USDC.

## Operations notes (learned on the real chain)

- **Amoy fee spikes are real**: a run saw `eth_gasPrice` suggest **84 gwei** against the ~25 gwei
  enforced floor and burned 3× the expected POL before dying mid-flow. The example clamps the
  suggestion to 50 gwei (floor 30) and **preflights the entire gas budget** via
  `polygon_gateway::get_balance` before spending anything.
- **An interrupted run cannot strand funds**: swaps are self-swaps (funder = recipient), so any
  `Live` leftover is claimable with the run's secret or simply refundable after its timelock —
  both payouts land back on the test key. (Both paths have been exercised for real.)
- The node's upfront balance check is `gas_limit × gas_price` per tx — keep the example's limit
  constants tight or a healthy balance still bounces.

## What stays gated

Everything mainnet (G8 review first), any non-throwaway key, and any real funds — see
`docs/swap/OWNER-GATED.md`. The relayer/forwarder work (G7 #78 implementation) builds on this
deployment per ADR-0006.
