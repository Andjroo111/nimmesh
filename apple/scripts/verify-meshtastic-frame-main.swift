//
//  verify-meshtastic-frame-main.swift
//
//  Proves MeshtasticFrame byte-exact against hand-computed protobuf, with no hardware and
//  no swift-protobuf dependency. Same pattern as verify-mnemonic-main.swift.
//
//  Top-level code must live in a file named main.swift, so compile it via a copy:
//
//    cp apple/scripts/verify-meshtastic-frame-main.swift /tmp/main.swift && \
//      swiftc shared/MeshtasticFrame.swift /tmp/main.swift -o /tmp/vmf && /tmp/vmf
//
//  The expected frame below was computed BY HAND from meshtastic/protobufs mesh.proto, not
//  captured from this implementation. That is the whole point: a test that only compares the
//  code to itself would pass with the field numbers wrong.
//

import Foundation

var failures = 0

func check(_ name: String, _ condition: Bool, _ detail: @autoclosure () -> String = "") {
    if condition {
        print("  ok    \(name)")
    } else {
        failures += 1
        let d = detail()
        print("  FAIL  \(name)\(d.isEmpty ? "" : " — \(d)")")
    }
}

func hex(_ bytes: [UInt8]) -> String {
    bytes.map { String(format: "%02x", $0) }.joined()
}

print("MeshtasticFrame")

// ---------------------------------------------------------------------------
// 1. Byte exactness against a hand-computed frame.
//
//   ToRadio { packet = 1 }                         tag 0x0A, len 0x17 (23)
//     MeshPacket
//       to        = 2, fixed32  tag 0x15  ffffffff (broadcast)
//       decoded   = 4, bytes    tag 0x22  len 0x09
//         Data
//           portnum = 1, varint tag 0x08  8002      (256, PRIVATE_APP)
//           payload = 2, bytes  tag 0x12  len 0x04  deadbeef
//       id        = 6, fixed32  tag 0x35  44332211 (0x11223344 little-endian)
//       hop_limit = 9, varint   tag 0x48  03
// ---------------------------------------------------------------------------
let expected = "0a1715ffffffff22090880021204deadbeef35443322114803"
let frame = try! MeshtasticFrame.encodeToRadio(
    payload: [0xDE, 0xAD, 0xBE, 0xEF],
    packetId: 0x1122_3344
)
check("encodes byte-exact against hand-computed protobuf",
      hex(frame) == expected,
      "got \(hex(frame)), want \(expected)")

// PRIVATE_APP is 256, which is the first portnum needing a two-byte varint. Getting this
// wrong is silent: the radio would accept the frame and file it under the wrong app.
check("portnum 256 encodes as the two-byte varint 0x80 0x02",
      hex(MeshtasticFrame.varint(256)) == "8002")
check("varint 0 is a single zero byte", hex(MeshtasticFrame.varint(0)) == "00")
check("varint 127 stays one byte", hex(MeshtasticFrame.varint(127)) == "7f")
check("varint 128 rolls to two bytes", hex(MeshtasticFrame.varint(128)) == "8001")
check("fixed32 is little-endian", hex(MeshtasticFrame.fixed32(0x1122_3344)) == "44332211")

// ---------------------------------------------------------------------------
// 2. The single-packet property, which is the entire reason for this transport.
// ---------------------------------------------------------------------------
let realTxSize = 139
let memoTxSize = 205
check("a 139-byte transfer fits one packet",
      (try? MeshtasticFrame.encodeToRadio(
          payload: [UInt8](repeating: 0xAB, count: realTxSize), packetId: 1)) != nil)
check("a 205-byte transfer with a memo also fits",
      (try? MeshtasticFrame.encodeToRadio(
          payload: [UInt8](repeating: 0xAB, count: memoTxSize), packetId: 1)) != nil)

