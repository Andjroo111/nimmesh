//! # reachability — G16 "will it send?" signal + validity-window countdown
//!
//! The biggest anxiety of paying offline is not knowing whether the payment can *get*
//! anywhere. This module turns the node's already-observed mesh state into an honest answer,
//! and tells the user how long a signed transaction stays spendable before its validity
//! window closes. Both are **pure, clock-free, key-free** decisions over public inputs — the
//! node FFI feeds them the live peer count, whether a gateway has been heard (a head beacon
//! means an internet-bearing gateway is reachable through the mesh, G9), and the freshest
//! head height (G9). No signing, no broadcast, no payload inspection.
//!
//! What this is **not**: the actual queue-and-auto-send of a *signed* transaction is the
//! money path (C1) — it rides the seed/signing seam and is Andjroo-gated. G16 is the honest
//! *signal* layer over the existing mesh state that the send UI and that queue both read.

/// Albatross produces ~one micro/skip block per second, so the 7200-block validity window is
/// ~2 h. Used to render the validity countdown as a wall-clock estimate.
pub const ALBATROSS_BLOCK_SECS: u32 = 1;

/// Below this many blocks left in the validity window, prompt the user to re-sign before the
/// transaction expires in flight (~10 min of head room at ~1 block/s).
pub const RESIGN_THRESHOLD_BLOCKS: u32 = 600;

/// How far a payment can travel from where this node stands right now — the honest "will it
/// send?" answer. FFI-visible so the send UI can reassure (or warn) accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Reachability {
    /// A gateway is reachable now (this node has internet, or it has heard a gateway's head
    /// beacon through the mesh): a send will go out to the network.
    Online,
    /// Connected to peers but no gateway seen yet: a send will relay and reach the network as
    /// soon as a connected device comes online (store-and-forward carries it, G7).
    Meshed,
    /// No peers nearby: a send is queued locally and floods the instant a device is in range.
    Offline,
}

/// Assess reachability from the node's public mesh state.
///
/// - `self_is_gateway` — this device has internet itself, so it can broadcast directly.
/// - `peer_count` — how many BLE peers are connected right now.
/// - `heard_gateway` — whether a gateway head beacon (G9) has been heard (a gateway is alive
///   somewhere in mesh reach).
///
/// `Online` if we can reach a gateway (ourselves, or via the mesh with peers + a heard
/// beacon); `Meshed` if we have peers but no gateway yet; `Offline` with no peers.
pub fn assess_reachability(
    self_is_gateway: bool,
    peer_count: u32,
    heard_gateway: bool,
) -> Reachability {
    if self_is_gateway || (peer_count > 0 && heard_gateway) {
        Reachability::Online
    } else if peer_count > 0 {
        Reachability::Meshed
    } else {
        Reachability::Offline
    }
}

/// Blocks remaining before a transaction's validity window closes, given the freshest head
/// this node has heard.
///
/// Returns `None` when it can't be judged (no head heard yet, or the tx carries no
/// `valid_until`); `Some(0)` once expired (at/after `valid_until`); otherwise the remaining
/// blocks. Saturating, so a hostile/huge `valid_until` can't wrap.
pub fn blocks_until_expiry(head: Option<u32>, valid_until: Option<u32>) -> Option<u32> {
    let head = head?;
    let valid_until = valid_until?;
    Some(valid_until.saturating_sub(head))
}

/// The validity countdown as an estimated number of **seconds** (`blocks × ~1 s`), for a
/// human-readable "expires in ~N min" stamp. `None` when [`blocks_until_expiry`] is `None`.
pub fn secs_until_expiry(head: Option<u32>, valid_until: Option<u32>) -> Option<u32> {
    blocks_until_expiry(head, valid_until).map(|b| b.saturating_mul(ALBATROSS_BLOCK_SECS))
}

/// Whether a still-valid transaction is close enough to expiry that the user should re-sign
/// it before it dies in flight. `false` when it can't be judged or it has already expired
/// (an expired tx needs a fresh one, not a re-sign nudge — that is the expiry state, not the
/// near-expiry warning).
pub fn needs_resign(head: Option<u32>, valid_until: Option<u32>) -> bool {
    match blocks_until_expiry(head, valid_until) {
        Some(left) => left > 0 && left <= RESIGN_THRESHOLD_BLOCKS,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gateway_node_is_always_online() {
        assert_eq!(assess_reachability(true, 0, false), Reachability::Online);
        assert_eq!(assess_reachability(true, 5, true), Reachability::Online);
    }

    #[test]
    fn peers_plus_a_heard_gateway_is_online() {
        assert_eq!(assess_reachability(false, 3, true), Reachability::Online);
    }

    #[test]
    fn peers_without_a_gateway_is_meshed() {
        assert_eq!(assess_reachability(false, 3, false), Reachability::Meshed);
    }

    #[test]
    fn no_peers_is_offline() {
        assert_eq!(assess_reachability(false, 0, false), Reachability::Offline);
        // A "heard gateway" with no peers is still offline — there's no one to hand it to.
        assert_eq!(assess_reachability(false, 0, true), Reachability::Offline);
    }

    #[test]
    fn expiry_countdown_basics() {
        // 7200-block window, head 100 below the cap → 100 blocks left.
        assert_eq!(blocks_until_expiry(Some(7100), Some(7200)), Some(100));
        // exactly at / past the window → 0 (expired), never a wrap.
        assert_eq!(blocks_until_expiry(Some(7200), Some(7200)), Some(0));
        assert_eq!(blocks_until_expiry(Some(7300), Some(7200)), Some(0));
        // unknown head or no validity window → cannot judge.
        assert_eq!(blocks_until_expiry(None, Some(7200)), None);
        assert_eq!(blocks_until_expiry(Some(7100), None), None);
    }

    #[test]
    fn secs_tracks_blocks_at_one_per_second() {
        assert_eq!(secs_until_expiry(Some(7000), Some(7200)), Some(200));
        assert_eq!(secs_until_expiry(None, None), None);
    }

    #[test]
    fn resign_nudge_only_fires_in_the_near_expiry_window() {
        // Plenty of head room → no nudge.
        assert!(!needs_resign(Some(0), Some(7200)));
        // Inside the last 600 blocks → nudge.
        assert!(needs_resign(Some(7200 - 600), Some(7200)));
        assert!(needs_resign(Some(7200 - 1), Some(7200)));
        // Already expired → not a re-sign nudge (it's the expiry state).
        assert!(!needs_resign(Some(7200), Some(7200)));
        // Can't judge → no nudge.
        assert!(!needs_resign(None, Some(7200)));
    }
}
