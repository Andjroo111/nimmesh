import CoreBluetooth
import CryptoKit
import Foundation

// BitchatKit — a minimal, byte-exact implementation of the Bitchat public-chat wire
// protocol (github.com/permissionlesstech/bitchat, The Unlicense / public domain), so a
// nimmesh node can appear as a Bitchat peer and exchange PUBLIC messages with real
// Bitchat apps nearby, fully offline. Scope: signed announce (0x01) + signed public
// message (0x02) on protocol v1. Private (Noise) messages, fragments, and relaying are
// intentionally out of scope — we are an endpoint on their mesh, not a router of it.
//
// Interop-critical details (verified against bitchat main, 2026-07-08):
// • GATT service F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5C, one characteristic
//   A1B2C3D4-E5F6-4A5B-8C9D-0E1F2A3B4C5D (notify+write+writeWithoutResponse+read).
// • Header v1 (14 B, big-endian): ver(1)=1 ‖ type(1) ‖ ttl(1) ‖ timestampMs(8) ‖
//   flags(1) ‖ payloadLen(2); then senderID(8) ‖ [recipient(8)] ‖ payload ‖ [sig(64)] ‖
//   PKCS#7 pad to the smallest of [256,512,1024,2048] ≥ size+16 (none if pad>255).
// • peerID(8) = SHA-256(Curve25519 noise pubkey)[0..8]; a second Ed25519 key signs.
// • Signatures cover the canonical re-encoding with signature ABSENT, ttl = 0, padded.
// • An announce (TLV: 0x01 nickname, 0x02 noisePub, 0x03 signingPub — signed, fresh
//   ≤ 900 s) must be seen before a peer's public messages are displayed.

// MARK: - Wire constants

enum BitchatWire {
    static let serviceUUID = CBUUID(string: "F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5C")
    static let characteristicUUID = CBUUID(string: "A1B2C3D4-E5F6-4A5B-8C9D-0E1F2A3B4C5D")
    static let version: UInt8 = 1
    static let headerSize = 14
    static let peerIDSize = 8
    static let signatureSize = 64
    static let defaultTTL: UInt8 = 7
    static let typeAnnounce: UInt8 = 0x01
    static let typeMessage: UInt8 = 0x02
    static let flagHasRecipient: UInt8 = 0x01
    static let flagHasSignature: UInt8 = 0x02
    static let flagIsCompressed: UInt8 = 0x04
    static let announceMaxAgeMs: UInt64 = 900_000
    static let messageMaxAgeMs: UInt64 = 6 * 3600_000
}

// MARK: - PKCS#7 padding (bitchat MessagePadding semantics)

enum BitchatPadding {
    static let blocks = [256, 512, 1024, 2048]

    /// The bitchat rule: pad to the smallest block ≥ dataSize+16; if the needed pad
    /// exceeds 255 (PKCS#7 limit) or the data is past the largest block, add none.
    static func pad(_ data: Data) -> Data {
        guard let block = blocks.first(where: { $0 >= data.count + 16 }) else { return data }
        let padLen = block - data.count
        guard padLen > 0, padLen <= 255 else { return data }
        var out = data
        out.append(Data(repeating: UInt8(padLen), count: padLen))
        return out
    }
}

// MARK: - Packet

struct BitchatPacket {
    var type: UInt8
    var ttl: UInt8
    var timestampMs: UInt64
    var senderID: Data          // exactly 8 bytes
    var recipientID: Data?      // nil for broadcast
    var payload: Data
    var signature: Data?        // 64 bytes when present

    /// Serialize. `padded` mirrors bitchat's default (wire AND signing both pad).
    func encode(padded: Bool = true) -> Data {
        var flags: UInt8 = 0
        if recipientID != nil { flags |= BitchatWire.flagHasRecipient }
        if signature != nil { flags |= BitchatWire.flagHasSignature }
        var out = Data(capacity: 64 + payload.count)
        out.append(BitchatWire.version)
        out.append(type)
        out.append(ttl)
        var ts = timestampMs.bigEndian
        withUnsafeBytes(of: &ts) { out.append(contentsOf: $0) }
        out.append(flags)
        var plen = UInt16(payload.count).bigEndian
        withUnsafeBytes(of: &plen) { out.append(contentsOf: $0) }
        out.append(senderID.prefix(8) + Data(repeating: 0, count: max(0, 8 - senderID.count)))
        if let r = recipientID {
            out.append(r.prefix(8) + Data(repeating: 0, count: max(0, 8 - r.count)))
        }
        out.append(payload)
        if let s = signature { out.append(s) }
        return padded ? BitchatPadding.pad(out) : out
    }

