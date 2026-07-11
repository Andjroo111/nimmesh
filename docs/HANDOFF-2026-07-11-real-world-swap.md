# Handoff — real-world swap testing (2026-07-11)

**Everything below is merged to `main` (v0.72.0, commit `eb97c36`) and on GitHub. No open PRs,
clean tree.** This document hands the swap to real-world testing. Read it end to end before
touching the money path.

---

## TL;DR — where the swap stands

A cross-chain **NIM ⇄ USDC atomic swap** is fully built, security-reviewed, hardened, and
**proven on-chain on testnet through the exact constructors the phone app calls**. The
autonomous roadmap is complete; everything remaining is either a real-device test (Track A,
testnet, safe to run now) or a mainnet real-funds step (Track B, **Andjroo-gated** — funds +
written authorization required, the agent never moves real funds autonomously).

Chosen first mainnet pair (Andjroo, 2026-07-09): **USDC on Polygon** ⇄ NIM (least new risk — the
HTLC stack is already live-proven on Amoy).

### What is proven (with receipts)
- **Act 2** (`docs/swap/ACT2-RECEIPTS.md`) — first real atomic swap, two headless nodes, real
  NIM-testnet ⇄ USDC-Amoy, secret revealed on-chain, both legs settled, nothing stranded.
- **G10** (`docs/swap/G10-RECEIPTS.md`) — the same swap run through the **actual app-facing FFI
  ctors** `new_live_swap_initiator` (phone) + `new_live_swap_responder` (Mac). Independently
  verified: `withdraw(S)` mined (Amoy blk 41,838,613), initiator claim address holds the 1 USDC.
- **M5** (`docs/swap/M5-RECEIPTS.md`, ADR-0011) — RPC-trust hardening: verifier cross-read against
  a 2nd RPC, NIM content-hash bind, independent reveal-confirmation before releasing `S`.

### Security posture
The **G8 independent review** passed after fixes: contracts + pure decision layers were already
solid; the findings were all in the wiring and are **closed on testnet**:
- **C1** — a live signer can never be built with the sim `AcceptAllVerifier` or `sim_secret`
  (asserted in `MeshNode::build`). Safe by construction, not by remembering builder calls.
- **H2** — the #189 secret-reuse tombstone now persists across restart (in the session snapshot).
- **M3** — reveal-deadline checked *before* the on-chain reveal; `S` held off the mesh until the
  withdraw is buried.
- **M4** — timelocks anchored to the chain head + absolute-timelock sanity bounds.
- **M5** — cross-read / content-hash / independent-reveal (above).

---

## Track A — real-world TESTNET swap on a real device (safe to run now)

This is the honest next real-world milestone before mainnet: drive a **real testnet swap from the
phone app over Bluetooth**, with the Mac as the live responder. No mainnet, no real funds.

**Step 1 — refresh the phone build (optional but recommended).** OTA currently serves **0.67.0**
(has the "Real testnet coins" swap toggle from G10b). The core is now **0.72.0** with the M5
hardening; for the device test to carry M5 in its embedded framework, rebuild + republish the ipa:
```
cd ~/projects/nimiq.nimmesh-ui
./apple/scripts/build-adhoc.sh                 # builds with -F polygon-gateway (~3.4M)
cp build/adhoc/export/NimmeshApp.ipa ota/nimmesh.ipa
# bump Cargo.toml + CHANGELOG + ota/manifest.plist bundle-version + ota/index.html, PR, merge, verify OTA serves it
```
(Testnet device testing on 0.67.0 is acceptable — M5 is mainnet-relevant hardening — but ship the
current core so what you test is what goes to mainnet.)

**Step 2 — put the Mac in LIVE responder mode.** It is currently running the **sim** responder
(`--swap-responder --bitchat`, pid was 35819). Relaunch it live:
```
pkill -f nimmesh-node
open ~/projects/nimiq.nimmesh-ui/mac-node/nimmesh-node.app --args --swap-responder-live
# (the G10c flag — carries LiveResponderSigner: real Amoy USDC leg + real NIM claim, testnet-pinned)
```
Watch `~/.nimmesh-relay/node.log`.

