# ADR-0006 — USDC gas abstraction: relayer-sponsored claims (EIP-2771) with a mandatory self-funded fallback

**Status:** accepted (2026-07-02), **revisitable** — this fixes the *model* so the G5 contract (#76) can be shaped for it; the decision re-opens if Amoy integration (**G6, #77**) contradicts its assumptions (relayer economics, forwarder ergonomics, real gas numbers). **Implementation is gated on G6 — nothing in this ADR is wired up.** · **Context:** closes the decision half of **G7 (#78)** / finding **S4**. A NIM⇄USDC swap settles the USDC leg with real Polygon transactions (`newSwap`, `withdraw`, `refund`), each costing gas in the chain's native coin (MATIC/POL) — but the swap user holds USDC, not MATIC. The NIM and BTC legs pay fees in the swapped asset itself; USDC is the odd one out because the fee currency and the swapped currency differ. The four candidate models are catalogued in [`docs/swap/USDC-GAS.md`](../swap/USDC-GAS.md): user-holds-MATIC, relayer + EIP-2771 meta-tx, ERC-4337 paymaster, counterparty-sponsored.

## Decision

**Primary path: relayer-sponsored meta-transactions (EIP-2771).** The USDC recipient signs the claim (the `withdraw` intent, carrying the revealed secret `S`); a **relayer** submits it through a trusted forwarder and pays the gas; the HTLC contract recovers the real signer via `_msgSender()` (ERC-2771). The recipient never needs to hold MATIC. The relayer is reimbursed out of band — a small USDC fee priced into the swap terms, or the relayer is simply run/paid by the party motivated to see the USDC move (Option 4 of `USDC-GAS.md` folds in here as a deployment question of *who operates* the relayer, not a distinct mechanism).

**Mandatory fallback: self-funded claim.** The contract's plain `withdraw(S)` / `refund()` entry points stay first-class and forwarder-independent: any party holding a little MATIC can always settle directly (a `withdraw` is tens of thousands of gas — a fraction of a cent). The fallback is not a convenience tier; it is the security floor that bounds every relayer failure mode below.

## Trust and griefing surface

- **A relayer cannot steal.** The contract checks `_msgSender()` — it acts only on a claim the recipient actually signed — and an HTLC `withdraw` pays the swap's designated recipient regardless of who submits it. A forged or replayed meta-tx moves nothing.
- **A relayer can grief/censor** — drop, delay, or reorder the meta-tx. Two properties keep that at inconvenience rather than loss:
  1. the **self-funded fallback**: the recipient (or anyone acting for them) submits the plain `withdraw(S)` the moment the relayer looks unresponsive;
  2. the **reveal-deadline guard** (ADR-0004): the engine refuses to publish `S` at all when the remaining claim window is too thin, so a censored claim degrades to *self-fund or refund*, never to a burned secret with no time left.
- **The sharp edge: handing `S` to a relayer IS a reveal.** The signed meta-tx contains `S`. A malicious relayer that learns `S`, feeds it to the counterparty (who claims the NIM leg with it), and censors the USDC claim past `T_B` would convert griefing into theft *if the recipient could not get a claim on-chain in time*. The implementation must therefore treat "meta-tx handed to relayer" exactly like "S published": start the claim clock, watch for on-chain inclusion, and **fail over to the self-funded path with margin still inside the claim window** (a fixed inclusion timeout, comfortably under `min_claim_window_blocks`). This is why the fallback is mandatory, not optional.

## Why not the alternatives

- **User-holds-MATIC only** (Option 1): zero infrastructure, but the wrong onboarding for this product — a NIM holder who wants USDC would need a second asset just to receive the first. Kept as the fallback, rejected as the only path.
- **ERC-4337 paymaster** (Option 3): the most flexible and the most infrastructure (bundler, funded paymaster contract, a smart account per user). Overkill for a first version; nothing in the EIP-2771 choice forecloses moving to it at scale — the forwarder-aware contract is compatible with both.
- **Counterparty-sponsored** (Option 4): folds into the relayer model (above), as `USDC-GAS.md` already notes.

## What this implies for G5/G6 (recorded here, implemented there)

- The **G5 Solidity HTLC (#76)** should be ERC-2771-aware: extend `ERC2771Context` with an immutable, constructor-set trusted forwarder (zero address = disabled → pure self-funded behavior), and consult `_msgSender()` anywhere the caller matters.
- **Open questions that ride G6 (#77, Amoy):** whether `withdraw(S)` is caller-restricted to the recipient (the relayer then submits on the recipient's signature) or caller-open with a fixed payout (any gas-paying submitter can land it, and the forwarder machinery matters mainly for caller-restricted paths); the concrete forwarder (e.g. OpenZeppelin `ERC2771Forwarder`); the fee mechanic; the inclusion-timeout constant for the failover rule above.
- **Mainnet stays gated regardless** (G8 independent review + Andjroo), as does running any real relayer service.
