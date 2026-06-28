# SwapEngine — the cross-chain swap as one API

`swap_engine::SwapEngine` folds the `live_cross_chain_swap` orchestration into a **first-class, pure
(no-network) component** that the UI and the mesh both drive. It is the contract the swap UI binds to.

## What it ties together

```
swap::Swap            (state machine: rules → SwapAction, chain-agnostic)
swap_builder::NimiqLeg (NIM HTLC tx bytes, signs via EnclaveKey)
swap_btc_leg::BtcSwapLeg (BTC HTLC tx bytes, signs via BtcEnclaveKey)
        │
        ▼
   SwapEngine  ──►  SwapEffect  (Broadcast{leg, tx}  |  FundBtcAddress{address, amount_sat})
```

The engine owns the `Swap` state machine and this node's two legs, and turns each `SwapAction` into a
concrete `SwapEffect`. **The engine does no I/O** — the caller broadcasts / watches and feeds
observations back, so the same engine works under a CLI, the WebView UI, or the BLE mesh.

## The per-node API

Construct `new_initiator(config, secret)` or `new_responder(config)`, then drive:

| step | initiator | responder |
| --- | --- | --- |
| `accept(head, ladder)` | gate the timelock ladder | gate the timelock ladder |
| `observe_initiator_funded(nim_wire)` | — | record the NIM funding tx |
| `fund(head, ladder, vsh)` | → `Broadcast{Nim, tx}` | → `FundBtcAddress{addr, sat}` |
| `observe_btc_funded(outpoint)` | record the BTC outpoint → `BothFunded` | — |
| `observe_nim_funded(own_btc_outpoint)` | — | → `BothFunded` |
| `reveal_and_claim_btc()` | → `Broadcast{Counterparty, claim}` (reveals S) | — |
| `claim_nim_from_btc_claim(btc_claim, vsh)` | — | reads S off-chain → `Broadcast{Nim, claim}` |
| `observe_settled()` | → `Settled` | → `Settled` |
| `refund(head, vsh)` | timeout-refund the NIM leg | timeout-refund the BTC leg |

## Atomicity stays real

The responder is **never told** the secret. It reads `S` off the initiator's on-chain BTC claim
(`btc::extract_preimage`, witness item [1]) and verifies `SHA-256(S) == H` before claiming the NIM
leg — a forged preimage is rejected (`EngineError::BadPreimage`). Every funded phase keeps a timeout
`refund` exit, so the worst case is a refund, never a theft.

## The one cross-chain bridge: units

Everything the ladder touches — `SwapTerms`, `LadderParams`, `head` — is in **Unix-milliseconds**
(the NIM validator compares the HTLC timeout to the block *time* in ms; the head-beacon is the mesh
clock). The BTC leg's CLTV is **Unix-seconds** = `T_B_ms / 1000`, baked into the `BtcSwapLeg` at
construction. The engine itself does no conversion — it wires the proven pieces together.

## Proof

`swap_engine` tests drive **two real engines** (initiator + responder, in-memory keys) through a full
swap to `Settled`, asserting: both sides derive the same P2WSH; the BTC claim reveals exactly `S`; the
responder reads `S` off that claim and claims the NIM leg with it; the NIM funding/claim are the
248-byte / RegularTransfer-framed bytes the live testnet already accepted; a forged preimage is
rejected; and both legs keep a timeout-refund exit. The bytes the legs emit are the same ones
byte-validated vs `@nimiq/core` + `bitcoinjs-lib` and live-confirmed on both chains.

> The `live_cross_chain_swap` example remains the **live network harness** (faucet + broadcast +
> watch). The engine is the reusable core it (and the UI / mesh) orchestrate through.

## FFI surface (`swap_ffi`) — how the UI/mesh call it

`swap_ffi::SwapEngineHandle` is a `#[uniffi::export]` object (like `nimiq::signer::AppSigner`) that
wraps the engine behind a `Mutex` so the WebView UI and the native mesh shim can drive a swap. The
pure `SwapEngine` stays uniffi-free; the facade handles interior mutability + the bitcoin-type↔FFI
conversions (txids/addresses as strings, network as an enum, `EngineError` flattened to
`SwapEngineError`). The two keys cross as the same `with_foreign` traits the signer uses
(`EnclaveKey` / `BtcEnclaveKey`) — **seeds never cross FFI**, the native enclave signs.

Verified: `uniffi-bindgen generate --library <dylib built with --features bitcoin-leg>` emits valid
**Swift + Kotlin** with `class SwapEngineHandle` (`newInitiator`/`newResponder`/`fund`/
`revealAndClaimBtc`/`claimNimFromBtcClaim`/`refund`/…), the `FfiSwapPhase`/`FfiSwapEffect`/
`FfiSwapParams` types, and `protocol BtcEnclaveKey` for the native secp256k1 enclave to implement.

**Packaging caveat:** because the facade is behind `bitcoin-leg`, the binding/xcframework build must
pass `--features bitcoin-leg` for `SwapEngineHandle` to appear in the generated Swift/Kotlin
(consistent with the rest of the BTC leg). The default (feature-off) build is unaffected.

## A typical UI flow (initiator)

```
let h = SwapEngineHandle.newInitiator(params, nimKey, btcKey, secret)  // nimKey/btcKey = native enclaves
try h.accept(headMs, ladder)
let eff = try h.fund(headMs, ladder, vsh)        // → .broadcast(.nim, tx)  → app broadcasts it
// … app watches the BTC HTLC address (h.btcHtlcAddress()) for funding …
try h.observeBtcFunded(txid, vout, valueSat)
let claim = try h.revealAndClaimBtc()            // → .broadcast(.counterparty, tx) → app broadcasts
try h.observeSettled()
```

## Relationship to the mesh node integration (G8–G27)

`SwapEngine` is the **single-node, real-legs** view: it owns one node's `Swap` + `NimiqLeg` +
`BtcSwapLeg` and turns each `SwapAction` into a `SwapEffect` (tx bytes to broadcast), behind the
`bitcoin-leg` feature, for the on-device app to drive over the internet at the gateway.

A separate, default-build stack drives a swap over the **BLE flood loop** between two phones:
`swap_coordinator::SwapCoordinator` (the protocol brain) → `swap_session::SwapSession` (the
per-`swap_id` router a `MeshNode` runs) → the `swap_node` hook + driver → the
`swap_signer::SwapSigner` seam. That seam is exactly where this engine's leg builders belong: the
node driver asks its `SwapSigner` for each funding/claim `tx_wire`, and today a `MockSigner` returns
sim stand-ins — the real NIM/BTC tx-building those legs do is the money-path signer that drops in
there (gated). So `SwapEngine` = how a node builds + settles its legs; the node stack = how two nodes
negotiate, transport, and drive that swap over the mesh. Full map: [MESH-INTEGRATION.md](./MESH-INTEGRATION.md).
