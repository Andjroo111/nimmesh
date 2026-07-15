import CoreBluetooth
import Foundation
import NimmeshCore

/// G5: the native CoreBluetooth radio — the `BleRadio` foreign-trait impl (ADR-0002).
///
/// Each device runs **both** roles concurrently: a `CBPeripheralManager` that advertises the
/// nimmesh service + exposes one write+notify characteristic (inbound bytes), and a
/// `CBCentralManager` that scans, connects to peers' peripherals, subscribes, and writes
/// (outbound bytes). The Rust `MeshNode` holds this radio **strongly**; this holds the node
/// **weakly** (gotcha d). `send` is **fire-and-forget** (gotcha b) — the write outcome is
/// reported back via `node.onSendResult`. Callbacks only hand bytes + a peer id to the node
/// (which enqueues, gotcha a); the radio never sees a TTL or a packet.
///
/// **Phone-test scope:** the discovery + byte-pipe are the standard GATT pattern and compile
/// clean, but the on-device BLE *tuning* is what the 2-phone interop test validates — MTU for
/// the 256-byte packet (the Rust G6 fragmenter already splits larger messages), the iOS
/// background overflow-UUID dead spot, and collapsing the two directed links between a pair.
/// The core protocol (relay / dedup / TTL / store-and-forward) is already proven headlessly.
final class BleMeshRadio: NSObject, BleRadio {
    /// nimmesh GATT service + characteristic (write-without-response + notify).
    static let serviceUUID = CBUUID(string: "4E494D4D-4553-4800-0000-6E696D6D6573")
    static let charUUID = CBUUID(string: "4E494D4D-4553-4800-0001-6E696D6D6573")

    /// The Rust node. Held STRONGLY: a weak ref was getting released out from under the radio
    /// on-device, so `linkUp`'s `node?.onPeerConnected` silently no-op'd — the radio counted
    /// the peer but the mesh node never did (phone read 0 while the Mac saw 1). The node is
    /// app-lifetime here (never torn down), so the node↔radio cycle is an accepted one-instance
    /// leak in exchange for the callbacks actually landing. `stop()` breaks it if ever needed.
    /// Swapping this (the Swap sheet replaces the wallet node with a swap participant, and
    /// restores it on close) REPLAYS the links that are already up onto the new node — see
    /// `didSet`. Without that, a node swap while a peer is connected leaves the new node
    /// permanently at 0 peers (field bug 2026-07-14: "Listening… · 0 peers" forever).
    var node: MeshNode? {
        didSet {
            // `linkUp` announces a peer ONLY on its first link (the two directed BLE links per
            // pair are ref-counted), and `linkCount` lives on the RADIO, which outlives any
            // node. So a node installed AFTER a peer linked would never hear onPeerConnected —
            // the radio is linked, the new node sees nobody, discovery never starts. Replay the
            // live links onto whoever the node is now. Idempotent: the core dedups a peer it
            // already knows; on the first launch `linkCount` is empty, so this is a no-op.
            guard let n = node else { return }
            queue.async {
                for id in self.linkCount.keys { n.onPeerConnected(peerId: id) }
            }
        }
    }

    private let queue = DispatchQueue(label: "com.nimmesh.ble")
    private var central: CBCentralManager?
    private var peripheralMgr: CBPeripheralManager?
    private var meshChar: CBMutableCharacteristic?

    // Diagnostics for the on-device 2-node test: exactly which BLE roles are alive + what the
    // radio has seen, surfaced to the Network screen so the phone's side is visible.
    private var sawDiscover = 0            // central: advertisements discovered
    private var sawConnect = 0             // central: peripherals connected
    private var sawSubscribe = 0           // peripheral: centrals that subscribed to us
    func debugSummary() -> String {
        queue.sync {
            let cAuth = central?.authorization
            let auth: String
            switch cAuth {
            case .some(.allowedAlways): auth = "ok"
            case .some(.notDetermined): auth = "notDet"
            case .some(.denied): auth = "DENIED"
            case .some(.restricted): auth = "restr"
            default: auth = "?"
            }
            let scan = (central?.state == .some(.poweredOn)) ? "on" : "\(central?.state.rawValue ?? -1)"
            let adv = peripheralMgr?.isAdvertising == true ? "on" : "off"
            return "auth:\(auth) scan:\(scan) adv:\(adv) | disc:\(sawDiscover) conn:\(sawConnect) subs:\(sawSubscribe) | c-link:\(centralLinked.count) p-link:\(periphLinked.count) peers:\(linkCount.count)"
        }
    }

    // peerId (UUID string) → how to reach it.
    private var peripherals: [String: CBPeripheral] = [:]       // peers we are central to
    private var writeChars: [String: CBCharacteristic] = [:]    // their inbound characteristic
    private var subscribedCentrals: [String: CBCentral] = [:]   // peers subscribed to us (notify path)

    // A pair links TWICE (we are central to them AND they are central to us) under the SAME
    // peer id. Reference-count the two directed links so a flap on one doesn't tell the mesh
    // "peer gone" while the other is still up — that mismatch made the peer count crash to 0.
    // onPeerConnected fires only on the FIRST link up; onPeerDisconnected only when the LAST
    // link drops. `centralLinked`/`periphLinked` dedup each role so re-fires don't double-count.
    private var linkCount: [String: Int] = [:]
    private var centralLinked: Set<String> = []                 // we hold a central link to this peer
    private var periphLinked: Set<String> = []                  // this peer is subscribed to us
    private func linkUp(_ id: String) {
        linkCount[id, default: 0] += 1
        if linkCount[id] == 1 { node?.onPeerConnected(peerId: id) }
    }
    private func linkDown(_ id: String) {
        guard let c = linkCount[id] else { return }
        if c <= 1 { linkCount[id] = nil; node?.onPeerDisconnected(peerId: id) }
        else { linkCount[id] = c - 1 }
    }

