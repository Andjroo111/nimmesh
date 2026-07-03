# contracts — the USDC HTLC (G5, #76)

The Solidity side of the NIM⇄USDC swap leg: `NimmeshHtlc.sol` escrows USDC under a **SHA-256**
hashlock (the `0x02` precompile — the same `H = SHA-256(S)` the NIM and BTC legs use, so one
secret unlocks all three legs). The Rust model it must stay byte-identical to lives in
`crates/nimmesh-core/src/swap_usdc_leg.rs`; the calldata the app builds for it lives in
`crates/nimmesh-core/src/evm_abi.rs`. Design decisions: ADR-0006 (gas abstraction) and
ADR-0007 (contract shape).

## Build + test

Needs [Foundry](https://getfoundry.sh) (`curl -L https://foundry.paradigm.xyz | bash && foundryup`):

```
cd contracts
forge test -vv
```

Self-contained on purpose — no forge-std / OpenZeppelin submodules; the tests vendor the
cheatcode slice they use (`test/Cheats.sol`) and a permit-capable mock USDC.

## The invariants CI proves

- **Swap-id byte-match**: `swapIdFor` == the Rust `usdc_swap_id` on the shared published vector
  (`0x81137ded…31e0`) — G5's "done when".
- **SHA-256, not keccak**: a keccak-locked slot is NOT claimable by the same secret.
- **Boundary**: claim while `block.timestamp <= timelock`, refund strictly after — no overlap,
  no gap (matches the Rust model's code).
- **Single occupancy**: identical parameters = same slot, rejected while live; resolved slots
  are never reused (future-timelock rule makes identical re-creation impossible).
- **Caller-open, fixed payouts**: anyone may submit `withdraw`/`refund`; funds only ever move
  to the stored receiver/funder (ADR-0006's self-funded fallback).
- **Single-tx funding**: `newSwapWithPermit` (EIP-2612) with front-run-tolerant try/catch.
- **ERC-2771**: funder attribution through the immutable trusted forwarder; suffix from anyone
  else is ignored.

## What is deliberately NOT here

Deployment (Amoy) and real-RPC integration tests are **G6 (#77)** — they need an RPC URL and a
funded testnet key (owner-gated). Mainnet is gated behind G8 review. See
`docs/swap/OWNER-GATED.md`.
