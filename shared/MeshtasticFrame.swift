//
//  MeshtasticFrame.swift
//  nimmesh
//
//  Wraps a signed Nimiq transaction into a Meshtastic packet, and pulls one back out.
//
//  WHY THIS EXISTS
//  Bluetooth gives nimmesh roughly 30 m. LoRa gives it kilometres, and there is already a
//  60-node community Meshtastic network across the metro that will relay for us. A radio
//  at each end and their nodes supply every hop in between. See docs/TRANSPORTS.md.
//
//  WHY IT IS HAND-ROLLED
//  Meshtastic speaks protobuf over its BLE client API. We need exactly two shapes out of a
//  large schema, so this hand-encodes them rather than pulling swift-protobuf and the whole
//  Meshtastic .proto set into the app. That matches how the rest of this codebase treats
//  wire formats (Nimiq's tx serializer, CashlinkCodec, BitchatKit are all hand-rolled and
//  proven byte-exact by a swiftc harness). Field numbers are transcribed from
//  meshtastic/protobufs mesh.proto and are asserted in verify-meshtastic-frame-main.swift.
//
//  THE ONE PROPERTY THAT MAKES THIS WORK
//  A signed basic transfer is 139 bytes and a Meshtastic packet carries 237. It rides in a
//  SINGLE packet: no chunking, no reassembly, no partial-delivery state. The Bitcoin
//  equivalents have to chunk, partly because their transactions are bigger and partly
//  because they ship hex, which doubles the byte count. We send raw bytes on PRIVATE_APP.
//  nimmesh-core's transport_mtu_tests keeps that invariant honest in CI.
//

import Foundation

public enum MeshtasticFrame {

    // MARK: - Constants

    /// Meshtastic's BLE GATT service and the three characteristics its client API exposes.
    /// https://meshtastic.org/docs/development/device/client-api/
    public enum BLE {
        public static let service   = "6BA1B218-15A8-461F-9FA8-5DCAE273EAFD"
        /// Write a ToRadio protobuf here to transmit.
        public static let toRadio   = "F75C76D2-129E-4DAD-A1DD-7866124401E7"
        /// Read to drain the next inbound FromRadio packet.
        public static let fromRadio = "2C55E69E-4993-11ED-B878-0242AC120002"
        /// Notifies with the current inbound packet number; read fromRadio until caught up.
        public static let fromNum   = "ED9DA18C-A800-4F66-A670-AA7547E34453"
    }

    /// Meshtastic reserves 256-511 for private applications. 256 needs no protobuf rebuild
    /// and no registration with the Meshtastic project.
    public static let privateAppPortnum: UInt64 = 256

    /// Meshtastic's broadcast destination.
    public static let broadcastAddress: UInt32 = 0xFFFF_FFFF

    /// What one packet can carry. 237 usable of a 256-byte packet, minus Meshtastic's own
    /// 16-byte header. A 139-byte transfer clears this with room to spare; a 205-byte
    /// transfer carrying a memo still fits.
    public static let maxPayloadBytes = 237 - 16

    /// the local community mesh publishes a hop limit of 3. Matching it keeps us a good citizen on
    /// a network built for off-grid messaging rather than for our traffic.
    public static let defaultHopLimit: UInt64 = 3

    // MARK: - Errors

    public enum FrameError: Error, Equatable {
        /// The payload would need fragmenting, which would forfeit the single-packet property.
        case payloadTooLarge(bytes: Int, limit: Int)
        case malformed(String)
    }

    // MARK: - Encode

    /// Build a `ToRadio` protobuf carrying `payload` on the private application port.
    ///
    /// - Parameters:
    ///   - payload: raw transaction bytes. RAW, never hex: hex doubles the size and is
    ///     precisely why the Bitcoin bridges have to chunk.
    ///   - packetId: Meshtastic's packet id. Caller supplies it so this stays deterministic
    ///     and therefore testable.
    public static func encodeToRadio(
        payload: [UInt8],
        packetId: UInt32,
        to: UInt32 = broadcastAddress,
        hopLimit: UInt64 = defaultHopLimit
    ) throws -> [UInt8] {
        guard payload.count <= maxPayloadBytes else {
            throw FrameError.payloadTooLarge(bytes: payload.count, limit: maxPayloadBytes)
        }

        // Data { portnum = 1 (varint), payload = 2 (bytes) }
        var data: [UInt8] = []
        data += tag(field: 1, wire: .varint)
        data += varint(privateAppPortnum)
        data += tag(field: 2, wire: .lengthDelimited)
        data += varint(UInt64(payload.count))
        data += payload

        // MeshPacket { to = 2 (fixed32), decoded = 4 (Data), id = 6 (fixed32), hop_limit = 9 }
        var packet: [UInt8] = []
        packet += tag(field: 2, wire: .fixed32)
        packet += fixed32(to)
        packet += tag(field: 4, wire: .lengthDelimited)
        packet += varint(UInt64(data.count))
        packet += data
        packet += tag(field: 6, wire: .fixed32)
        packet += fixed32(packetId)
        packet += tag(field: 9, wire: .varint)
        packet += varint(hopLimit)
        // want_ack (field 10) is left unset. Protobuf treats an absent bool as false, and a
        // mesh relay cannot meaningfully ack a payment anyway; the chain is the receipt.

        // ToRadio { packet = 1 (MeshPacket) }
        var frame: [UInt8] = []
        frame += tag(field: 1, wire: .lengthDelimited)
        frame += varint(UInt64(packet.count))
        frame += packet
        return frame
    }

    // MARK: - Decode

    /// A payload recovered from an inbound `FromRadio`.
    public struct Inbound: Equatable {
        public let portnum: UInt64
        public let payload: [UInt8]
        public let from: UInt32
    }

