import CoreBluetooth
import Foundation

// nimmesh Mac mesh node — a headless CoreBluetooth peer that joins the SAME Bluetooth mesh
// the iPhones use (identical service UUID). Bring a phone with the app near the Mac and each
// should see the other: "mesh meshed · 1 nearby" on the phone, "peers: 1" here. This is a
// test rig / permanent second node — no wallet UI, just the Rust MeshNode + the BLE radio.

setbuf(stdout, nil)
func ts() -> String {
    let f = DateFormatter(); f.dateFormat = "HH:mm:ss"
    return f.string(from: Date())
}
func line(_ s: String) { print("[\(ts())] \(s)") }

line("nimmesh mac-node starting — core \(coreVersion())")

// A stable sender id for this node (32 bytes derived from the host name so it persists across
// runs but is distinct from any phone). No wallet keys here — this node only relays.
let host = (Host.current().localizedName ?? "mac-node").data(using: .utf8) ?? Data("mac".utf8)
var sid = Data(count: 32)
for (i, b) in host.enumerated() where i < 32 { sid[i] = b }
sid[31] = 0x4D // 'M' marker so it's obviously the mac node

let radio = BleMeshRadio()
let node = MeshNode(senderId: sid, radio: radio)
radio.node = node
radio.onLog = { line($0) }

// Bring the radio up (advertise + scan), exactly like the app does on first meshStatus.
radio.startAdvertising()
radio.startScanning()
line("radio up — advertising + scanning. Waiting for a nimmesh phone nearby…")
line("(if you see 'BLUETOOTH NOT AUTHORIZED', grant Bluetooth to your terminal in")
line(" System Settings › Privacy & Security › Bluetooth, then re-run.)")

// Heartbeat: print the live peer count + relay stats whenever they change, so you can watch
// the mesh form. reachability() mirrors what the phone shows.
var lastPeers: UInt32 = 0xFFFFFFFF
var lastRelayed: UInt64 = 0
let timer = DispatchSource.makeTimerSource(queue: .main)
timer.schedule(deadline: .now() + 1, repeating: 2.0)
timer.setEventHandler {
    let peers = node.peerCount()
    let stats = node.relayStats()
    if peers != lastPeers {
        let reach: String
        switch node.reachability() {
        case .online: reach = "online"
        case .meshed: reach = "meshed"
        case .offline: reach = "offline"
        }
        line("mesh \(reach) · \(peers) nearby")
        lastPeers = peers
    }
    if stats.paymentsRelayed != lastRelayed {
        line("relayed \(stats.paymentsRelayed) payment(s) so far — you ARE the network")
        lastRelayed = stats.paymentsRelayed
    }
}
timer.resume()

// Clean shutdown on Ctrl-C.
signal(SIGINT, SIG_IGN)
let sigint = DispatchSource.makeSignalSource(signal: SIGINT, queue: .main)
sigint.setEventHandler {
    line("shutting down…")
    radio.stop()
    exit(0)
}
sigint.resume()

RunLoop.main.run()
