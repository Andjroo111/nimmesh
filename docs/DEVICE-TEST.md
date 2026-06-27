# nimmesh — device test runbook (your turn)

The whole testable wallet is built + verified in the simulator. This is the one part only you
can do: the **offline BLE mesh on 2 real phones**, then a **real-funds send**. CoreBluetooth
doesn't work in the simulator, so this needs hardware.

## What you need

- A **Mac with Xcode** (the Mini or your laptop).
- **2 iPhones** (yours + a second/old one, or a friend's).
- An **Apple ID** — a **free** one is enough (see "Signing" below); the **$99/yr** program is
  optional (only for TestFlight / no weekly re-sign).
- For the mainnet step: a **Nimiq wallet with a little real NIM** to fund the app's address.

## 1. Sign + install (free Apple ID works)

1. `cd ~/projects/nimiq.nimmesh/crates/nimmesh-core && cargo swift package -n NimmeshCore -p ios -y`
   then `cd ../../apple && xcodegen generate` (regenerates `NimmeshApp.xcodeproj`).
2. Open `apple/NimmeshApp.xcodeproj` in Xcode. Select the **NimmeshApp** target → **Signing &
   Capabilities** → check **Automatically manage signing** → **Team: your personal Apple ID**
   ("Personal Team"). Xcode will pick a bundle id; that's fine.
3. Plug in iPhone #1, pick it as the run destination, press **Run** (⌘R). Repeat for iPhone #2.
4. On each phone: **Settings → General → VPN & Device Management → trust your developer cert**.
   - Free-tier apps **expire after 7 days** — just press Run again from Xcode to refresh.
5. First launch asks for **Bluetooth permission** — allow it (the mesh needs advertise + scan).

## 2. The offline-mesh test — on TESTNET first (free, no risk)

1. On **both** phones, open the app and note each wallet address (the home header / **Receive**).
2. Fund both addresses with testnet NIM: tap the testnet faucet at
   `https://faucet.pos.nimiq-testnet.com/tapit` (POST `address=NQ…`), or send from a testnet wallet.
3. Put **both phones in Airplane Mode but turn Bluetooth back ON.** The mesh bar should show
   `mesh meshed · 1 nearby` once they see each other (give it a few seconds, keep both foregrounded).
4. On phone A: **Send** → enter phone B's address + a small amount → **Send**. The signed tx now
   has to ride the BLE mesh.
5. Bring **one** phone back online (or have a third online device in range). The tx broadcasts at
   that hop; verify it on the testnet explorer and that phone B shows it land (the "pending → ✓"
   closure). That's the whole point: **you paid with no internet.**

### What to watch (the on-device tuning this test exists to surface)
- **MTU / packet size:** a signed tx packet is ~256 B; the Rust core already fragments larger
  messages, but confirm writes arrive whole. If not, that's an MTU negotiation tweak in `BleMeshRadio`.
- **iOS background:** iOS throttles BLE in the background (the overflow-UUID dead spot). For the
  test, keep both apps **foregrounded**. (Background-survival UX is a later decision.)
- **Two directed links:** A↔B can form two connections (each as central to the other's peripheral);
  the relay dedups, but if you see doubled traffic that's the link-dedup tuning.

## 3. The real-funds test — MAINNET (small amount, your call)

> Mainnet is the gated switch, only enable it deliberately. The in-app **network toggle** is on the
> Send sheet (Testnet / Mainnet); switching to mainnet asks you to confirm and shows a real-funds
> warning. The app never auto-sends, a mainnet send is always your tap.

1. On the **Send** sheet, flip the network toggle to **Mainnet** (confirm the prompt). Fund the app's
   address with a **small** amount of real NIM from your wallet.
2. Repeat the mesh send with a tiny amount first. Confirm inclusion on the mainnet explorer.
3. Only scale up once a small send works end to end.

## Cost summary
Free Apple ID → full mesh + real-funds test (re-sign weekly). `$99/yr` only if you want TestFlight
(share a build / no weekly re-sign) or the App Store. Start free.
