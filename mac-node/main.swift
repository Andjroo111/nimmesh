import AppKit
import CoreBluetooth
import Foundation

// nimmesh Mac mesh node — a headless CoreBluetooth peer that joins the SAME Bluetooth mesh
// the iPhones use (identical service UUID). It also acts as a "relay operator" console:
// it measures the two things a mesh-incentive model would reward (see docs/adr/0009):
//   • AVAILABILITY — how long this node stays up and reachable (uptime), and
//   • UTILITY       — how many payments actually pass through it (throughput).
// Both are persisted across runs in ~/.nimmesh-relay/stats.json so "lifetime contribution"
// accrues. The projected-reward line is ILLUSTRATIVE (rates are placeholders until the
// ADR-0009 economics are set) — it exists to make the model concrete, not to promise value.

setbuf(stdout, nil)
func ts() -> String {
    let f = DateFormatter(); f.dateFormat = "HH:mm:ss"
    return f.string(from: Date())
}
// Tee every line to ~/.nimmesh-relay/node.log so the state is readable without screenshots.
let logDir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".nimmesh-relay")
let logFileURL = logDir.appendingPathComponent("node.log")
try? FileManager.default.createDirectory(at: logDir, withIntermediateDirectories: true)
let logFH: FileHandle? = {
    if !FileManager.default.fileExists(atPath: logFileURL.path) {
        FileManager.default.createFile(atPath: logFileURL.path, contents: nil)
    }
    let fh = try? FileHandle(forWritingTo: logFileURL)
    try? fh?.seekToEnd()
    return fh
}()
func line(_ s: String) {
    let msg = "[\(ts())] \(s)"
    print(msg)
    logFH?.write((msg + "\n").data(using: .utf8) ?? Data())
}

// ---- illustrative reward rates (PLACEHOLDERS — real economics pending ADR-0009) ----
let UPTIME_NIM_PER_HOUR = 0.10        // availability reward per hour up
let USAGE_NIM_PER_PAYMENT = 1.00      // utility reward per payment relayed

// ---- persistent lifetime contribution ----
struct RelayState: Codable {
    var firstRunISO: String
    var sessions: Int
    var lifetimeUptimeSec: Double
    var lifetimePaymentsRelayed: UInt64
    var lifetimePacketsRelayed: UInt64
    var peakPeers: UInt32
}
let stateDir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".nimmesh-relay")
let stateURL = stateDir.appendingPathComponent("stats.json")
func loadState() -> RelayState {
    if let d = try? Data(contentsOf: stateURL), let s = try? JSONDecoder().decode(RelayState.self, from: d) {
        return s
    }
    let iso = ISO8601DateFormatter().string(from: Date())
    return RelayState(firstRunISO: iso, sessions: 0, lifetimeUptimeSec: 0,
                      lifetimePaymentsRelayed: 0, lifetimePacketsRelayed: 0, peakPeers: 0)
}
func saveState(_ s: RelayState) {
    try? FileManager.default.createDirectory(at: stateDir, withIntermediateDirectories: true)
    if let d = try? JSONEncoder().encode(s) { try? d.write(to: stateURL) }
}

var state = loadState()

func fmtDuration(_ sec: Double) -> String {
    let s = Int(sec)
    let h = s / 3600, m = (s % 3600) / 60, ss = s % 60
    if h > 0 { return "\(h)h \(m)m" }
    if m > 0 { return "\(m)m \(ss)s" }
    return "\(ss)s"
}

// `--stats` just prints the saved lifetime contribution and exits (read-only; for cron/quick
// checks). Handled BEFORE the session bump so it never mutates state.
if CommandLine.arguments.contains("--stats") {
    let hrs = state.lifetimeUptimeSec / 3600
    let reward = hrs * UPTIME_NIM_PER_HOUR + Double(state.lifetimePaymentsRelayed) * USAGE_NIM_PER_PAYMENT
    print("nimmesh relay — lifetime contribution")
    print("  since         \(state.firstRunISO)")
    print("  sessions      \(state.sessions)")
    print("  availability  \(fmtDuration(state.lifetimeUptimeSec)) up")
    print("  utility       \(state.lifetimePaymentsRelayed) payments relayed (\(state.lifetimePacketsRelayed) packets)")
    print("  peak peers    \(state.peakPeers)")
    print(String(format: "  est. reward   ~%.2f NIM  (ILLUSTRATIVE: %.2f/uptime-hr + %.2f/payment — pending ADR-0009)",
                 reward, UPTIME_NIM_PER_HOUR, USAGE_NIM_PER_PAYMENT))
    exit(0)
}

// Real run: now bump the session counter and capture the lifetime baselines.
let baseUptime = state.lifetimeUptimeSec
let basePayments = state.lifetimePaymentsRelayed
let basePackets = state.lifetimePacketsRelayed
state.sessions += 1
saveState(state)

line("nimmesh mac-node starting — core \(coreVersion())")
line("relay operator: lifetime \(fmtDuration(baseUptime)) up · \(basePayments) payments relayed over \(state.sessions - 1) prior session(s)")

// A stable sender id (32B from the host name) so this node is distinct from any phone.
let host = (Host.current().localizedName ?? "mac-node").data(using: .utf8) ?? Data("mac".utf8)
var sid = Data(count: 32)
for (i, b) in host.enumerated() where i < 32 { sid[i] = b }
sid[31] = 0x4D // 'M' marker