    /// The canonical bytes a signature covers: signature absent, ttl = 0, padded.
    func signingBytes() -> Data {
        var c = self
        c.signature = nil
        c.ttl = 0
        return c.encode(padded: true)
    }

    /// Parse a v1 packet, reading only declared lengths (trailing pad tolerated).
    /// Compressed (0x04) payloads are refused here — a public one-liner never compresses.
    static func decode(_ data: Data) -> BitchatPacket? {
        let d = Data(data) // normalize indices
        guard d.count >= BitchatWire.headerSize + BitchatWire.peerIDSize else { return nil }
        guard d[0] == BitchatWire.version else { return nil }
        let type = d[1]
        let ttl = d[2]
        let ts = d[3..<11].reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
        let flags = d[11]
        if flags & BitchatWire.flagIsCompressed != 0 { return nil }
        let plen = Int(d[12]) << 8 | Int(d[13])
        var i = BitchatWire.headerSize
        func take(_ n: Int) -> Data? {
            guard d.count >= i + n else { return nil }
            defer { i += n }
            return d.subdata(in: i..<i + n)
        }
        guard let sender = take(BitchatWire.peerIDSize) else { return nil }
        var recipient: Data?
        if flags & BitchatWire.flagHasRecipient != 0 {
            guard let r = take(BitchatWire.peerIDSize) else { return nil }
            recipient = r
        }
        guard let payload = take(plen) else { return nil }
        var signature: Data?
        if flags & BitchatWire.flagHasSignature != 0 {
            guard let s = take(BitchatWire.signatureSize) else { return nil }
            signature = s
        }
        return BitchatPacket(
            type: type, ttl: ttl, timestampMs: ts, senderID: sender,
            recipientID: recipient, payload: payload, signature: signature)
    }
}

// MARK: - Announce TLV

struct BitchatAnnounce {
    var nickname: String
    var noisePublicKey: Data    // 32
    var signingPublicKey: Data  // 32

    func encode() -> Data {
        var out = Data()
        let nick = Data(nickname.utf8).prefix(255)
        out.append(contentsOf: [0x01, UInt8(nick.count)]); out.append(nick)
        out.append(contentsOf: [0x02, 32]); out.append(noisePublicKey)
        out.append(contentsOf: [0x03, 32]); out.append(signingPublicKey)
        return out
    }

    static func decode(_ data: Data) -> BitchatAnnounce? {
        let d = Data(data)
        var i = 0
        var nick: String?, noise: Data?, signing: Data?
        while i + 2 <= d.count {
            let t = d[i], l = Int(d[i + 1])
            i += 2
            guard i + l <= d.count else { return nil }
            let v = d.subdata(in: i..<i + l)
            i += l
            switch t {
            case 0x01: nick = String(data: v, encoding: .utf8)
            case 0x02: noise = v
            case 0x03: signing = v
            default: break // unknown TLVs skipped (forward-compatible)
            }
        }
        guard let n = nick, let np = noise, np.count == 32, let sp = signing, sp.count == 32
        else { return nil }
        return BitchatAnnounce(nickname: n, noisePublicKey: np, signingPublicKey: sp)
    }
}

// MARK: - Identity

struct BitchatIdentity {
    let noiseKey: Curve25519.KeyAgreement.PrivateKey
    let signingKey: Curve25519.Signing.PrivateKey

    /// peerID = first 8 bytes of SHA-256(noise public key).
    var peerID: Data {
        Data(SHA256.hash(data: noiseKey.publicKey.rawRepresentation).prefix(8))
    }

