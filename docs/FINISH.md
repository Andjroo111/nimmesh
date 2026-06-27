# nimmesh — the finish line (autonomous to the phone test)

> Andjroo (2026-06-27): "Stop pausing. Create a goal with clear loops and take it as far as you
> can without involving me — even real-chain prep. I just want to do the phone test at the end
> with real funds (I have wallets to fund the addresses)." This file is the contract for that.

## Goal (one line)

Ship nimmesh to **"ready for Andjroo's real-device test"**: a complete, thoroughly-tested wallet
+ offline BLE mesh, **proven end-to-end on a real chain (testnet)**, **mainnet-capable behind a
toggle**, so the only thing left is the physical 2-phone test with real funds.

## Autonomy rules

- **Testnet = full speed, auto-merge on green** (keygen, signing, real testnet broadcast — all of
  it). Faucet-fund freely; testnet NIM is play money.
- **Build mainnet-CAPABLE, gated.** Add the mainnet path behind an explicit, off-by-default
  toggle so the phone test can flip it. Default stays testnet.
- **The one floor (not a pause):** never **autonomously broadcast real mainnet funds**. That is
  irreversible and is literally the phone test — Andjroo does it on-device. Everything up to that
  is built + wired + verified.
- **Real devices are Andjroo's:** I *write + compile* the native BLE shim; on-device BLE interop
  + the real-funds send are the phone test.
- **Invariant:** the seed never crosses the FFI boundary, the mesh, or a log — only a public key
  + a signature do.

## The loop — one PR per step, auto-merge on green, no pausing

1. **C1c-2 — live testnet send from the app.** Swift JSON-RPC (URLSession): fetch the head for
   `validityStartHeight` → sign with the Keychain key (AppSigner) → `sendRawTransaction` →
   poll `getTransactionByHash` to inclusion. Faucet-fund the wallet. Prove a real testnet tx
   sent from the app in the simulator. Wire the Send sheet + pending→settled UX.
2. **G5 — native BLE shim (CoreBluetooth).** Implement the `BleRadio` foreign trait in Swift
   (concurrent `CBCentralManager` + `CBPeripheralManager`), wire `MeshNode` to real BLE events.
   Write + compile-gate (`xcodebuild`); on-device interop is the phone test.
3. **Mainnet-capable (gated).** A simple, off-by-default network toggle (testnet ↔ mainnet) the
   phone test flips; mainnet RPC config; the validity / fee / "unconfirmed-until-inclusion"
   honesty audited for real funds. No autonomous real-fund broadcast.
4. **Device-test runbook** (`docs/DEVICE-TEST.md`): free-tier Apple ID signing (no $99 needed for
   the test), install on 2 phones, fund the addresses, exact steps for the offline-mesh + the
   real-funds send.

When all four are done, nimmesh is feature-complete and the only remaining action is Andjroo's
physical test. Report then (and at any genuine blocker), not between steps.

## Apple signing (the $99 question)

A **free Apple ID** ("Personal Team") signs + runs the app on your own devices — the profile
expires every 7 days (re-install from Xcode), max ~3 apps, no TestFlight. **CoreBluetooth needs
no paid entitlement**, so the offline-mesh + real-funds phone test works for **$0**. The
**$99/yr** only adds TestFlight (share / no weekly re-sign), the App Store, and push. Start free;
pay only to distribute or ship.