    // MARK: BleRadio (called by Rust, off the worker thread)

    func startAdvertising() {
        queue.async {
            if self.peripheralMgr == nil {
                self.peripheralMgr = CBPeripheralManager(delegate: self, queue: self.queue)
            }
        }
    }

    func startScanning() {
        queue.async {
            if self.central == nil {
                self.central = CBCentralManager(delegate: self, queue: self.queue)
            }
        }
    }

    /// Fire-and-forget write to one peer. Prefer the central→peripheral write; fall back to a
    /// notify to a subscribed central. The outcome is reported asynchronously.
    func send(peerId: String, bytes: Data) {
        queue.async {
            if let p = self.peripherals[peerId], let ch = self.writeChars[peerId] {
                p.writeValue(bytes, for: ch, type: .withoutResponse)
                self.node?.onSendResult(peerId: peerId, ok: true)
            } else if let central = self.subscribedCentrals[peerId], let ch = self.meshChar {
                let ok = self.peripheralMgr?.updateValue(bytes, for: ch, onSubscribedCentrals: [central]) ?? false
                self.node?.onSendResult(peerId: peerId, ok: ok)
            } else {
                self.node?.onSendResult(peerId: peerId, ok: false)
            }
        }
    }

    func disconnect(peerId: String) {
        queue.async {
            if let p = self.peripherals[peerId] { self.central?.cancelPeripheralConnection(p) }
        }
    }

    func stop() {
        queue.async {
            self.central?.stopScan()
            self.peripheralMgr?.stopAdvertising()
            self.central = nil
            self.peripheralMgr = nil
            self.meshChar = nil
            self.peripherals.removeAll()
            self.writeChars.removeAll()
            self.subscribedCentrals.removeAll()
            self.node = nil // break the node↔radio cycle on teardown
        }
    }
}

// MARK: - Central role (scan → connect → subscribe → write)

extension BleMeshRadio: CBCentralManagerDelegate, CBPeripheralDelegate {
    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        if central.state == .poweredOn {
            central.scanForPeripherals(withServices: [Self.serviceUUID], options: nil)
        }
    }

    func centralManager(_ central: CBCentralManager, didDiscover peripheral: CBPeripheral,
                        advertisementData: [String: Any], rssi RSSI: NSNumber) {
        let id = peripheral.identifier.uuidString
        sawDiscover += 1
        peripherals[id] = peripheral // retain through connection
        central.connect(peripheral, options: nil)
    }

    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
        sawConnect += 1
        peripheral.delegate = self
        peripheral.discoverServices([Self.serviceUUID])
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
        for service in peripheral.services ?? [] where service.uuid == Self.serviceUUID {
            peripheral.discoverCharacteristics([Self.charUUID], for: service)
        }
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
        let id = peripheral.identifier.uuidString
        for ch in service.characteristics ?? [] where ch.uuid == Self.charUUID {
            writeChars[id] = ch
            peripheral.setNotifyValue(true, for: ch) // reverse path (peer notifies us)
            if centralLinked.insert(id).inserted { linkUp(id) } // count this directed link once
        }
    }

    func peripheral(_ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic, error: Error?) {
        guard let bytes = characteristic.value else { return }
        node?.onPacketReceivedFrom(peerId: peripheral.identifier.uuidString, bytes: bytes)
    }

    func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
        let id = peripheral.identifier.uuidString
        peripherals[id] = nil
        writeChars[id] = nil
        if centralLinked.remove(id) != nil { linkDown(id) } // our central link dropped
        central.scanForPeripherals(withServices: [Self.serviceUUID], options: nil) // keep the mesh healing
    }
}

// MARK: - Peripheral role (advertise → receive writes → notify subscribers)

extension BleMeshRadio: CBPeripheralManagerDelegate {
    func peripheralManagerDidUpdateState(_ peripheral: CBPeripheralManager) {
        guard peripheral.state == .poweredOn else { return }
        let ch = CBMutableCharacteristic(
            type: Self.charUUID,
            properties: [.writeWithoutResponse, .notify],
            value: nil,
            permissions: [.writeable]
        )
        let service = CBMutableService(type: Self.serviceUUID, primary: true)
        service.characteristics = [ch]
        meshChar = ch
        peripheral.add(service)
        peripheral.startAdvertising([CBAdvertisementDataServiceUUIDsKey: [Self.serviceUUID]])
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, didReceiveWrite requests: [CBATTRequest]) {
        for req in requests {
            if let bytes = req.value {
                node?.onPacketReceivedFrom(peerId: req.central.identifier.uuidString, bytes: bytes)
            }
            peripheral.respond(to: req, withResult: .success)
        }
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, central: CBCentral,
                          didSubscribeTo characteristic: CBCharacteristic) {
        let id = central.identifier.uuidString
        sawSubscribe += 1
        subscribedCentrals[id] = central
        if periphLinked.insert(id).inserted { linkUp(id) } // count this directed link once
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, central: CBCentral,
                          didUnsubscribeFrom characteristic: CBCharacteristic) {
        let id = central.identifier.uuidString
        subscribedCentrals[id] = nil
        if periphLinked.remove(id) != nil { linkDown(id) } // their central link dropped
    }
}