    /// Load-or-create, persisted in UserDefaults. These keys are a CHAT TRANSPORT
    /// identity only — never the wallet, never money-path.
    static func loadOrCreate(defaults: UserDefaults = .standard) -> BitchatIdentity {
        let nk = "nimmesh.bitchat.noiseKey", sk = "nimmesh.bitchat.signingKey"
        if let nd = defaults.data(forKey: nk), let sd = defaults.data(forKey: sk),
           let n = try? Curve25519.KeyAgreement.PrivateKey(rawRepresentation: nd),
           let s = try? Curve25519.Signing.PrivateKey(rawRepresentation: sd) {
            return BitchatIdentity(noiseKey: n, signingKey: s)
        }
        let id = BitchatIdentity(
            noiseKey: Curve25519.KeyAgreement.PrivateKey(),
            signingKey: Curve25519.Signing.PrivateKey())
        defaults.set(id.noiseKey.rawRepresentation, forKey: nk)
        defaults.set(id.signingKey.rawRepresentation, forKey: sk)
        return id
    }

    func sign(_ packet: BitchatPacket) -> BitchatPacket {
        var p = packet
        p.signature = try? signingKey.signature(for: packet.signingBytes())
        return p
    }
}

// MARK: - The BLE link

/// A dual-role CoreBluetooth endpoint on the Bitchat mesh: advertises their service,
/// scans for their peers, announces our identity, sends signed public messages, and
/// surfaces verified inbound ones. Runs its OWN managers — zero contact with the
/// nimmesh radio. Not a relay: we never re-flood their packets.
final class BitchatLink: NSObject {
    private let identity: BitchatIdentity
    private let nickname: String
    private var central: CBCentralManager!
    private var peripheralMgr: CBPeripheralManager!
    private var characteristic: CBMutableCharacteristic?
    private var peripherals: [UUID: CBPeripheral] = [:]
    private var remoteChars: [UUID: CBCharacteristic] = [:]
    private var announceTimer: DispatchSourceTimer?
    /// Verified peers: peerID → (nickname, Ed25519 signing key).
    private var peers: [Data: (nickname: String, signingKey: Curve25519.Signing.PublicKey)] = [:]
    private var seen = Set<Data>() // dedup by senderID ‖ timestamp
    private let queue = DispatchQueue(label: "bitchat.link")

    /// A verified public message arrived (nickname, text, sender wall-clock ms).
    var onMessage: ((String, String, UInt64) -> Void)?
    /// A peer announced (nickname) — first time only.
    var onPeer: ((String) -> Void)?
    var onLog: ((String) -> Void)?

    init(nickname: String) {
        self.identity = BitchatIdentity.loadOrCreate()
        self.nickname = nickname
        super.init()
        central = CBCentralManager(delegate: self, queue: queue)
        peripheralMgr = CBPeripheralManager(delegate: self, queue: queue)
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now() + 2, repeating: 20.0)
        t.setEventHandler { [weak self] in self?.sendAnnounce() }
        t.resume()
        announceTimer = t
    }

    func stop() {
        announceTimer?.cancel()
        if central.isScanning { central.stopScan() }
        peripheralMgr.stopAdvertising()
    }

    // MARK: outbound

    func sendAnnounce() {
        let ann = BitchatAnnounce(
            nickname: nickname,
            noisePublicKey: identity.noiseKey.publicKey.rawRepresentation,
            signingPublicKey: identity.signingKey.publicKey.rawRepresentation)
        let packet = identity.sign(BitchatPacket(
            type: BitchatWire.typeAnnounce, ttl: BitchatWire.defaultTTL,
            timestampMs: UInt64(Date().timeIntervalSince1970 * 1000),
            senderID: identity.peerID, recipientID: nil,
            payload: ann.encode(), signature: nil))
        broadcast(packet.encode())
    }

    /// Send a public chat message every Bitchat app in range will display.
    func sendPublic(_ text: String) {
        let packet = identity.sign(BitchatPacket(
            type: BitchatWire.typeMessage, ttl: BitchatWire.defaultTTL,
            timestampMs: UInt64(Date().timeIntervalSince1970 * 1000),
            senderID: identity.peerID, recipientID: nil,
            payload: Data(text.utf8), signature: nil))
        broadcast(packet.encode())
    }

    private func broadcast(_ bytes: Data) {
        if let ch = characteristic {
            _ = peripheralMgr.updateValue(bytes, for: ch, onSubscribedCentrals: nil)
        }
        for (uuid, p) in peripherals {
            guard let ch = remoteChars[uuid], p.state == .connected else { continue }
            let noRsp = p.maximumWriteValueLength(for: .withoutResponse)
            if bytes.count <= noRsp {
                p.writeValue(bytes, for: ch, type: .withoutResponse)
            } else if bytes.count <= p.maximumWriteValueLength(for: .withResponse) {
                p.writeValue(bytes, for: ch, type: .withResponse)
            }
        }
    }

    // MARK: inbound

    private func handleInbound(_ data: Data) {
        guard let packet = BitchatPacket.decode(data) else { return }
        if packet.senderID == identity.peerID { return } // self-echo
        let key = packet.senderID + withUnsafeBytes(of: packet.timestampMs.bigEndian) { Data($0) }
        if seen.contains(key) { return }
        seen.insert(key)
        if seen.count > 2000 { seen.removeAll() } // crude bound; dedup window resets

        switch packet.type {
        case BitchatWire.typeAnnounce:
            guard let ann = BitchatAnnounce.decode(packet.payload) else { return }
            // peerID must be bound to the announced noise key…
            guard Data(SHA256.hash(data: ann.noisePublicKey).prefix(8)) == packet.senderID
            else { return }
            // …and the packet signed by the announced Ed25519 key.
            guard let sig = packet.signature,
                  let vk = try? Curve25519.Signing.PublicKey(rawRepresentation: ann.signingPublicKey),
                  vk.isValidSignature(sig, for: packet.signingBytes())
            else { return }
            let isNew = peers[packet.senderID] == nil
            peers[packet.senderID] = (ann.nickname, vk)
            if isNew { onPeer?(ann.nickname) }
        case BitchatWire.typeMessage:
            // Bitchat's gate: only display messages verifiable against a known announce.
            guard let peer = peers[packet.senderID], let sig = packet.signature,
                  peer.signingKey.isValidSignature(sig, for: packet.signingBytes()),
                  let text = String(data: packet.payload, encoding: .utf8)
            else { return }
            onMessage?(peer.nickname, text, packet.timestampMs)
        default:
            break // fragments / noise / everything else: out of scope, never relayed
        }
    }
}