**Step 3 — drive a swap from the phone.** Near the Mac, open the app → **Swap** → confirm the
"Real testnet coins" toggle/label is showing → small amount (the proven size is **5 tNIM ⇄ 1
USDC**) → Confirm. The Swap sheet renders the node's real coordinator phases. Fund the phone
wallet with tNIM first via the faucet if needed:
`POST address=NQ… https://faucet.pos.nimiq-testnet.com/tapit`.

**Step 4 — verify on-chain** and capture receipts (append to `docs/swap/G10-RECEIPTS.md`): the NIM
HTLC on nimiq-testnet, the Amoy `newSwap` / `withdraw(S)` on amoy.polygonscan.com, the claim
balance delta. The example is self-recovering; if a run dies, the refund path reclaims — never
strand funds.

**Also gated / still-open real-device work:** the true **2-phone BLE swap** (G12, issue #83) needs
a second iOS-16+ device (Andjroo has only one modern iPhone; the Mac responder stands in today).

---

## Track B — the mainnet real-funds swap (Andjroo-gated, NOT autonomous)

The full runbook is **`docs/MAINNET-GATING.md` §8**. Summary of the gate — all must be green, in
order, before any real value moves:

1. **M6 — mainnet confirmation-depth retune** (ADR-0003). Testnet depths (NIM 2 / USDC 5) are NOT
   finality-safe on mainnet. Set Polygon + NIM mainnet depths; exercise the reorg re-verify path.
2. **Trusted secondary RPC on the live path.** The M5 cross-read seam is built but only Amoy has
   two public endpoints wired; NIM needs a second (ideally self-hosted) mainnet RPC. Wire a
   trusted/self-hosted endpoint as the cross-read source for real funds.
3. **The guard-lift PR** — a deliberate, **off-by-default**, `money-path` + `needs:owner` PR (does
   NOT auto-merge) that lifts exactly: `HttpGatewayRpc::new_mainnet` + the `polygon_gateway`
   mainnet host allow-list; the `guard_testnet` / `guard_amoy` ctor guards; `fund_nim`'s
   testnet-network assertion; `MeshNode::build`'s testnet-only live-signer assertion; the
   `ConfirmationPolicy` depths (→ M6). **Get an independent review of that diff** before merge.
4. **Deploy a source-verified `NimmeshHtlc` on Polygon mainnet** (fresh, forwarder-bound, verified
   on polygonscan). Record its address in the runbook. **Verify the canonical Polygon USDC address
   from Circle's docs** (native USDC vs bridged USDC.e differ — do NOT hardcode from memory).
5. **Hard per-swap cap wired in code** — the responder refuses any swap above the agreed test size.
6. **Andjroo provides + authorizes**: a small amount of **mainnet USDC + a little POL for gas** on
   the responder address, and **written go** for the ≤ $5 first swap.

**The shape of the first mainnet run (§8.3):** a swap **with himself across two chains** — Andjroo
funds the NIM leg on his phone (on-device, exactly like the mainnet mesh payment) AND controls the
USDC responder (runs it, or launches the capped rig with an explicit go and watches each
broadcast). Zero counterparty-trust exposure, clean self-refund floor. A third-party counterparty
only after that proves clean.

**The safety floor (non-negotiable, `docs/MAINNET-GATING.md` §1/§3/§5, §8.1):** the agent never
autonomously moves mainnet funds; the responder's `newSwap` is the one real-value action a node
takes — hence capped, explicitly launched, Andjroo-triggered. The secret leaves the initiator only
inside a public claim tx. The timelock refund is always the worst-case floor (each side reclaims
its own).

---

## Live rig reference

**Contracts (Amoy testnet; the code is what would deploy to mainnet):**
- `NimmeshHtlc` v2 (forwarder-bound, the app targets this): `0xb3B3703E07AC897B7E3e864C113a2Fa547D76736`
- `NimmeshForwarder`: `0x94618C9429BA431d69dA1762b5ABd3AaaA0267e1`
- `NimmeshHtlc` v1 (forwarder=0): `0xaaCa309B5EF3e57D3f206220F230F5cB2562F7f3`

**Amoy wallet** (`~/.nimmesh-amoy.env`, chmod 600 — never print the key):
`0xA7bB819Ba03743643249dFCCa7508280eCE059b1` — ~18 USDC, **~0.028 POL (LOW)**. The G10c example
seeds 0.02 POL to the initiator's derived gas account when empty; a couple more testnet swaps will
exhaust it. **The Amoy POL faucet needs a human** (captcha) — top up before a device-test session.

**NIM testnet wallets:** `~/secrets/nimmesh-swap-wallets.env` (read via `docs/swap/WALLETS.md`;
never print secrets). Treasury `NQ92 VGEX VYH9 KHP0 Y00L DAQM 32N2 8H12 H9F7` (~110 tNIM). Faucet
`https://faucet.pos.nimiq-testnet.com/tapit` is scriptable.

**RPCs:** NIM testnet `https://rpc.testnet.nimiqwatch.com`; Amoy `https://rpc-amoy.polygon.technology`
(public, `eth_getLogs` capped ~50 blocks — verifiers anchor scans at the funding block).

**Mac node:** `~/projects/nimiq.nimmesh-ui/mac-node/nimmesh-node.app`. Flags: `--swap-responder`
(sim), `--swap-responder-live` (real testnet, Track A), `--bitchat` (joins the real Bitchat mesh),
`--mainnet` (mainnet mesh *payment* gateway, NOT swap). Log: `~/.nimmesh-relay/node.log`. Bluetooth
grant persists across rebuilds (manual "+"-add in System Settings if it ever resets).

---

## Key code + docs

- **App-facing FFI ctors:** `MeshNode::new_live_swap_initiator` / `new_live_swap_responder`
  (`crates/nimmesh-core/src/swap_participant_ffi.rs`) — testnet/Amoy-pinned, C1-asserted.
- **Live signer:** `crates/nimmesh-core/src/live_swap_signer.rs` (LiveInitiatorSigner /
  LiveResponderSigner, behind `polygon-gateway`). **Verifiers:** `nim_verifier.rs`,
  `polygon_verifier.rs`, `amoy_swap_verifier.rs` (all with `.with_secondary` cross-read).
- **Never-strand:** `LiveLockBook` + `NimHtlcRefunder` (refund on failure/abandon).
- **App bridge:** `apple/NimmeshApp/Sources/SwapMesh.swift` (`swapMeshStart {real:true}`,
  `swapMeshRefund`). Swap sheet: `webui/index.html`.
- **Mac responder:** `mac-node/main.swift` (`--swap-responder-live`).
- **Docs:** `docs/MAINNET-GATING.md` §8 (the gate), `docs/swap/{ACT2,G10,M5}-RECEIPTS.md`,
  `docs/adr/0001..0011` (0003 depths, 0004 reveal-deadline, 0007 timelock boundary, 0010 term
  mapping, 0011 RPC-trust). Security findings table: `docs/swap/INTEGRATION-AGENDA.md`.
- **Issues:** #83 (G12 on-device 2-phone), #81 (WebView bridge, G10 — now largely done), #58
  (epic). #189 (closed). The needs:owner mainnet items belong on a new gated issue.

---

## Suggested first move for the next agent

1. Top up Amoy POL (ask Andjroo — captcha faucet) and fund the phone wallet with tNIM.
2. Rebuild + republish the OTA ipa at the current core (0.72.0) so the device carries M5.
3. Relaunch the Mac as `--swap-responder-live`.
4. Run Track A on Andjroo's phone → capture receipts → confirm the app-driven real testnet swap.
5. Only then open the Track B mainnet gate with Andjroo (M6 + guard-lift PR for his review + the
   deploy + his funds + his written go). Prepare the guard-lift as a reviewable diff; do NOT merge
   it or move real funds without his explicit authorization.
