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

## Open intents view (G38, discovery layer)

`swap/intents.html` is a read-only "open intents seen on the mesh" view: a list of the swap
advertisements (`SwapIntent`, G34) peers near you have flooded, with each one's give/take amounts,
rate, and freshness (Fresh vs Expired, the G35 expiry rule made visible against the chain head).
Built on the real vendored nimiq-ui (legacy `@nimiq/style`, `@nimiq/iqons` identicons) and the swap
demo's own tokens; passes `nq lint` (0 errors).

```sh
cargo run --example swap_demo_server --features bitcoin-leg
# → open http://127.0.0.1:8090/swap/intents.html
```

Below the list it also shows a **discovery-stats strip** (G46): the node's G42 `IntentMetrics` —
intents `seen` / `matched` / `re-advertised`, and dropped-by-reason (`expired` G35 / `rate` G40 /
`forged` G41 / `throttled` G36) — so the otherwise-invisible gate activity is legible at a glance,
topped by a one-line **health summary** (G57: status + match rate, e.g. `Healthy · 23% match rate`)
derived from those same counts via `swap_health::discovery_health` and served at `GET /api/health`.

- **Now:** the rows AND the stats are **fixture data**. Opened as a static file the page renders its
  inline fixtures; opened behind the server (above) the page's `loadIntents`/`loadStats` seam upgrades
  them LIVE from `GET /api/intents` + `GET /api/stats` (G54), which return the fixtures as JSON
  (`demo_http::intents_fixture_json` / `stats_fixture_json`). Either way the visible data is identical;
  the fetch just proves the data-driven seam end to end. A fetch failure falls back to the inline
  fixtures, so `nq lint` / `file://` render unchanged.
- **BLOCKED (human-gated):** the truly-live wiring — having `/api/intents` + `/api/stats` (or the
  native bridge) return the node's REAL `SwapSession` intents + `IntentMetrics` instead of fixtures —
  needs the native WebView↔Rust bridge (OG-1 in `docs/swap/OWNER-GATED.md`).