extension BitchatLink: CBCentralManagerDelegate, CBPeripheralDelegate {
    func centralManagerDidUpdateState(_ c: CBCentralManager) {
        if c.state == .poweredOn {
            c.scanForPeripherals(withServices: [BitchatWire.serviceUUID],
                                 options: [CBCentralManagerScanOptionAllowDuplicatesKey: false])
            onLog?("bitchat: scanning for peers")
        }
    }

    func centralManager(_ c: CBCentralManager, didDiscover p: CBPeripheral,
                        advertisementData: [String: Any], rssi: NSNumber) {
        guard peripherals[p.identifier] == nil else { return }
        peripherals[p.identifier] = p
        p.delegate = self
        c.connect(p, options: nil)
    }

    func centralManager(_ c: CBCentralManager, didConnect p: CBPeripheral) {
        p.discoverServices([BitchatWire.serviceUUID])
    }

    func centralManager(_ c: CBCentralManager, didDisconnectPeripheral p: CBPeripheral, error: Error?) {
        peripherals[p.identifier] = nil
        remoteChars[p.identifier] = nil
    }

    func peripheral(_ p: CBPeripheral, didDiscoverServices error: Error?) {
        for s in p.services ?? [] where s.uuid == BitchatWire.serviceUUID {
            p.discoverCharacteristics([BitchatWire.characteristicUUID], for: s)
        }
    }

    func peripheral(_ p: CBPeripheral, didDiscoverCharacteristicsFor s: CBService, error: Error?) {
        for ch in s.characteristics ?? [] where ch.uuid == BitchatWire.characteristicUUID {
            remoteChars[p.identifier] = ch
            p.setNotifyValue(true, for: ch)
            onLog?("bitchat: linked to a peer — announcing")
            sendAnnounce()
        }
    }

    func peripheral(_ p: CBPeripheral, didUpdateValueFor ch: CBCharacteristic, error: Error?) {
        if let v = ch.value { handleInbound(v) }
    }
}

