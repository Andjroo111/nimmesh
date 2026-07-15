import Foundation

/// The hub cashlink FRAGMENT codec — pure bytes, no keys, no IO. Kept free of
/// NimmeshCore/UIKit so a bare `swiftc` harness can prove it byte-exact against
/// fixtures (the Mnemonic.swift discipline). Layout per nimiq/hub `Cashlink.render()`
/// (corroborated by the MIT nimiq/cashlink-generator):
/// `seed(32) ‖ value u64 BE (luna) ‖ [msgLen u8 ‖ msg UTF-8]`, base64url UNPADDED.
enum CashlinkCodec {
    /// RFC 4648 base64url, unpadded — what the current hub emits and parses (`+`→`-`,
    /// `/`→`_`, padding stripped; the legacy `.` padding is accepted on decode only).
    static func base64UrlHub(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    static func encodeFragment(seed: Data, amountLuna: UInt64, message: String) -> String {
        var buf = Data()
        buf.append(seed) // 32-byte Ed25519 private key seed — the account key itself
        var v = amountLuna.bigEndian
        withUnsafeBytes(of: &v) { buf.append(contentsOf: $0) }
        let msg = Data(message.utf8).prefix(255)
        if !msg.isEmpty {
            buf.append(UInt8(msg.count))
            buf.append(msg)
        }
        return base64UrlHub(buf)
    }

    /// Byte-exact inverse of `encodeFragment`. Returns nil on anything malformed —
    /// bad base64, short buffer, zero amount — so callers can fall back honestly.
    static func decodeFragment(_ fragment: String) -> (seed: Data, amountLuna: UInt64, message: String)? {
        var b64 = fragment
            .replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
            .replacingOccurrences(of: ".", with: "=")
        while b64.count % 4 != 0 { b64 += "=" }
        guard let data = Data(base64Encoded: b64), data.count >= 40 else { return nil }
        let seed = data.subdata(in: 0..<32)
        let value = data.subdata(in: 32..<40).reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
        guard value > 0 else { return nil }
        var message = ""
        if data.count > 41 {
            let end = min(41 + Int(data[40]), data.count)
            message = String(data: data.subdata(in: 41..<end), encoding: .utf8) ?? ""
        }
        return (seed, value, message)
    }

    /// The fragment out of a full hub URL (anything after the first `#`); a bare
    /// fragment passes through untouched.
    static func fragment(fromUrl url: String) -> String? {
        guard let hash = url.firstIndex(of: "#") else {
            return url.contains("/") ? nil : (url.isEmpty ? nil : url)
        }
        let frag = String(url[url.index(after: hash)...])
        return frag.isEmpty ? nil : frag
    }
}
