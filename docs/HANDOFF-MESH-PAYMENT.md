# Handoff — finish the offline mesh payment

**Written 2026-07-06. Continues the nimiq.nimmesh mesh work in a fresh chat.**
Worktree: `~/projects/nimiq.nimmesh-ui` (this is MY isolated clone — all app/mac-node work here).
`crates/` is shared with a concurrent **swap** session — treat as coordinate-before-touch (details below).

---

## Where we are (DONE — the mesh itself works)

**v0.51.6 is live and the two-way Bluetooth mesh is CONFIRMED working** between Andjroo's
phone and the Mac mini. On-device the phone shows **"meshed · 1 Peers"**, debug line
`c-link:1 p-link:1 peers:1 node-peers:1`, and it holds steady (no more flapping). This is the
milestone the whole project was built to prove — a real Nimiq BLE mesh on real hardware.

What got it there (all merged):
- **Strong `node` ref** in `BleMeshRadio.swift` (was `weak`, got released → `onPeerConnected` no-op'd).
- **Ref-counted links** (`linkUp`/`linkDown`, `linkCount`/`centralLinked`/`periphLinked`) — a pair
  links twice (central+peripheral) under one peer id; fire connect on first, disconnect on last.
- **Live status polling** — `window.refreshMeshStatus()` on a 3s `setInterval` (header/banner/map
  were set ONCE at boot and froze at "offline·0" while actually connected).
- **Keepalive beacon** every ~15s on BOTH sides (`node.pollBeacon()`) — stops the ~50s iOS BLE
  idle-drop. Phone: `keepalive` bridge in WebHostView + 15s setInterval in index.html. Mac:
  `beatCount % 7` in the 2s timer in `mac-node/main.swift`.
- **macOS Bluetooth permission** — the TCC prompt never fires on the headless mini; fixed by
  manually adding `nimmesh-node.app` via System Settings → Privacy & Security → Bluetooth "+".

Parked, non-blocking:
- **Debug readout** (`ble ▸ auth:ok scan:on …`) still on the Network screen. Remove once
  confidence is high (it's `meshDebug` bridge + `#nw-debug` + `startNwDebug()` in index.html,
  `debugSummary()` in BleMeshRadio.swift, the `meshDebug` case in WebHostView.swift).
- **Map polish = GitHub issue #163** — map hexes are pointy-top/sharp → should be Nimiq
  flat-top rounded brand hexagons; the "you" marker is at an arbitrary NA cell → should sit at
  **the operator's configured home cell** on the 129×52 grid; markers should match the brand hex shape.

---

## The goal for the new chat: SEND A REAL PAYMENT OVER THE MESH

The finish line: **phone in airplane mode → signs a NIM tx → hands it to the Mac over Bluetooth
→ the Mac broadcasts it to the Nimiq chain → it confirms.** Real offline crypto payment.

**Andjroo's question answered:** the Mac mini is NOT the recipient — it's the delivery
truck/gateway. The payment goes to **any recipient address Andjroo picks** (best: a 2nd wallet he
owns, so he can watch the NIM land). The Mac never needs an address; it just carries the signed
tx and puts it on-chain.

### What's already wired (good news)
- **Phone can inject into the mesh:** FFI-exported in `crates/nimmesh-core/generated/nimmesh_core.swift`:
  - `submitSignedTransfer(signedTransfer: SignedTransfer) -> Data` (@2207)
  - `submitLocalTx(txWire: Data) -> Data` (@2190)
- The gateway broadcast machinery EXISTS in the Rust core (validated + unit-tested).

### What's missing (the work)
1. **Gateway mode is NOT exposed to the app.** The only FFI `MeshNode` constructor is
   `new(sender_id, radio)` → a **plain** node (`node.rs:194`). The gateway constructor
   `new_gateway_with_policy(sender_id, radio, gateway: Arc<dyn MeshGateway>, policy)` is
   **`pub(crate)`** (`node.rs:597`) — not callable from Swift.
2. **The HTTP broadcast client isn't compiled into the xcframework.** `HttpGatewayRpc`
   (`rpc.rs:293+`, the real `ureq` client that calls `sendRawTransaction`) is behind the
   **`gateway-rpc`** cargo feature (`Cargo.toml:70`), only built for examples — not the default
   build the app/mac-node use.
3. **The phone's Send path broadcasts online, not over the mesh.** `WebHostView.swift`
   `sendTransaction` (@174) signs with the Keychain key and broadcasts directly via RPC. For the
   offline test it must call `submitSignedTransfer` to inject into the mesh instead.
4. **The Mac node is a pure relay.** `mac-node/main.swift:108` builds a plain
   `MeshNode(senderId: sid, radio: radio)`. It must become a **gateway** node so it broadcasts
   what it hears over BLE.

### SAFETY GUARD to respect (this is the money path)
`HttpGatewayRpc` is **testnet-guarded by design** — `MAINNET_RPC_HOSTS = ["rpc.nimiqwatch.com"]`
is **refused** (`rpc.rs:24,37`). So the live broadcast path only works against a **testnet** RPC
out of the box. That is exactly what we want for the proof.
- **Testnet (steps 1–4): safe, no gate.** Build it, prove it end-to-end.
- **Mainnet (step 5): HARD-GATED.** Lifting the mainnet-host guard is Andjroo's explicit call, and
  **the agent NEVER moves real funds** — Andjroo signs + taps Send on his phone; the Mac just
  delivers. Small amount, a recipient address he owns.

---

## The plan (build 1–4 on testnet, then Andjroo drives 5)

1. **Expose a gateway constructor via UniFFI** in `crates/nimmesh-core/src/node.rs` — e.g.
   `#[uniffi::constructor] pub fn new_gateway(sender_id, radio, rpc_url: String, network) -> Arc<Self>`
   that builds an `HttpGatewayRpc` + `RpcGateway` and calls the existing
   `new_gateway_with_policy(...)`. Enable the `gateway-rpc` feature in the xcframework build
   (the `cargo swift` / xcframework build script under `apple/` or the core's build script).
   **This edits `crates/` — the swap session's active area. It's ADDITIVE (a new constructor +
   a build flag), so collision risk is low, but coordinate / rebase carefully and don't touch
   swap code.** Regenerate the UniFFI Swift bindings.
2. **Make the Mac node a gateway** — `mac-node/main.swift`: construct `MeshNode.newGateway(...)`
   pointed at a **testnet** RPC host instead of the plain `MeshNode(senderId:radio:)`. It then
   broadcasts any signed tx heard over BLE. (Its relay-stats "payment relayed" counter already
   exists — the ★ line in the 2s timer.)
3. **Add an offline-send path on the phone** — in `WebHostView.swift` / `webui/index.html`, when
   offline (or a "send over mesh" toggle), sign then call `submitSignedTransfer` to inject into
   the mesh rather than RPC-broadcasting.
4. **Prove it on TESTNET** — phone airplane mode (BT on) → sign a testnet tx → BLE → Mac gateway
   broadcasts → confirms on the testnet chain. Watch `~/.nimmesh-relay/node.log` for the ★
   "payment relayed" line and verify the tx on a testnet explorer.
5. **MAINNET, small amount — Andjroo drives.** Deliberately open the mainnet path (his call), he
   signs + sends on his phone to an address he owns, Mac delivers, confirm on-chain.

---

## Key files
- **Rust core (crates/ — shared with swap session, coordinate):**
  - `crates/nimmesh-core/src/node.rs` — `new` FFI @194; `new_gateway_with_policy` pub(crate) @597;
    `submit_signed_transfer` @250; `build(...)` shared ctor.
  - `crates/nimmesh-core/src/gateway.rs` — `MeshGateway` trait @86; real `RpcGateway` @216.
  - `crates/nimmesh-core/src/rpc.rs` — `GatewayRpc` seam; `MockRpc`; `HttpGatewayRpc` @293
    (feature `gateway-rpc`); `MAINNET_RPC_HOSTS` guard @37.
  - `crates/nimmesh-core/Cargo.toml` — `gateway-rpc` feature @70.
  - `crates/nimmesh-core/generated/nimmesh_core.swift` — FFI surface (regenerate after step 1).
- **Phone app:** `apple/NimmeshApp/Sources/WebHostView.swift` (`sendTransaction` @174, lazy `node`
  @98, jsShim/bridge cases), `webui/index.html` (~2800 lines: send UI + Network screen + polling).
- **Mac node:** `mac-node/main.swift` (@108 plain node), `mac-node/BleMeshRadio.swift`,
  `mac-node/build.sh`, `mac-node/run.sh`.

## Build / deploy / run
- **iOS OTA:** `./apple/scripts/build-adhoc.sh` → `cp build/adhoc/export/NimmeshApp.ipa ota/nimmesh.ipa`
  → bump `Cargo.toml` version + `CHANGELOG.md` + `ota/manifest.plist` bundle-version + `ota/index.html`
  version → commit → PR → CI (contracts + core) green → **squash merge** (STANDING GRANT: merge
  nimmesh PRs without asking) → CF Pages auto-deploys → wait for
  `https://nimiq-nimmesh.pages.dev/ota/manifest.plist` to serve the new version → Andjroo reinstalls
  from `https://nimiq-nimmesh.pages.dev/ota/`.
- **Mac node:** `~/projects/nimiq.nimmesh-ui/mac-node/build.sh` then
  `open ~/projects/nimiq.nimmesh-ui/mac-node/nimmesh-node.app`. Log: `~/.nimmesh-relay/node.log`
  (read it directly — don't rely on screenshots). Stats: `~/.nimmesh-relay/stats.json`.

## Constraints & gotchas (Andjroo)
- **Don't suggest breaks.** He works mornings; just keep going.
- **The Mac mini IS the second node** — never pivot to "use a second phone."
- **Picture uploads are flaky** — lean on `~/.nimmesh-relay/node.log` and the on-device debug line.
- **Money path:** agent never broadcasts/moves real mainnet funds; testnet-first; Andjroo signs+sends.
- **mac-node real-cert signing** gives `errSecInternalComponent` when the agent (headless) runs
  `build.sh`; the Apple Development cert only works when **Andjroo** runs it in his GUI session.
  Ad-hoc fallback works headless but may need the manual Bluetooth "+" re-add after a rebuild.
- **macOS BT TCC:** prompt only appears for a signed, foreground, windowed GUI app in a console
  session; on the mini, add via System Settings → Bluetooth "+" (⌘⇧G to the app path, toggle ON,
  Quit & Reopen).
- **WKWebView** has no `confirm()`/`alert()` without a `WKUIDelegate`.
- **nimiq-ui rule 18:** no em/en dashes in any UI copy.
- Two directed BLE links per pair → ref-counting (already handled; keep it if editing the radio).
