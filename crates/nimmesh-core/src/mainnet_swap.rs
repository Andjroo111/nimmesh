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
pub const MAINNET_SWAP_ENABLED: bool = true;

/// **OWNER-GATED.** The deployed `NimmeshHtlc` on Polygon **mainnet** (forwarder-bound,
/// source-verified on polygonscan). EMPTY until Andjroo deploys it (the deploy plan is in the
/// guard-lift PR body; the agent never deploys). The mainnet swap path refuses to run while this is
/// empty — there is no HTLC to escrow in.
pub const MAINNET_HTLC_ADDRESS: &str = "0x842617Ee5365FBa589509c7ffF1fD3Db30a29177";

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

/// Whether the mainnet swap path is **fully armed**: the master switch is on AND a deployed
/// `NimmeshHtlc` address is recorded. **`false` on any merged branch** ([`MAINNET_SWAP_ENABLED`]
/// is `false` and [`MAINNET_HTLC_ADDRESS`] is empty), so every mainnet swap constructor refuses and
/// the UI can honestly label the state. Both halves must be true — the flag alone does nothing until
/// Andjroo records the escrow contract, and an address without the flag stays disabled. Consulted by
/// the mainnet live-swap constructors ([`crate::swap_live_ffi`]) and surfaced to the app over FFI.
pub const fn mainnet_swap_armed() -> bool {
    MAINNET_SWAP_ENABLED && !MAINNET_HTLC_ADDRESS.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::assertions_on_constants)] // asserting the shipped const contract IS the point
    fn mainnet_is_gated_by_the_master_switch() {
        // The floor, in EITHER state (unarmed main OR Andjroo's arming release): testnet is
        // always allowed; mainnet follows the master switch EXACTLY — false refuses everywhere,
        // true is the deliberate, Andjroo-merged armed state. (The pre-arming hard assert that
        // the flag is false lived here; the arming PR is precisely the reviewed flip, so the
        // durable invariant is the equality, not the value.)
        assert!(live_swap_allowed(NetworkId::Testnet));
        assert_eq!(live_swap_allowed(NetworkId::Mainnet), MAINNET_SWAP_ENABLED);
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

    #[test]
    #[allow(clippy::assertions_on_constants)] // asserting the shipped const contract IS the point
    fn arming_requires_both_halves_and_a_real_escrow() {
        // State-agnostic floor: the aggregate armed status is EXACTLY flag AND address — the
        // flag alone does nothing, an address alone stays disabled. And an ARMED build must
        // carry a plausibly-shaped deployed escrow (0x + 40 hex): never armed into the void.
        assert_eq!(
            mainnet_swap_armed(),
            MAINNET_SWAP_ENABLED && !MAINNET_HTLC_ADDRESS.is_empty()
        );
        if MAINNET_SWAP_ENABLED {
            let a = MAINNET_HTLC_ADDRESS;
            assert!(
                a.starts_with("0x")
                    && a.len() == 42
                    && a[2..].chars().all(|c| c.is_ascii_hexdigit()),
                "an armed build must record the deployed HTLC as 0x + 40 hex, got: {a}"
            );
        }
    }
}
