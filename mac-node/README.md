# nimmesh Mac mesh node

A headless CoreBluetooth peer that joins the **same** Bluetooth mesh the iPhone app uses
(identical GATT service `4E494D4D-4553-4800-0000-6E696D6D6573`). It runs the real Rust
`MeshNode` — so a Mac running this is a genuine second mesh node: bring a phone with the
nimmesh app near it and each sees the other ("mesh meshed · 1 nearby" on the phone,
`mesh meshed · 1 nearby` in the Mac's log). No wallet UI here — it advertises, scans,
connects, and relays. It's a permanent test rig and, later, can play the responder side
of a swap so mesh tests don't need two phones.

## Build

```bash
./mac-node/build.sh
```

Compiles the Rust core for macOS (`libnimmesh_core.a`), swiftc-links it with the same
CoreBluetooth radio the phone uses, and assembles + ad-hoc-signs `nimmesh-node.app`.
(Ad-hoc because this headless Mac's Keychain blocks real-cert signing; the only cost is
re-approving Bluetooth after a rebuild — see below.)

## Run

```bash
./mac-node/run.sh          # logs stream to your terminal; Ctrl-C to stop
```

## One-time Bluetooth approval (required, ~30 seconds)

macOS won't let any program use Bluetooth until you approve it, and that approval prompt
**only appears in a real logged-in desktop session** — not from an automated/SSH shell.
So the first run has to be done by a human at the Mac's screen:

1. **Screen Share into the Mac mini** (Finder → Go → Connect to Server, or the Screen
   Sharing app) so you have its actual desktop.
2. Open **Terminal** there and run:
   `~/projects/nimiq.nimmesh-ui/mac-node/run.sh`
3. A dialog appears: **"nimmesh-node" would like to use Bluetooth** → click **Allow**.
4. The log flips from `BLUETOOTH NOT AUTHORIZED` to `advertising the nimmesh service`
   and `central: powered on, scanning`. Leave it running.

After that it's granted permanently (until the next `build.sh`, which re-signs and needs
one more Allow). You can confirm/re-toggle it any time under **System Settings › Privacy
& Security › Bluetooth**.

## The mesh test

With the node running and approved, open the nimmesh app on your iPhone and walk it near
the Mac mini. Within a few seconds:
- the Mac log prints `linked to peer … ✓` then `mesh meshed · 1 nearby`,
- the phone's Network screen shows `mesh meshed · 1 nearby` and a green peer hex lights up
  next to your gold node.

That's the first real phone-to-phone (phone-to-Mac) relay over Bluetooth — the mesh is live.
