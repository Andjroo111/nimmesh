import Foundation
import UIKit
import NimmeshCore

/// The standalone USDC **send** — the money-path twin of the NIM `sendTransaction` bridge, for USDC
/// on Polygon mainnet. The user initiates it on device; a native confirm is shown BEFORE anything is
/// signed, and the send is REFUSED unless the mainnet path is armed (the core's `mainnet_swap_armed`,
/// enforced inside `sendUsdcMainnet`). Nothing sends autonomously.
///
/// The heavy logic lives here (not in `WebHostView`) to keep the coordinator under the size ceiling.
/// It picks the funded source account automatically (the wallet-derived `claim` account first, then
/// `fund` — whichever holds ≥ the amount), passes that account's DERIVED SECRET into the Rust core
/// ONCE (it never crosses back — only the tx hash + public sender address return), and hands the page
/// the broadcast hash. The token, gas preflight, balance check, and EIP-155 chain id are all pinned
/// in the core; this file only picks the source, confirms with the user, and forwards.
enum PolygonSend {
    /// Single bridge entry point (mirrors `PolygonReads.handle`), so `WebHostView`'s dispatch glue
    /// stays a single line under the size guard.
    static func handle(method: String, args: Any?) async -> (Bool, Any) {
        switch method {
        case "sendUsdc": return await sendUsdc(args: args)
        default: return (false, "unknown polygon-send method: \(method)")
        }
    }

    /// Args: `{ to: "0x…", amountMicro: Int }`. Answers `{ txHash, from, amountMicro }` on a broadcast.
    private static func sendUsdc(args: Any?) async -> (Bool, Any) {
        let a = args as? [String: Any] ?? [:]
        let to = ((a["to"] as? String) ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        guard isEvmAddr(to) else { return (false, "enter a valid 0x address") }
        let amountMicro = (a["amountMicro"] as? NSNumber)?.uint64Value ?? 0
        guard amountMicro > 0 else { return (false, "enter an amount") }

        // The wallet-derived USDC-bearing accounts, in send-preference order: `claim` first, then
        // `fund` (the `gas` account holds POL only, never USDC). The secret never leaves Swift until
        // the ONE FFI call below.
        guard let evm = Wallet.swapEvmSecrets(), let rs = Wallet.swapResponderSecrets() else {
            return (false, "no wallet — create or import one first")
        }
        let candidates: [Data] = [evm.claim, rs.fund]

        // Pick the first account that actually holds ≥ the amount; error honestly if neither does.
        var chosen: (secret: Data, address: String)?
        for secret in candidates {
            guard let addr = try? NimmeshCore.evmAddressForSecret(secret: secret), !addr.isEmpty
            else { continue }
            if let bal = await PolygonReads.usdcBalance(of: addr), UInt64(max(0, bal)) >= amountMicro {
                chosen = (secret, addr)
                break
            }
        }
        guard let src = chosen else {
            return (false, "no account holds enough USDC to cover this send")
        }

        // Native confirm BEFORE signing — unbypassable (Swift-side), spelled out as REAL mainnet
        // funds, exactly like the app's other confirms.
        let human = String(format: "%.2f", Double(amountMicro) / 1_000_000)
        let ok = await confirm(
            "Send \(human) USDC to \(shortAddr(to))?\n\nThis moves REAL Polygon USDC on mainnet."
        )
        guard ok else { return (false, "cancelled") }

        // Sign + broadcast in the core. The blocking FFI runs off the main + cooperative pools; the
        // derived secret enters once and only the public result returns.
        let cfg = FfiUsdcSendConfig(
            sourceSecret: src.secret,
            toAddress: to,
            amountMicro: amountMicro,
            rpcUrl: PolygonReads.rpcURL.absoluteString
        )
        do {
            let res: FfiUsdcSendResult = try await withCheckedThrowingContinuation { cont in
                DispatchQueue.global(qos: .userInitiated).async {
                    do { cont.resume(returning: try NimmeshCore.sendUsdcMainnet(config: cfg)) }
                    catch { cont.resume(throwing: error) }
                }
            }
            return (true, [
                "txHash": res.txHash,
                "from": res.fromAddress,
                "amountMicro": Int(res.amountMicro),
            ])
        } catch {
            return (false, "\(error)")
        }
    }

    // MARK: - helpers

    private static func isEvmAddr(_ s: String) -> Bool {
        let a = (s.hasPrefix("0x") || s.hasPrefix("0X")) ? String(s.dropFirst(2)) : s
        return a.count == 40 && a.allSatisfy { $0.isHexDigit }
    }

    private static func shortAddr(_ a: String) -> String {
        a.count >= 24 ? "\(a.prefix(8))…\(a.suffix(6))" : a
    }

    /// A native confirm alert (the same UIAlertController the WKUIDelegate `confirm()` uses), awaited
    /// so the send only proceeds on an explicit "Send". No presenter → treated as declined.
    @MainActor
    private static func confirm(_ message: String) async -> Bool {
        await withCheckedContinuation { cont in
            guard let top = Bridge.topmostViewController() else {
                cont.resume(returning: false)
                return
            }
            let alert = UIAlertController(title: "Confirm send", message: message, preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "Cancel", style: .cancel) { _ in cont.resume(returning: false) })
            alert.addAction(UIAlertAction(title: "Send", style: .default) { _ in cont.resume(returning: true) })
            top.present(alert, animated: true)
        }
    }
}