let radio = BleMeshRadio()
let node = MeshNode(senderId: sid, radio: radio)
radio.node = node
radio.onLog = { line($0) }

var startTime = Date()
var lastPeers: UInt32 = 0xFFFFFFFF
var lastPayments: UInt64 = 0
var lastStatusPrint = Date(timeIntervalSince1970: 0)

func syncState() {
    let stats = node.relayStats()
    state.lifetimeUptimeSec = baseUptime + Date().timeIntervalSince(startTime)
    state.lifetimePaymentsRelayed = basePayments + stats.paymentsRelayed
    state.lifetimePacketsRelayed = basePackets + stats.packetsRelayed
    state.peakPeers = max(state.peakPeers, node.peerCount())
    saveState(state)
}

// The Bluetooth request MUST happen after the app has finished launching and is active —
// otherwise macOS has no running foreground app to attach the permission prompt to, and it
// silently stays notDetermined forever (the bug). So all radio startup lives in
// applicationDidFinishLaunching, not in top-level code that runs before the event loop.
final class AppDelegate: NSObject, NSApplicationDelegate {
    var timer: DispatchSourceTimer?
    var sigint: DispatchSourceSignal?
    var window: NSWindow?
    var statusLabel: NSTextField?

    func applicationDidFinishLaunching(_ note: Notification) {
        // A REAL visible window: macOS attaches the Bluetooth permission prompt to a
        // foreground app that actually shows something on screen. A windowless CLI-in-a-bundle
        // may be why the prompt never presented. This makes us an unambiguous GUI app.
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 520, height: 180),
                         styleMask: [.titled, .closable, .miniaturizable],
                         backing: .buffered, defer: false)
        w.title = "nimmesh mesh node"
        w.center()
        let label = NSTextField(labelWithString: "nimmesh mesh node is starting…\n\nWhen macOS asks, click Allow to let this node use Bluetooth\nand join the mesh.")
        label.frame = NSRect(x: 24, y: 24, width: 472, height: 132)
        label.font = .systemFont(ofSize: 14)
        w.contentView?.addSubview(label)
        w.makeKeyAndOrderFront(nil)
        window = w
        statusLabel = label

        NSApp.activate(ignoringOtherApps: true)
        line("bluetooth authorization: \(BleMeshRadio.authName(CBCentralManager.authorization))")

        startTime = Date()
        radio.startAdvertising()
        radio.startScanning()
        line("radio up — advertising + scanning. Waiting for a nimmesh phone nearby…")

        let statusLabel = self.statusLabel
        let t = DispatchSource.makeTimerSource(queue: .main)
        t.schedule(deadline: .now() + 1, repeating: 2.0)
        var beatCount = 0
        t.setEventHandler {
            let peers = node.peerCount()
            let stats = node.relayStats()
            syncState()
            // Keepalive: emit a head beacon every ~14s (7 × 2s ticks) so the BLE link to a
            // phone doesn't idle-timeout and flap.
            beatCount += 1
            if beatCount % 7 == 0 { node.pollBeacon() }
            let reachNow: String
            switch node.reachability() {
            case .online: reachNow = "online"
            case .meshed: reachNow = "meshed"
            case .offline: reachNow = "offline"
            }
            statusLabel?.stringValue = "nimmesh mesh node — running\n\nMesh: \(reachNow) · \(peers) nearby\nPayments relayed: \(stats.paymentsRelayed)\n\nBring a phone with the nimmesh app nearby to link up."
            if peers != lastPeers {
                line("mesh \(reachNow) · \(peers) nearby")
                lastPeers = peers
            }
            if stats.paymentsRelayed != lastPayments {
                line("★ payment relayed through this node (\(stats.paymentsRelayed) this session) — utility earned")
                lastPayments = stats.paymentsRelayed
            }
            if Date().timeIntervalSince(lastStatusPrint) >= 60 {
                lastStatusPrint = Date()
                let sessUp = Date().timeIntervalSince(startTime)
                let hrs = state.lifetimeUptimeSec / 3600
                let reward = hrs * UPTIME_NIM_PER_HOUR + Double(state.lifetimePaymentsRelayed) * USAGE_NIM_PER_PAYMENT
                line("── relay status ── session \(fmtDuration(sessUp)) up, \(peers) peers · lifetime \(fmtDuration(state.lifetimeUptimeSec)) + \(state.lifetimePaymentsRelayed) payments")
                line(String(format: "   est. contribution reward ~%.2f NIM (illustrative)", reward))
            }
        }
        t.resume()
        timer = t

        signal(SIGINT, SIG_IGN)
        let s = DispatchSource.makeSignalSource(signal: SIGINT, queue: .main)
        s.setEventHandler {
            syncState()
            line("shutting down — lifetime \(fmtDuration(state.lifetimeUptimeSec)) up, \(state.lifetimePaymentsRelayed) payments relayed. Saved.")
            radio.stop()
            exit(0)
        }
        s.resume()
        sigint = s
    }
}

// Become a proper FOREGROUND app so macOS presents the Bluetooth prompt, then run the app
// event loop; the delegate requests Bluetooth once the app is live (see the comment above).
let nsApp = NSApplication.shared
nsApp.setActivationPolicy(.regular)
let appDelegate = AppDelegate()
nsApp.delegate = appDelegate
nsApp.run()