do {
    _ = try MeshtasticFrame.encodeToRadio(
        payload: [UInt8](repeating: 0xAB, count: MeshtasticFrame.maxPayloadBytes + 1),
        packetId: 1)
    check("oversized payload is refused rather than silently truncated", false, "no throw")
} catch let e as MeshtasticFrame.FrameError {
    check("oversized payload is refused rather than silently truncated",
          e == .payloadTooLarge(bytes: MeshtasticFrame.maxPayloadBytes + 1,
                                limit: MeshtasticFrame.maxPayloadBytes))
} catch {
    check("oversized payload is refused rather than silently truncated", false, "\(error)")
}

// ---------------------------------------------------------------------------
// 3. Round trip. Wrap the encoded MeshPacket in a FromRadio (packet = field 2) the way a
//    radio hands it back, then recover the payload.
// ---------------------------------------------------------------------------
func fromRadio(wrapping toRadio: [UInt8]) -> [UInt8] {
    // Strip the ToRadio envelope (tag + length) to get the bare MeshPacket back...
    let packet = try! MeshtasticFrame.firstField(1, wire: .lengthDelimited, in: toRadio)!
    // ...and re-wrap it as FromRadio.packet, which is field 2, not field 1.
    var out: [UInt8] = []
    out += MeshtasticFrame.tag(field: 2, wire: .lengthDelimited)
    out += MeshtasticFrame.varint(UInt64(packet.count))
    out += packet
    return out
}

let tx = (0..<139).map { UInt8($0 & 0xFF) }
let sent = try! MeshtasticFrame.encodeToRadio(payload: tx, packetId: 0xCAFE_BABE)
let received = try! MeshtasticFrame.decodeFromRadio(fromRadio(wrapping: sent))
check("round trip recovers the exact payload", received?.payload == tx,
      "got \(received?.payload.count ?? -1) bytes")
check("round trip reports the private-app portnum",
      received?.portnum == MeshtasticFrame.privateAppPortnum)

// ---------------------------------------------------------------------------
// 4. Traffic that is not ours. The radio delivers everyone's chat and telemetry on the same
//    characteristic, so this must be quiet, not fatal.
// ---------------------------------------------------------------------------
func foreignPacket(portnum: UInt64) -> [UInt8] {
    var data: [UInt8] = []
    data += MeshtasticFrame.tag(field: 1, wire: .varint)
    data += MeshtasticFrame.varint(portnum)
    data += MeshtasticFrame.tag(field: 2, wire: .lengthDelimited)
    data += MeshtasticFrame.varint(5)
    data += Array("hello".utf8)

    var packet: [UInt8] = []
    packet += MeshtasticFrame.tag(field: 4, wire: .lengthDelimited)
    packet += MeshtasticFrame.varint(UInt64(data.count))
    packet += data

    var out: [UInt8] = []
    out += MeshtasticFrame.tag(field: 2, wire: .lengthDelimited)
    out += MeshtasticFrame.varint(UInt64(packet.count))
    out += packet
    return out
}
check("a TEXT_MESSAGE_APP packet is ignored, not an error",
      (try? MeshtasticFrame.decodeFromRadio(foreignPacket(portnum: 1))) == .some(nil))
check("a FromRadio with no packet at all is ignored",
      (try? MeshtasticFrame.decodeFromRadio([0x08, 0x2A])) == .some(nil))

// ---------------------------------------------------------------------------
// 5. Hostile input. A relay hands us bytes from strangers, so the parser must refuse
//    truncation rather than read past the end.
// ---------------------------------------------------------------------------
let truncated = Array(sent.prefix(sent.count - 4))
var threw = false
do { _ = try MeshtasticFrame.decodeFromRadio(fromRadio(wrapping: sent).dropLast(4).map { $0 }) }
catch { threw = true }
check("a truncated frame throws instead of over-reading", threw)

check("a length prefix that overruns the buffer throws",
      { do { _ = try MeshtasticFrame.decodeFromRadio([0x12, 0x7F, 0x01]); return false }
        catch { return true } }())
_ = truncated

print(failures == 0 ? "\nPASS" : "\n\(failures) FAILURE(S)")
exit(failures == 0 ? 0 : 1)
