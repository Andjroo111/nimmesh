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

    /// The Rust node — weak, to break the cross-language refcount cycle (ADR-0002 gotcha d).
    weak var node: MeshNode?

    private let queue = DispatchQueue(label: "com.nimmesh.ble")
    private var central: CBCentralManager?
    private var peripheralMgr: CBPeripheralManager?
    private var meshChar: CBMutableCharacteristic?

    // peerId (UUID string) → how to reach it.
    private var peripherals: [String: CBPeripheral] = [:]       // peers we are central to
    private var writeChars: [String: CBCharacteristic] = [:]    // their inbound characteristic
    private var subscribedCentrals: [String: CBCentral] = [:]   // peers subscribed to us (notify path)

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
        peripherals[id] = peripheral // retain through connection
        central.connect(peripheral, options: nil)
    }

    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
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
            node?.onPeerConnected(peerId: id)
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
        node?.onPeerDisconnected(peerId: id)
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
        subscribedCentrals[id] = central
        node?.onPeerConnected(peerId: id)
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, central: CBCentral,
                          didUnsubscribeFrom characteristic: CBCharacteristic) {
        let id = central.identifier.uuidString
        subscribedCentrals[id] = nil
        node?.onPeerDisconnected(peerId: id)
    }
}
