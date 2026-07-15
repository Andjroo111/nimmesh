//! The MAINNET **assembly** tests for the live swap doors — extracted from `swap_live_ffi_tests.rs`
//! when it hit the 800-line ceiling. It reuses that file's `live` fixtures (`init_cfg`, `resp_cfg`,
//! `radio`, `wallet_key`), which are `pub(super)` for exactly this.
//!
//! These tests arm the mainnet path with an **INJECTED** HTLC address: the shipped
//! `MAINNET_HTLC_ADDRESS` const stays empty and `MAINNET_SWAP_ENABLED` stays false, so nothing here
//! flips a real gate or touches real funds.

use super::live::{init_cfg, radio, resp_cfg, wallet_key};
use super::live_impl::{
    assemble_initiator_mainnet, assemble_responder_mainnet, mainnet_money_path,
};
use super::*;
use crate::node::MeshNode;
use crate::swap_funding_verify::{ConfirmationPolicy, SwapCaps};
use crate::swap_intent::Asset;

/// A stand-in deployed HTLC for the ARMED-assembly tests: any valid 20-byte 0x-hex. This is
/// injected straight into `assemble_*_mainnet`, so it NEVER touches the shipped
/// `MAINNET_HTLC_ADDRESS` const (still empty) or `MAINNET_SWAP_ENABLED` (still false).
const MAINNET_HTLC_HEX: &str = "0xA7bB819Ba03743643249dFCCa7508280eCE059b1";
/// A Nimiq mainnet RPC (no "testnet" fragment → `HttpGatewayRpc::new_mainnet` admits it).
const MAINNET_NIM_RPC: &str = "https://rpc.nimiqwatch.com";
/// An allow-listed Polygon mainnet RPC (`HttpPolygonRpc::new_mainnet` admits only these).
const MAINNET_POLY_RPC: &str = "https://polygon.drpc.org";

fn mainnet_init_cfg() -> FfiLiveInitiatorConfig {
    let mut c = init_cfg();
    c.nim_rpc_url = MAINNET_NIM_RPC.into();
    c.amoy_rpc_url = MAINNET_POLY_RPC.into();
    c.htlc_address = String::new(); // ignored on the mainnet path (the const wins)
    c
}

fn mainnet_resp_cfg() -> FfiLiveResponderConfig {
    let mut c = resp_cfg();
    c.nim_rpc_url = MAINNET_NIM_RPC.into();
    c.amoy_rpc_url = MAINNET_POLY_RPC.into();
    c.htlc_address = String::new();
    c.usdc_address = String::new(); // ignored — the native mainnet USDC const wins
    c
}

#[test]
fn mainnet_money_path_selects_every_mainnet_component() {
    let m = mainnet_money_path(MAINNET_HTLC_HEX).expect("valid htlc resolves");
    assert_eq!(m.evm_chain_id, crate::evm_rlp::POLYGON_MAINNET_CHAIN_ID);
    assert_eq!(m.evm_chain_id, 137, "Polygon mainnet chain id");
    assert_eq!(m.network, crate::NetworkId::Mainnet);
    assert_eq!(m.confirm_policy, ConfirmationPolicy::mainnet_defaults());
    assert_eq!(m.caps, SwapCaps::mainnet_first_swap());
    // The escrow contract is the injected address; the token is the NATIVE Circle USDC.
    assert!(format!("0x{}", bytes_to_hex(&m.htlc)).eq_ignore_ascii_case(MAINNET_HTLC_HEX));
    assert!(format!("0x{}", bytes_to_hex(&m.usdc))
        .eq_ignore_ascii_case(crate::polygon_gateway::NATIVE_USDC_POLYGON_MAINNET));
}

