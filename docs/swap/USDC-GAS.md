# USDC on Polygon: the gas problem

A NIM⇄USDC swap settles the USDC leg with real Polygon transactions (`newSwap`, `withdraw`,
`refund`). Every one of those costs **gas, paid in MATIC**, not in USDC. So a user who only holds
USDC cannot, on their own, claim the USDC leg: they have the token but not the native coin the chain
charges for the transaction. The NIM side has no such issue (the NIM HTLC pays its fee in NIM, which
the swapper already holds), and Bitcoin pays its fee in BTC out of the swapped amount. USDC is the
odd one out because the fee currency and the swapped currency differ.

This note records the options. None of them are wired up: the swap leg builds and signs the calldata
(P3/P4) and can submit it (P8), but who pays the gas is a product + funds decision, so the live paths
are owner gated.

## Option 1: the claimer holds a little MATIC

The simplest path. The USDC recipient keeps a small MATIC balance and pays its own gas. A Polygon
`withdraw` is cheap (tens of thousands of gas, a fraction of a cent at Amoy/Polygon gas prices), so a
one time top up covers many swaps. The downside is onboarding friction: a NIM holder who wants USDC
now needs a second asset just to receive the first. Fine for power users, poor for newcomers.

## Option 2: a relayer / meta transaction (EIP-2771)

The recipient signs the intent to withdraw, but a **relayer** submits the actual transaction and pays
the MATIC. The HTLC contract trusts a forwarder (EIP-2771 `_msgSender()`), so the relayer cannot
steal: it can only relay a withdrawal the recipient already authorized with the secret. The relayer is
reimbursed out of band (a small USDC fee skimmed at settlement, or a subscription). This keeps the
recipient MATIC free. It needs a relayer service and a contract that supports trusted forwarders.

## Option 3: an ERC-4337 paymaster

Account abstraction: the recipient operates a smart account, and a **paymaster** sponsors the gas
(optionally charging the user in USDC via the paymaster's own logic). This is the most flexible and
the most infrastructure heavy: a bundler, a paymaster contract with a deposit, and a smart account
per user. Overkill for a first version, the right answer at scale.

## Option 4: the counterparty sponsors the claim

Because the swap already has two parties, the NIM giver (the one who wants USDC) can be the one who
pays the USDC leg's gas, since it is the party motivated to see the USDC move. In practice this folds
into Option 2 (the NIM giver runs, or pays, the relayer). It needs the swap terms to price the gas in
so neither side is surprised.

## What is gated

- Building + signing the `withdraw`/`newSwap`/`refund` calldata: **done in sim** (P3/P4), validated
  against published vectors. No funds.
- Submitting a real transaction (even on Amoy testnet) with a funded key: **owner gated**
  (needs:owner) — it spends real testnet MATIC and moves real testnet USDC.
- Choosing + deploying a relayer / paymaster, and any mainnet path: **owner gated** (real funds, a
  deployed contract, a service to run).

The recommended first step when the owner is ready: Option 1 on Amoy (a self funded test key), then
Option 2 (a relayer) for a real product, before any mainnet consideration.
