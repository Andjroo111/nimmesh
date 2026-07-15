//! G11: the secret-bearing FFI records must never render their secrets.
//!
//! Each test builds a config whose secret fields hold a recognisable pattern, then asserts the
//! `Debug` rendering carries NONE of it. The assertion is exact rather than heuristic: it looks
//! for `format!("{:?}", the_secret_vec)` — the literal `[7, 7, 7, …]` a derived `Debug` emits —
//! so re-deriving `Debug` on any of these records fails the test loudly instead of silently
//! re-opening the leak.

use crate::swap_live_ffi::{FfiLiveInitiatorConfig, FfiLiveResponderConfig};
use crate::swap_participant_ffi::FfiParticipantConfig;
use crate::usdc_send_ffi::FfiUsdcSendConfig;

/// The rendering must not contain the vector's own `Debug` form (what a derive would print),
/// and must mark the field redacted instead.
fn assert_hidden(rendered: &str, secret: &[u8], what: &str) {
    let leaked = format!("{secret:?}");
    assert!(
        !rendered.contains(&leaked),
        "{what} leaked into Debug: {rendered}"
    );
    assert!(
        rendered.contains(&format!("<redacted {} bytes>", secret.len())),
        "{what} is not marked redacted: {rendered}"
    );
}

fn init_cfg() -> FfiLiveInitiatorConfig {
    FfiLiveInitiatorConfig {
        nim_luna: 500_000,
        usdc_micro: 1_000_000,
        expiry_height: 1_000_000,
        intent_seed: vec![7; 32],
        evm_claim_address: vec![0xA5; 20],
        evm_gas_secret: vec![0x46; 32],
        nim_rpc_url: "https://rpc.testnet.nimiqwatch.com".into(),
        amoy_rpc_url: "https://rpc-amoy.polygon.technology".into(),
        htlc_address: "0x".to_string() + &"11".repeat(20),
        delta_safe_blocks: 0,
        min_claim_window_blocks: 0,
    }
}

fn resp_cfg() -> FfiLiveResponderConfig {
    FfiLiveResponderConfig {
        usdc_micro: 1_000_000,
        nim_luna: 500_000,
        expiry_height: 1_000_000,
        nim_claim_seed: vec![0x52; 32],
        evm_funding_secret: vec![0x46; 32],
        nim_rpc_url: "https://rpc.testnet.nimiqwatch.com".into(),
        amoy_rpc_url: "https://rpc-amoy.polygon.technology".into(),
        htlc_address: "0x".to_string() + &"11".repeat(20),
        usdc_address: "0x".to_string() + &"22".repeat(20),
        delta_safe_blocks: 0,
        min_claim_window_blocks: 0,
    }
}

#[test]
fn the_initiator_config_hides_its_seed_and_gas_secret() {
    let cfg = init_cfg();
    let rendered = format!("{cfg:?}");
    assert_hidden(&rendered, &cfg.intent_seed, "intent_seed");
    assert_hidden(&rendered, &cfg.evm_gas_secret, "evm_gas_secret");
}

#[test]
fn the_responder_config_hides_its_claim_seed_and_funding_secret() {
    let cfg = resp_cfg();
    let rendered = format!("{cfg:?}");
    assert_hidden(&rendered, &cfg.nim_claim_seed, "nim_claim_seed");
    assert_hidden(&rendered, &cfg.evm_funding_secret, "evm_funding_secret");
}

#[test]
fn the_participant_config_hides_its_intent_seed() {
    let cfg = FfiParticipantConfig {
        btc_pubkey: vec![2; 33],
        btc_address: vec![9; 20],
        max_concurrent_swaps: 4,
        delta_safe_blocks: 0,
        min_claim_window_blocks: 0,
        standing_intent: None,
        intent_seed: vec![7; 32],
    };
    assert_hidden(&format!("{cfg:?}"), &cfg.intent_seed, "intent_seed");
}

#[test]
fn the_usdc_send_config_hides_its_source_secret() {
    let cfg = FfiUsdcSendConfig {
        source_secret: vec![0x46; 32],
        to_address: "0x".to_string() + &"33".repeat(20),
        amount_micro: 1_000_000,
        rpc_url: "https://polygon-rpc.com".into(),
    };
    assert_hidden(&format!("{cfg:?}"), &cfg.source_secret, "source_secret");
}

/// Redaction must not cost the operator their diagnostics: the PUBLIC fields — the amounts, the
/// endpoints, the addresses money lands on — still have to render, or the app logs a wall of
/// `<redacted>` and nobody can debug a bad config.
#[test]
fn the_public_fields_still_render() {
    let rendered = format!("{:?}", init_cfg());
    for expected in [
        "nim_luna: 500000",
        "usdc_micro: 1000000",
        "expiry_height: 1000000",
        "rpc.testnet.nimiqwatch.com",
        "rpc-amoy.polygon.technology",
    ] {
        assert!(
            rendered.contains(expected),
            "public field {expected} missing from Debug: {rendered}"
        );
    }
    // The claim address is a PUBLIC payout target — the operator must be able to read it back.
    assert!(
        rendered.contains(&format!("{:?}", vec![0xA5u8; 20])),
        "evm_claim_address should not be redacted: {rendered}"
    );
}

/// A redacted field must not reveal the value through the alternate (`{:#?}`) formatter either —
/// pretty-printing is what a panic/`dbg!` actually uses.
#[test]
fn the_alternate_formatter_redacts_too() {
    let cfg = resp_cfg();
    let rendered = format!("{cfg:#?}");
    assert!(
        !rendered.contains(&format!("{:?}", cfg.nim_claim_seed)),
        "nim_claim_seed leaked under {{:#?}}: {rendered}"
    );
    assert!(rendered.contains("<redacted 32 bytes>"));
}
