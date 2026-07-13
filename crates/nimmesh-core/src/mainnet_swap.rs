//! # mainnet_swap — the single OFF-BY-DEFAULT master switch for a real-funds cross-chain swap
//!
//! The G13 safety contract (`docs/MAINNET-GATING.md`) keeps every swap constructor pinned to
//! testnet/Amoy by construction. The first mainnet swap lifts exactly the §8.4 guard points — but
//! this crate must NEVER lift a money-path guard on a merged branch. So the lift is threaded through
//! ONE flag here that is **`false`**: every guard this module gates still refuses mainnet while it is
//! false, so the merged behaviour is byte-identical to today. Andjroo flips it (a one-line, reviewed
//! change) as the deliberate act that arms the ≤ $5 self-swap — never the agent, never the loop.
//!
//! What flipping it to `true` enables (once the §8.3 first-run wiring is in place):
//! - [`fund_nim`](crate::live_swap_signer) may build a NIM HTLC on `NetworkId::Mainnet`;
//! - [`MeshNode::build`](crate::node) may ride a LIVE swap signer on a mainnet node;
//! - the mainnet Polygon client ([`crate::polygon_gateway::HttpPolygonRpc::new_mainnet`]) + native
//!   USDC ([`crate::polygon_gateway::NATIVE_USDC_POLYGON_MAINNET`]) + mainnet chain id `137`
//!   ([`crate::evm_rlp::POLYGON_MAINNET_CHAIN_ID`]) + mainnet depths
//!   ([`crate::swap_funding_verify::ConfirmationPolicy::mainnet_defaults`]) + the hard per-swap caps
//!   ([`crate::swap_funding_verify::SwapCaps::mainnet_first_swap`]) become the money-path config.
//!
//! The C1 live-safety gate ([`crate::swap_session::SwapSession::live_safety`]) and the per-swap caps
//! are enforced REGARDLESS of this flag — they are additional floors, not gated by it.

use crate::NetworkId;

/// The master switch. **`false`** — a merged branch never has mainnet swaps enabled; every gated
/// guard below still refuses mainnet. Andjroo flips this to `true` (reviewed, `money-path`,
/// `needs:owner`) to arm the ≤ $5 first self-swap. The autonomous loop never touches it.
pub const MAINNET_SWAP_ENABLED: bool = false;

/// **OWNER-GATED.** The deployed `NimmeshHtlc` on Polygon **mainnet** (forwarder-bound,
/// source-verified on polygonscan). EMPTY until Andjroo deploys it (the deploy plan is in the
/// guard-lift PR body; the agent never deploys). The mainnet swap path refuses to run while this is
/// empty — there is no HTLC to escrow in.
pub const MAINNET_HTLC_ADDRESS: &str = "";

/// Whether a swap money-path leg may run on `network`. **Testnet is always allowed; mainnet ONLY
/// when [`MAINNET_SWAP_ENABLED`] is `true`.** The single predicate the lifted `fund_nim` /
/// `MeshNode::build` guards consult — with the flag off, it returns `false` for mainnet, so those
/// guards refuse mainnet exactly as before the lift.
pub const fn live_swap_allowed(network: NetworkId) -> bool {
    match network {
        NetworkId::Testnet => true,
        NetworkId::Mainnet => MAINNET_SWAP_ENABLED,
    }
}

/// The wire-id form of [`live_swap_allowed`], for call sites that hold a raw `network_id: u8`
/// (e.g. [`crate::swap_coordinator::SwapContext`]). An unrecognized id is refused (fail-closed).
pub fn live_swap_allowed_wire(network_id: u8) -> bool {
    if network_id == NetworkId::Testnet.wire_id() {
        true
    } else if network_id == NetworkId::Mainnet.wire_id() {
        MAINNET_SWAP_ENABLED
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::assertions_on_constants)] // asserting the shipped const value IS the point
    fn mainnet_is_gated_off_by_default() {
        // The floor: while the master switch is false, a live swap leg is refused on mainnet and
        // allowed on testnet — byte-identical to the pre-lift behaviour.
        assert!(live_swap_allowed(NetworkId::Testnet));
        assert_eq!(live_swap_allowed(NetworkId::Mainnet), MAINNET_SWAP_ENABLED);
        assert!(
            !MAINNET_SWAP_ENABLED,
            "a merged branch must ship with mainnet swaps DISABLED"
        );
        assert!(live_swap_allowed_wire(NetworkId::Testnet.wire_id()));
        assert_eq!(
            live_swap_allowed_wire(NetworkId::Mainnet.wire_id()),
            MAINNET_SWAP_ENABLED
        );
        assert!(
            !live_swap_allowed_wire(99),
            "an unknown network id is refused"
        );
    }
}
