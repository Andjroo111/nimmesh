# Run the swap demo (real engine, sim chain)

The swap UI driven by the **actual `SwapEngine`** — a full initiator+responder NIM⇄BTC atomic swap
against an in-memory simulated chain. No real funds, no faucets, no native bridge.

```sh
# from the repo root
cargo run --example swap_demo_server --features bitcoin-leg
# → open http://127.0.0.1:8090/swap/swap.html?engine=1
```

Click **Confirm → Confirm & sign** to start the engine, then **›** (or →) to advance each real engine
action. The step counter, the gold/orange HTLC locks, and the tx ids all come from the live engine
(`GET /api/state` to see the raw snapshot). Env: `PORT` (8090), `NIMMESH_WEBUI` (`webui`).

## What's real vs simulated

- **Real:** the `SwapEngine` state machine + Δ_safe ladder, the NIM HTLC funding/claim tx bytes, the
  BTC HTLC redeem script + claim, the preimage reveal/extraction — the same bytes validated vs
  `@nimiq/core` + `bitcoinjs-lib` and live-confirmed on testnet.
- **Simulated:** the chains. Funding is instantly "confirmed" in memory; there is no network, no
  broadcast, no funds. (`crates/nimmesh-core/src/swap_sim.rs`.)

## Offline / mock mode

Without `?engine=1` the page runs a self-contained mock phase machine (`?phase=N` + arrow keys) — no
server needed. Same screens; the data is canned instead of engine-driven.

> The production path is the native WebView↔Rust bridge (UniFFI `SwapEngineHandle`). This local
> server is the **browser-demo** transport — wasm isn't buildable here (no wasm clang; `uniffi_core`
> doesn't target wasm). See `docs/swap/DEMO-LOOP.md`.
