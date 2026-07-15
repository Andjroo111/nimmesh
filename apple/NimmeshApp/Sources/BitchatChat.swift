import Foundation

/// Phase 2 of the Bitchat interop (#233): the APP joins the real Bitchat mesh directly —
/// no Mac in the room. The phone is an ENDPOINT on both networks: it hears both, sends
/// to both, and never bridges between them (re-flooding across meshes is the mac-node
/// bridge's job; doing it here too would double every message).
///
/// Battery honesty: this runs a SECOND pair of CoreBluetooth managers next to the
/// nimmesh radio (two advertisers, two scanners). The cost on real hardware is
/// unmeasured, so the toggle defaults OFF and lives in Settings; flipping it on runs
/// the BitchatKit wire self-test first — a failed kit never touches the air.
final class BitchatChat {
    static let shared = BitchatChat()
    private static let enabledKey = "nimmesh.bitchat.enabled"
    private static let nickKey = "nimmesh.bitchat.nick"
    private var link: BitchatLink?
    private var log: [[String: Any]] = [] // inbound only; own sends live in the nimmesh log
    private var seq = 0
    private let lock = NSLock()

    var enabled: Bool { UserDefaults.standard.bool(forKey: Self.enabledKey) }
    var active: Bool { link != nil }

    /// Flip the toggle. Enabling gates on the self-test; the announce nickname is
    /// fixed for the session (Bitchat learns names from announces, not messages).
    func setEnabled(_ on: Bool, nickname: String) -> (Bool, String) {
        if !on {
            UserDefaults.standard.set(false, forKey: Self.enabledKey)
            link?.stop()
            link = nil
            return (true, "off")
        }
        let nick = nickname.isEmpty ? "Anon" : String(nickname.prefix(24))
        UserDefaults.standard.set(nick, forKey: Self.nickKey)
        let result = start(nickname: nick)
        UserDefaults.standard.set(result.0, forKey: Self.enabledKey)
        return result
    }

    /// Boot path: resume a previously-enabled link (called from the chat bridge — cheap
    /// no-op guard, so polling it is free).
    func startIfEnabled() {
        guard enabled, link == nil else { return }
        _ = start(nickname: UserDefaults.standard.string(forKey: Self.nickKey) ?? "Anon")
    }

    private func start(nickname: String) -> (Bool, String) {
        let fails = BitchatSelfTest.run()
        guard fails.isEmpty else {
            return (false, "self-test failed: \(fails.joined(separator: ", "))")
        }
        let l = BitchatLink(nickname: nickname)
        l.onMessage = { [weak self] nick, text, ts in
            self?.append(nickname: nick, text: text, timestampMs: ts)
        }
        link = l
        return (true, "on")
    }

    /// Fan a chat line out to the Bitchat mesh (the nimmesh flood already happened).
    func send(_ text: String) {
        link?.sendPublic(text)
    }

    private func append(nickname: String, text: String, timestampMs: UInt64) {
        lock.lock(); defer { lock.unlock() }
        seq += 1
        log.append([
            "id": "bc-\(seq)", "nickname": nickname, "text": text,
            "timestamp": Double(timestampMs), "mine": false, "net": "bitchat",
        ])
        if log.count > 200 { log.removeFirst(log.count - 200) }
    }

    var messages: [[String: Any]] {
        lock.lock(); defer { lock.unlock() }
        return log
    }
}

extension Bridge {
    /// The whole chat family behind ONE switch line in WebHostView (the 800-line guard):
    /// mesh chat send/read (moved verbatim from the old cases) + the Bitchat toggle.
    func chatHandle(method: String, args: Any?) -> (Bool, Any) {
        switch method {
        case "sendChat":
            // Public mesh chat: text + chosen nickname flood to everyone nearby (0x50).
            // Nothing but text crosses — no keys, no addresses, not money-path.
            let a = args as? [String: Any] ?? [:]
            let text = (a["text"] as? String ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            var nick = (a["nickname"] as? String ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if nick.isEmpty { nick = "Anon" }
            guard !text.isEmpty else { return (false, "empty message") }
            let ok = node.sendChat(
                nickname: String(nick.prefix(24)), text: text,
                timestampMs: UInt64(Date().timeIntervalSince1970 * 1000))
            // Same deliberate tap fans out to Bitchat when the toggle is on. Their mesh
            // shows the ANNOUNCE nickname; our log's "mine" copy is the nimmesh one.
            if ok { BitchatChat.shared.send(text) }
            var payload: [String: Any] = ["ok": ok]
            if !ok { payload["reason"] = "too long" }
            return (ok, payload)
        case "chatMessages":
            BitchatChat.shared.startIfEnabled() // resume-on-boot rides the chat poll
            var msgs: [[String: Any]] = node.chatMessages().map {
                [
                    "id": $0.id, "nickname": $0.nickname, "text": $0.text,
                    "timestamp": Double($0.timestampMs), "mine": $0.mine, "net": "mesh",
                ]
            }
            msgs.append(contentsOf: BitchatChat.shared.messages)
            msgs.sort {
                let a = $0["timestamp"] as? Double ?? 0, b = $1["timestamp"] as? Double ?? 0
                if a != b { return a < b }
                return ($0["id"] as? String ?? "") < ($1["id"] as? String ?? "")
            }
            return (true, ["messages": msgs])
        case "bitchatStatus":
            BitchatChat.shared.startIfEnabled()
            return (true, [
                "enabled": BitchatChat.shared.enabled, "active": BitchatChat.shared.active,
            ])
        case "bitchatSetEnabled":
            let a = args as? [String: Any] ?? [:]
            let on = (a["enabled"] as? Bool) ?? false
            let nick = (a["nickname"] as? String) ?? ""
            let (ok, detail) = BitchatChat.shared.setEnabled(on, nickname: nick)
            return (ok, ["enabled": BitchatChat.shared.enabled, "detail": detail])
        default:
            return (false, "unknown chat method: \(method)")
        }
    }
}
