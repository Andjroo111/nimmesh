# Owner-gated backlog (swap / discovery)

Everything the autonomous swap loop deliberately did NOT build because it needs a human decision —
money-path, real funds, mainnet, native-platform integration, or an architecture call. The loop ships
the sim/testnet half behind a clean seam and records the gated half here, so the human-decision
backlog is one findable list instead of being scattered across commits, agenda notes, and per-doc
asides. Each item names the EXACT code seam the gated work plugs into.

| id | item | why gated | source |
|---|---|---|---|
| OG-1 | Live WebView↔Rust bridge for the demos | native-bridge | G38, G46, DEMO-LOOP.md |
| OG-2 | Real NIM/BTC money-path signer (replace `MockSigner`) | money-path / real-funds | G14–G33, BTC-LEG.md |
| OG-3 | Real swap secret `S` from a CSPRNG (replace `sim_secret`) | money-path | G34 |
| OG-4 | Mainnet / real funds / live broadcast | mainnet / real-funds | whole money-path |
| OG-5 | Deeper discovery privacy (commit-reveal addressing, amount mixing) | money-path + match-handshake | G45 |

---

## OG-1 — Live WebView↔Rust bridge for the demos

**State:** the swap UI (`webui/swap/swap.html`) runs against the real `SwapEngine` over a tiny local
HTTP server (`examples/swap_demo_server.rs`, the **browser-demo transport**) because wasm isn't
buildable here — no wasm-capable clang for `secp256k1`, and `uniffi_core` doesn't target wasm
(`DEMO-LOOP.md`). The G38/G46 intents view (`webui/swap/intents.html`) renders **fixtures**.

**Seam to wire:** the inline module in `webui/swap/intents.html` marks a `loadIntents` + `loadStats`
seam; production replaces the canned arrays with the node's live discovery data (the `SwapSession`
intents + the G42 `IntentMetrics` counters) streamed across the **native WebView↔Rust bridge**. The
Rust side is the UniFFI `SwapEngineHandle` (`crates/nimmesh-core/src/swap_ffi.rs`), already exported to
Swift/Kotlin. Wiring the bridge into the shipping app is a native-platform integration decision.

## OG-2 — Real NIM/BTC money-path signer

**State:** every participant carries the deterministic **`MockSigner`** (`swap_signer.rs`) — fixed-
length filler funding blobs and a claim `tx_wire` that is just the 32-byte secret. No keys, no
broadcast.

**Seam to wire:** the **`SwapSigner` trait** (`crates/nimmesh-core/src/swap_signer.rs`) —
`build_funding(leg, swap_id)` and `build_claim(swap_id, secret)`. The real signer drops in behind this
trait unchanged (the node driver already feeds its `(tx_wire, tx_id)` straight to the coordinator).
The building blocks exist but are gated:
- **NIM leg:** `swap_builder::NimiqLeg` builds a byte-exact signed HTLC funding tx (seed stays behind
  the `EnclaveKey` seam); its claim/refund await the gated NIM resolve proof.
- **BTC leg:** `swap_btc_leg.rs` signs through `crate::btc::BtcEnclaveKey` — testnet/signet only,
  mainnet gated. `swap_builder::BitcoinLeg` is a documented **gated stub** (every method returns
  `LegBuildError::Gated`; the real P2WSH-HTLC signer + a BTC node/funds are `needs:owner`).

**Why gated:** it signs and would (with OG-4) broadcast real-fund transactions.

## OG-3 — Real swap secret `S`

**State:** an intent-initiated swap derives `S` from **`swap_node::sim_secret`**
(`crates/nimmesh-core/src/swap_node.rs`) — a deterministic `"NIMMESH-SIM-INTENT-SECRET-DO-NOT-USE-IN-PROD"`
placeholder so the no-RNG sim is reproducible.

**Seam to wire:** replace `sim_secret(swap_id)` with a CSPRNG draw at initiation. **Why gated:** a
predictable `S` lets an attacker pre-claim the BTC leg — only safe to flip on with the real money-path,
never in the deterministic test harness.

## OG-4 — Mainnet / real funds / live broadcast

**State:** the whole money-path is testnet-only. Leg builders hard-target the Albatross testnet
network id (`swap_builder.rs`), the BTC key seam is testnet/signet (`swap_btc_leg.rs`), and live
broadcast (`sendRawTransaction` via the `gateway`) is never exercised by the loop.

**Seam to wire:** mainnet network ids + a funded gateway/RPC + real-fund signing (OG-2). **Why gated:**
mainnet + real funds is an explicit owner-only (`needs:owner`) decision, possibly with a legal review
for the cross-chain swap.

## OG-5 — Deeper discovery privacy

**State:** G45 SHIPPED the cheap, clearly-safe mitigation — ephemeral per-advertisement keys
(`swap_intent::sign_intent_ephemeral`). `DISCOVERY-PRIVACY.md` documents the residual leaks.

**Seam to wire (gated):** commit-reveal addressing (flood only a commitment to the addresses, reveal
them to the matched counterparty on `Propose`) changes the `SwapIntent` wire (`swap_intent.rs`) AND
the match→propose handshake (`swap_node::handle_intent` → `initiate_from_intent`) AND the money-path
address wiring; amount bucketing/mixing is a further step. **Why gated:** it touches the gated
money-path and the negotiation handshake, so it's an architecture decision, not autonomous work.
