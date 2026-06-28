# Mesh-swap test wallets + how to re-run a live cross-chain swap

Two **persistent testnet wallets** back every live example, so swapped funds land in known wallets
and **cycle back** instead of stranding. Both are play-money.

| | address | how it's funded |
| --- | --- | --- |
| **NIM** (Nimiq testnet) | `NQ92 VGEX VYH9 KHP0 Y00L DAQM 32N2 8H12 H9F7` | auto — the testnet faucet (`tapit`) tops it up each run |
| **BTC** (testnet3 / signet) | `tb1q4n9al5rnhtfgpg4sd5qlpayc77qkf8hs026cjj` | **fund once** via a faucet; the swap recycles it from then on |

> **Seeds** live in `~/secrets/nimmesh-swap-wallets.env` (chmod 600, off the repo) as
> `NIMMESH_NIM_SEED` / `NIMMESH_BTC_SEED`. The examples read them via env. Never commit the seeds.

## The earlier test funds are stranded (lesson)

The first `live_cross_chain_swap` run used **fresh random keys that were not saved**, so its swapped
output (≈113,799 sat at `tb1qtz58ty…`, 2 NIM at `NQ57 RMAF…`) is unspendable. These persistent
wallets fix that: the orchestrator now derives the swap keys from the saved seeds, so the BTC claim
returns to `tb1q4n9al…` and the NIM claim returns to the NIM wallet — reusable forever.

## Re-run a live NIM⇄BTC swap (no faucet after the first BTC top-up)

```bash
cd ~/projects/nimiq.nimmesh-htlc
set -a; . ~/secrets/nimmesh-swap-wallets.env; set +a   # load the wallet seeds

# 1) start the swap — NIM leg auto-funds + funds the NIM HTLC, then it prints the BTC HTLC address
NIMMESH_BTC_NETWORK=testnet NIMMESH_BTC_API=https://mempool.space/testnet/api \
  cargo run --example live_cross_chain_swap --features "gateway-rpc bitcoin-gateway"

# 2) in another shell, fund that BTC HTLC FROM the treasury wallet (no faucet):
NIMMESH_BTC_SEED=$NIMMESH_BTC_SEED \
  bun run scripts/fixtures/fund-htlc-from-treasury.mjs <the-tb1q…-HTLC-address> 50000
```

The orchestrator detects the funding, **Alice claims the BTC (revealing `S`)**, **Bob claims the NIM
with that same `S`**, and both wallets end up holding their swapped asset — verifiable on
`nimiq-testnet.observer` and `mempool.space/testnet`. Funds return to the persistent wallets (minus
a few sats of fees), ready for the next run.

**First-time only:** fund `tb1q4n9al5rnhtfgpg4sd5qlpayc77qkf8hs026cjj` once from any testnet3 faucet
(e.g. https://coinfaucet.eu/en/btc-testnet/ or https://bitcoinfaucet.uo1.net). After that it's
self-sustaining until the dust runs out.