extension BitchatLink: CBPeripheralManagerDelegate {
    func peripheralManagerDidUpdateState(_ pm: CBPeripheralManager) {
        guard pm.state == .poweredOn else { return }
        let ch = CBMutableCharacteristic(
            type: BitchatWire.characteristicUUID,
            properties: [.notify, .write, .writeWithoutResponse, .read],
            value: nil, permissions: [.readable, .writeable])
        let service = CBMutableService(type: BitchatWire.serviceUUID, primary: true)
        service.characteristics = [ch]
        pm.add(service)
        characteristic = ch
        pm.startAdvertising([CBAdvertisementDataServiceUUIDsKey: [BitchatWire.serviceUUID]])
        onLog?("bitchat: advertising their service")
    }

    func peripheralManager(_ pm: CBPeripheralManager, didReceiveWrite requests: [CBATTRequest]) {
        for r in requests {
            if let v = r.value { handleInbound(v) }
            if r.characteristic.properties.contains(.write) { pm.respond(to: r, withResult: .success) }
        }
    }

    func peripheralManager(_ pm: CBPeripheralManager, central: CBCentral,
                           didSubscribeTo characteristic: CBCharacteristic) {
        onLog?("bitchat: a peer subscribed — announcing")
        sendAnnounce()
    }
}

// MARK: - Self-test (run at startup behind a flag; no test runner in this build chain)

enum BitchatSelfTest {
    /// Pure round-trip + canonicalization checks. Empty result = pass.
    static func run() -> [String] {
        var fails: [String] = []
        let id = BitchatIdentity(
            noiseKey: Curve25519.KeyAgreement.PrivateKey(),
            signingKey: Curve25519.Signing.PrivateKey())

        // Padding: block choice per the bitchat rule. Note the 241 case: its block is 512
        // but the needed pad (271) exceeds the one-byte PKCS#7 limit → ships UNPADDED.
        if BitchatPadding.pad(Data(count: 100)).count != 256 { fails.append("pad(100)≠256") }
        if BitchatPadding.pad(Data(count: 240)).count != 256 { fails.append("pad(240)≠256") }
        if BitchatPadding.pad(Data(count: 241)).count != 241 { fails.append("pad(241) padded") }
        if BitchatPadding.pad(Data(count: 400)).count != 512 { fails.append("pad(400)≠512") }
        if BitchatPadding.pad(Data(count: 500)).count != 500 { fails.append("pad(500) padded") }
        if BitchatPadding.pad(Data(count: 2200)).count != 2200 { fails.append("pad(2200) added") }

        // Packet round-trip through pad + decode (declared lengths ignore trailing pad).
        let msg = id.sign(BitchatPacket(
            type: BitchatWire.typeMessage, ttl: 7, timestampMs: 1_720_000_000_123,
            senderID: id.peerID, recipientID: nil,
            payload: Data("gm bitchat".utf8), signature: nil))
        guard let back = BitchatPacket.decode(msg.encode()) else {
            fails.append("decode(encode) nil"); return fails
        }
        if back.timestampMs != 1_720_000_000_123 { fails.append("timestamp mangled") }
        if back.senderID != id.peerID { fails.append("senderID mangled") }
        if String(data: back.payload, encoding: .utf8) != "gm bitchat" { fails.append("payload mangled") }

        // Signature verifies over the canonical (ttl=0, sig-less, padded) bytes, and the
        // canonicalization is ttl-independent (a relay decrementing ttl must not break it).
        guard let sig = back.signature else { fails.append("signature missing"); return fails }
        var relayed = back
        relayed.ttl = 3
        if !id.signingKey.publicKey.isValidSignature(sig, for: relayed.signingBytes()) {
            fails.append("signature not ttl-independent")
        }

        // Announce round-trip + the full inbound verification gates.
        let ann = BitchatAnnounce(
            nickname: "selftest",
            noisePublicKey: id.noiseKey.publicKey.rawRepresentation,
            signingPublicKey: id.signingKey.publicKey.rawRepresentation)
        guard let annBack = BitchatAnnounce.decode(ann.encode()) else {
            fails.append("announce decode nil"); return fails
        }
        if annBack.nickname != "selftest" { fails.append("announce nickname mangled") }
        if Data(SHA256.hash(data: annBack.noisePublicKey).prefix(8)) != id.peerID {
            fails.append("peerID binding broken")
        }

        // A tampered payload must fail verification.
        var tampered = back
        tampered.payload = Data("gm bitchat!".utf8)
        if id.signingKey.publicKey.isValidSignature(sig, for: tampered.signingBytes()) {
            fails.append("tampered payload verified")
        }
        return fails
    }
}
