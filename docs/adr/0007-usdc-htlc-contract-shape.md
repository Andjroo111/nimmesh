# ADR-0007 — The USDC HTLC contract's shape: model-faithful, caller-open, single-tx fundable

**Status:** accepted (2026-07-02) · **Context:** implements **G5 (#76)** — the real Solidity
contract (`contracts/src/NimmeshHtlc.sol`) for the escrow that `swap_usdc_leg.rs` has modeled in
sim since P2. The function surface was already frozen by the Rust side (`evm_abi.rs` builds
`newSwap(address,uint256,bytes32,uint256)` / `withdraw(bytes32,bytes32)` / `refund(bytes32)`
calldata; `usdc_swap_id` derives the slot as `keccak256(abi.encodePacked(sender, receiver,
amount, hashlock, timelock))`; the hashlock is SHA-256 so one secret spans NIM/BTC/USDC). What
was still open were the semantic edges. This ADR records them.

## Decisions

1. **Boundary semantics follow the Rust model's CODE, not its doc-comment.** `withdraw` while
   `block.timestamp <= timelock`; `refund` strictly after (`>`). The model (`UsdcLeg::claim`/
   `refund`) always behaved this way; its module doc-comment sketch said `<`/`>=` — the stale
   comment is fixed in this same PR. The boundary second belongs to the claimer; there is no
   overlap and no gap, which is the property the engine's ladder math actually needs.

2. **Slots are single-use forever.** A resolved (`Claimed`/`Refunded`) slot is never deleted or
   reused. `newSwap` requires `timelock > block.timestamp`, so re-creating a resolved swap with
   identical parameters is impossible anyway (its timelock is past). Keeping resolved state in
   storage costs a little gas but gives the G6 gateway verifier a readable outcome (`getSwap`),
   which the funding-verification seam (G1) wants.

3. **`withdraw`/`refund` are caller-open with fixed payouts.** Anyone may submit; the USDC only
   ever moves to the stored `receiver` (claim) or `sender` (refund). This RESOLVES the question
   ADR-0006 left open: the claim path needs **no** forwarder machinery at all — any gas-paying
   party (a relayer, the counterparty, a friend) can land the recipient's claim, and a
   "front-runner" simply pays the recipient's gas. ERC-2771 stays where the caller's identity
   actually matters: `newSwap*`, where `_msgSender()` is the funder whose tokens are pulled and
   whose address binds the swap id. The forwarder is immutable, set at deployment, and
   `address(0)` disables it.

4. **Single-transaction funding via EIP-2612 `permit`, with the plain path kept byte-compatible.**
   `newSwapWithPermit` closes S4's approve→transferFrom race (one tx, no standing allowance);
   the `permit` call is try/catch-wrapped so a front-run permit (someone spending the extracted
   signature first) degrades to a no-op instead of a DoS. The plain `newSwap` remains exactly
   the selector + layout `evm_abi::htlc_new_swap` already emits, so the Rust builders need no
   change until G6 wants a permit builder.

5. **No dependency framework.** No OpenZeppelin, no forge-std submodule: the ERC-20/permit
   interfaces, the ~10-line ERC-2771 `_msgSender()`, and the cheatcode slice the tests use are
   vendored by hand — the same no-framework rule `evm_abi.rs` documents. The contract is written
   for USDC's actual semantics (bool-returning transfers); it does not try to be a generic
   any-token HTLC.

## Proof

`contracts/test/NimmeshHtlc.t.sol` (Foundry) covers escrow/claim/refund happy paths, both
boundary seconds, wrong-secret and keccak-lock rejection (the cross-chain SHA-256 proof,
mirroring the Rust suite), duplicate-slot rejection, double-resolve rejection, permit single-tx
+ front-run tolerance, and forwarder attribution/impersonation-rejection. The byte-match anchor
is the published vector `0x81137ded176c774f8dbc1b69583fa8232031e4a2810ba97231a69becf44131e0`
(sender `0x11…11`, receiver `0x22…22`, amount 25_000_000, hashlock `0xc7…c7`, timelock 5000),
asserted by BOTH `test_SwapIdMatchesTheRustVector` (Solidity) and
`usdc_swap_id_matches_the_contract_vector` (Rust) — if either implementation drifts, its CI
fails. A `contracts (solidity)` CI job (pinned Foundry) runs `forge fmt --check` + `forge test`.

## What stays gated

Deployment to Amoy + real-RPC integration tests = **G6 (#77)**: needs an RPC URL and a funded
testnet key (owner-gated, never invented). The gas-abstraction implementation (relayer service,
forwarder deployment, fee mechanics) rides G6 per ADR-0006. Mainnet = G8 review + Andjroo.