#[test]
fn armed_initiator_assembly_is_mainnet_and_caps_refuse_over_cap() {
    let (session, _signer, gateway, money) = assemble_initiator_mainnet(
        wallet_key(),
        LiveLockBook::new(),
        mainnet_init_cfg(),
        None,
        MAINNET_HTLC_HEX,
    )
    .expect("armed initiator assembles");
    // The intent is stamped MAINNET and gives NIM (the initiator role).
    let intent = session
        .identity
        .standing_intent
        .as_ref()
        .expect("standing intent");
    assert_eq!(intent.network_id, crate::NetworkId::Mainnet.wire_id());
    assert_eq!(intent.gives, Asset::Nim);
    assert!(gateway.is_none(), "no gateway requested");
    // C1 live-safety holds: chain-backed verifier + non-sim secret + non-zero mainnet depths.
    assert!(session.live_safety().is_ok());
    // The hard mainnet caps are wired into the session and REFUSE over-cap.
    assert_eq!(money.caps, SwapCaps::mainnet_first_swap());
    assert!(
        session.within_caps(50 * 100_000, 5 * 1_000_000),
        "exactly at cap is admitted"
    );
    assert!(
        !session.within_caps(51 * 100_000, 5 * 1_000_000),
        "over the NIM cap is refused"
    );
    assert!(
        !session.within_caps(50 * 100_000, 6 * 1_000_000),
        "over the USDC cap is refused"
    );
}

#[test]
fn armed_responder_assembly_uses_native_usdc_and_mainnet() {
    let (session, _signer, gateway, money) =
        assemble_responder_mainnet(mainnet_resp_cfg(), None, MAINNET_HTLC_HEX)
            .expect("armed responder assembles");
    let intent = session
        .identity
        .standing_intent
        .as_ref()
        .expect("standing intent");
    assert_eq!(intent.network_id, crate::NetworkId::Mainnet.wire_id());
    assert_eq!(intent.gives, Asset::Usdc);
    assert!(gateway.is_none());
    assert!(session.live_safety().is_ok());
    // The responder escrows the NATIVE Polygon-mainnet USDC (never `config.usdc_address`).
    assert!(format!("0x{}", bytes_to_hex(&money.usdc))
        .eq_ignore_ascii_case(crate::polygon_gateway::NATIVE_USDC_POLYGON_MAINNET));
    assert!(
        !session.within_caps(50 * 100_000, 6 * 1_000_000),
        "over the USDC cap is refused"
    );
}

#[test]
fn the_mainnet_ctors_gate_on_the_arm_flag() {
    // State-agnostic (the mainnet_swap.rs pattern): the mainnet FFI ctors REFUSE while the path
    // is unarmed and PROCEED once Andjroo's arming release opens the flag + records the escrow.
    let init = MeshNode::new_live_swap_initiator_mainnet(
        b"m".to_vec(),
        radio("m"),
        wallet_key(),
        LiveLockBook::new(),
        mainnet_init_cfg(),
        None,
    );
    let resp = MeshNode::new_live_swap_responder_mainnet(
        b"m2".to_vec(),
        radio("m2"),
        mainnet_resp_cfg(),
        None,
    );
    if mainnet_swap_armed() {
        // ARMED: the arm gate is open, so the ctors build a real mainnet-network node from the
        // (valid) mainnet RPC fixtures — no longer a refusal. The armed-assembly invariants are
        // proven by the dedicated `assemble_*_mainnet` tests above; here just confirm they no
        // longer refuse on the arm gate.
        assert!(init.is_ok(), "armed mainnet initiator constructs");
        assert!(resp.is_ok(), "armed mainnet responder constructs");
    } else {
        // UNARMED (pre-arming build): both ctors REFUSE outright (flag off / empty HTLC) — inert.
        assert!(
            matches!(init, Err(LiveSwapFfiError::Refused { .. })),
            "the unarmed mainnet initiator refuses"
        );
        assert!(
            matches!(resp, Err(LiveSwapFfiError::Refused { .. })),
            "the unarmed mainnet responder refuses"
        );
    }
}
