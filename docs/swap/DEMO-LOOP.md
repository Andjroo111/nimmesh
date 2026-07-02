# Swap demo loop — the web UI driven by the REAL engine (contract)

## Goal

Make `webui/swap/` run a **genuine NIM⇄BTC atomic swap powered by the actual `SwapEngine`** —
replacing the mock phase machine with the real Rust engine's state + transitions. A full
**initiator + responder** swap runs step-by-step against an **in-memory simulated chain** (no real
funds, no faucets, no native bridge). The UI's phases, locks, and tx hashes come from the real engine.

### Why a local server (not wasm)

Wasm was the first plan; this environment blocks it: there is **no wasm-capable clang** (the C
`secp256k1` behind `rust-bitcoin` can't compile to wasm32) **and `uniffi_core` doesn't compile to
wasm**. So the engine runs **natively** in a tiny local server, and the web UI calls it over `fetch`.
Same result — the real engine drives the UI — without the toolchain wall. (Production still targets
the native WebView↔Rust bridge; this is the browser-demo path.)

## Milestones (the loop)

| # | Milestone | Gate |
| --- | --- | --- |
| **D1** | `swap_sim`: an in-memory sim chain + dual-engine stepper. Holds an initiator + responder `SwapEngine`, simulates funding confirmation + relays the preimage off the BTC claim, and advances one real engine action per `step()`. | unit test drives a full swap to **Settled** through the real engines + sim |
| **D2** | `examples/swap_demo_server.rs`: a std-only HTTP server over `SwapSim` (no new deps) that also serves `webui/`. Endpoints: start / state / advance / reset. Real engine state + tx hashes in the JSON. | `cargo build --example swap_demo_server --features bitcoin-leg` green; manual curl drives a swap |
| **D3** | Wire `webui/swap/swap.html`: the phase machine calls the server (`?engine=1`), reflecting **real** engine phase/locks; keeps the mock (`?phase=N`) as the offline fallback. | the UI runs a full swap driven by the server |
| **D4** | Screenshot-verify the real-engine-driven flow matches the reference screens; the step counter + puzzle-piece locks are driven by the actual `SwapEngine`. | screenshots match; `nq lint` 0 errors |
| **D5** | Polish + a short `RUN-DEMO.md` + commit. | green; committed to `feat/mesh-swap` |

## Guardrails

- **Sim / testnet only. No real funds, no faucets, no mainnet.** The money-path stays gated.
- Reuse the proven pieces: the `swap_engine` e2e drive is already the dual-engine orchestration —
  `swap_sim` extracts it into a stepper. The legs build the same bytes validated vs `@nimiq/core` +
  `bitcoinjs-lib` and live-confirmed.
- Presentation/demo only — no change to the engine's money-path or the gated mainnet logic.
- Auto-commit milestones to `feat/mesh-swap` when green (non-money-path); never touch `main`.
