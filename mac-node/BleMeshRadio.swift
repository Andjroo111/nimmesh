import CoreBluetooth
import Foundation

/// macOS twin of the iPhone's `BleMeshRadio` (apple/NimmeshApp/Sources/BleMeshRadio.swift).
/// CoreBluetooth is identical on macOS, so this speaks the SAME nimmesh GATT service and
/// characteristic the phones advertise — a Mac running this node is just another mesh peer.
/// The only additions are log lines (this is a headless test node, not the app).
final class BleMeshRadio: NSObject, BleRadio {
    // Byte-identical to the iOS shim so the Mac and the phones see each other.
    static let serviceUUID = CBUUID(string: "4E494D4D-4553-4800-0000-6E696D6D6573")
    static let charUUID = CBUUID(string: "4E494D4D-4553-4800-0001-6E696D6D6573")

    weak var node: MeshNode?
    var onLog: ((String) -> Void)?

    private let queue = DispatchQueue(label: "com.nimmesh.ble")
    private var central: CBCentralManager?
    private var peripheralMgr: CBPeripheralManager?
    private var meshChar: CBMutableCharacteristic?

    private var peripherals: [String: CBPeripheral] = [:]
    private var writeChars: [String: CBCharacteristic] = [:]
    private var subscribedCentrals: [String: CBCentral] = [:]

    private func log(_ s: String) { onLog?(s) }

    // MARK: BleRadio (called by Rust)

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
        switch central.state {
        case .poweredOn:
            log("central: powered on, scanning for nimmesh peers")
            central.scanForPeripherals(withServices: [Self.serviceUUID], options: nil)
        case .unauthorized:
            log("central: BLUETOOTH NOT AUTHORIZED — grant Bluetooth to this terminal in System Settings › Privacy & Security › Bluetooth")
        case .poweredOff:
            log("central: Bluetooth is OFF")
        default:
            log("central: state \(central.state.rawValue)")
        }
    }

    func centralManager(_ central: CBCentralManager, didDiscover peripheral: CBPeripheral,
                        advertisementData: [String: Any], rssi RSSI: NSNumber) {
        let id = peripheral.identifier.uuidString
        if peripherals[id] == nil { log("discovered peer \(id.prefix(8)) (rssi \(RSSI))") }
        peripherals[id] = peripheral
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
            peripheral.setNotifyValue(true, for: ch)
            log("linked to peer \(id.prefix(8)) ✓")
            node?.onPeerConnected(peerId: id)
        }
    }

    func peripheral(_ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic, error: Error?) {
        guard let bytes = characteristic.value else { return }
        log("← \(bytes.count)B from \(peripheral.identifier.uuidString.prefix(8))")
        node?.onPacketReceivedFrom(peerId: peripheral.identifier.uuidString, bytes: bytes)
    }

    func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
        let id = peripheral.identifier.uuidString
        peripherals[id] = nil
        writeChars[id] = nil
        log("peer \(id.prefix(8)) disconnected")
        node?.onPeerDisconnected(peerId: id)
        central.scanForPeripherals(withServices: [Self.serviceUUID], options: nil)
    }
}

// MARK: - Peripheral role (advertise → receive writes → notify subscribers)

extension BleMeshRadio: CBPeripheralManagerDelegate {
    func peripheralManagerDidUpdateState(_ peripheral: CBPeripheralManager) {
        guard peripheral.state == .poweredOn else {
            if peripheral.state == .unauthorized {
                log("peripheral: BLUETOOTH NOT AUTHORIZED")
            }
            return
        }
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
        peripheral.startAdvertising([
            CBAdvertisementDataServiceUUIDsKey: [Self.serviceUUID],
            CBAdvertisementDataLocalNameKey: "nimmesh-mac",
        ])
        log("peripheral: advertising the nimmesh service")
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, didReceiveWrite requests: [CBATTRequest]) {
        for req in requests {
            if let bytes = req.value {
                log("← \(bytes.count)B write from \(req.central.identifier.uuidString.prefix(8))")
                node?.onPacketReceivedFrom(peerId: req.central.identifier.uuidString, bytes: bytes)
            }
            peripheral.respond(to: req, withResult: .success)
        }
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, central: CBCentral,
                          didSubscribeTo characteristic: CBCharacteristic) {
        let id = central.identifier.uuidString
        subscribedCentrals[id] = central
        log("peer \(id.prefix(8)) subscribed (they are central to us) ✓")
        node?.onPeerConnected(peerId: id)
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, central: CBCentral,
                          didUnsubscribeFrom characteristic: CBCharacteristic) {
        let id = central.identifier.uuidString
        subscribedCentrals[id] = nil
        node?.onPeerDisconnected(peerId: id)
    }
}