    /// Pull a private-application payload out of a `FromRadio` protobuf.
    ///
    /// Returns nil, rather than throwing, for a well-formed frame that simply is not ours:
    /// the radio delivers position beacons, telemetry and everyone's chat traffic on the
    /// same characteristic, and none of that is an error condition.
    public static func decodeFromRadio(_ bytes: [UInt8]) throws -> Inbound? {
        // FromRadio { id = 1, packet = 2 (MeshPacket) }
        guard let packetBytes = try firstField(2, wire: .lengthDelimited, in: bytes) else {
            return nil
        }
        // MeshPacket { from = 1 (fixed32), decoded = 4 (Data) }
        guard let dataBytes = try firstField(4, wire: .lengthDelimited, in: packetBytes) else {
            // Most likely `encrypted` (field 5): a packet on a channel whose key we lack.
            return nil
        }
        let from = try firstFixed32(1, in: packetBytes) ?? 0

        // Data { portnum = 1 (varint), payload = 2 (bytes) }
        guard let portnum = try firstVarint(1, in: dataBytes) else { return nil }
        guard portnum == privateAppPortnum else { return nil }
        guard let payload = try firstField(2, wire: .lengthDelimited, in: dataBytes) else {
            throw FrameError.malformed("private-app packet with no payload")
        }
        return Inbound(portnum: portnum, payload: payload, from: from)
    }

    // MARK: - Minimal protobuf primitives

    enum WireType: UInt64 {
        case varint = 0
        case fixed64 = 1
        case lengthDelimited = 2
        case fixed32 = 5
    }

    static func tag(field: UInt64, wire: WireType) -> [UInt8] {
        varint((field << 3) | wire.rawValue)
    }

    static func varint(_ value: UInt64) -> [UInt8] {
        var v = value
        var out: [UInt8] = []
        repeat {
            var byte = UInt8(v & 0x7F)
            v >>= 7
            if v != 0 { byte |= 0x80 }
            out.append(byte)
        } while v != 0
        return out
    }

    static func fixed32(_ value: UInt32) -> [UInt8] {
        [UInt8(value & 0xFF),
         UInt8((value >> 8) & 0xFF),
         UInt8((value >> 16) & 0xFF),
         UInt8((value >> 24) & 0xFF)]
    }

    /// Walk a message once, returning the first occurrence of `field`.
    /// Skips every other field, whatever its wire type, so unknown fields never break us.
    static func scan(_ bytes: [UInt8], _ visit: (UInt64, WireType, ArraySlice<UInt8>) -> Bool) throws {
        var i = bytes.startIndex
        while i < bytes.endIndex {
            guard let (key, afterKey) = readVarint(bytes, i) else {
                throw FrameError.malformed("truncated field key")
            }
            guard let wire = WireType(rawValue: key & 0x07) else {
                throw FrameError.malformed("unsupported wire type \(key & 0x07)")
            }
            let field = key >> 3
            i = afterKey

            switch wire {
            case .varint:
                guard let (_, next) = readVarint(bytes, i) else {
                    throw FrameError.malformed("truncated varint")
                }
                if !visit(field, wire, bytes[i..<next]) { return }
                i = next
            case .fixed32, .fixed64:
                let width = wire == .fixed32 ? 4 : 8
                guard bytes.distance(from: i, to: bytes.endIndex) >= width else {
                    throw FrameError.malformed("truncated fixed field")
                }
                let next = bytes.index(i, offsetBy: width)
                if !visit(field, wire, bytes[i..<next]) { return }
                i = next
            case .lengthDelimited:
                guard let (len, afterLen) = readVarint(bytes, i) else {
                    throw FrameError.malformed("truncated length prefix")
                }
                guard bytes.distance(from: afterLen, to: bytes.endIndex) >= Int(len) else {
                    throw FrameError.malformed("length prefix overruns the buffer")
                }
                let next = bytes.index(afterLen, offsetBy: Int(len))
                if !visit(field, wire, bytes[afterLen..<next]) { return }
                i = next
            }
        }
    }

    static func readVarint(_ bytes: [UInt8], _ start: Int) -> (UInt64, Int)? {
        var value: UInt64 = 0
        var shift: UInt64 = 0
        var i = start
        while i < bytes.endIndex {
            let byte = bytes[i]
            // 10 groups of 7 bits overflows 64; refuse rather than wrap.
            if shift > 63 { return nil }
            value |= UInt64(byte & 0x7F) << shift
            i += 1
            if byte & 0x80 == 0 { return (value, i) }
            shift += 7
        }
        return nil
    }

    static func firstField(_ field: UInt64, wire: WireType, in bytes: [UInt8]) throws -> [UInt8]? {
        var found: [UInt8]?
        try scan(bytes) { f, w, slice in
            if f == field && w == wire { found = Array(slice); return false }
            return true
        }
        return found
    }

    static func firstVarint(_ field: UInt64, in bytes: [UInt8]) throws -> UInt64? {
        guard let raw = try firstField(field, wire: .varint, in: bytes) else { return nil }
        guard let (value, _) = readVarint(raw, raw.startIndex) else {
            throw FrameError.malformed("bad varint in field \(field)")
        }
        return value
    }

    static func firstFixed32(_ field: UInt64, in bytes: [UInt8]) throws -> UInt32? {
        guard let raw = try firstField(field, wire: .fixed32, in: bytes), raw.count == 4 else {
            return nil
        }
        return UInt32(raw[0]) | (UInt32(raw[1]) << 8) | (UInt32(raw[2]) << 16) | (UInt32(raw[3]) << 24)
    }
}
