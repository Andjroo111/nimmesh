//! # swap_usdc_dryrun_tests — the full NIM⇄USDC pipeline, end to end in SIM (P9, `cfg(test)`)
//!
//! The capstone that threads EVERY USDC layer together OFFLINE, proving the discovery → settlement →
//! sign → submit pipeline lines up:
//!
//! 1. **Discovery (P5/P7)** — a matched NIM-giver + USDC-giver [`SwapIntent`] pair with carried EVM
//!    payout addresses.
//! 2. **Leg selection (P6)** — `counterparty_leg_for` picks the USDC leg, built from the pair (P7).
//! 3. **ABI calldata (P3)** — `evm_abi::htlc_new_swap` / `htlc_withdraw` (selectors checked).
//! 4. **Sign (P4)** — assemble the Amoy tx (`evm_rlp`) and sign it with the real `k256`
//!    `LocalEvmKey` (the public EIP-155 test key).
//! 5. **Submit (P8)** — "broadcast" through the in-memory `MockPolygonRpc` (real codec, NO network);
//!    it records exactly the signed bytes and returns a success receipt.
//! 6. **Settlement** — the `UsdcLeg` + `swap::Swap` state machine settle atomically (no one-sided
//!    settlement).
//!
//! Money-path stays gated throughout: a public test key, a mock RPC, no real funds, no live
//! broadcast, Amoy chain id only.

use crate::evm::function_selector;
use crate::evm_abi::{htlc_new_swap, htlc_withdraw};
use crate::evm_rlp::{LegacyTx, POLYGON_AMOY_CHAIN_ID};
use crate::evm_signer::LocalEvmKey;
use crate::nimiq::hex::bytes_to_hex;
use crate::polygon_gateway::MockPolygonRpc;
use crate::swap::{LadderParams, Swap, SwapAction, SwapPhase, SwapRole, SwapTerms};
use crate::swap_intent::{Asset, SwapIntent, INTENT_PUBKEY_LEN, INTENT_SIG_LEN};
use crate::swap_leg::{sha256, HtlcParams, LegOutcome, MockLeg, SwapLeg};
use crate::swap_leg_select::{counterparty_leg_for, CounterpartyLeg};
use crate::swap_usdc_leg::{usdc_leg_for_pair, MICRO_USDC};
use crate::swap_wire::{SwapLegId, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN};

const T_A: u64 = 10_000; // NIM leg timeout (longer)
const T_B: u64 = 5_000; // USDC leg timeout (shorter)
const TEST_SECRET: [u8; 32] = [0x46; 32]; // the PUBLIC EIP-155 test key (NOT a funded key)
const HTLC_CONTRACT: [u8; 20] = [0x5A; 20]; // the (sim) deployed Polygon HTLC contract address

fn intent(
    gives: Asset,
    counter_asset: Asset,
    nim: u64,
    counter_usdc: u64,
    evm_address: [u8; 20],
) -> SwapIntent {
    SwapIntent {
        gives,
        counter_asset,
        nim_amount: nim,
        btc_amount: counter_usdc, // micro-USDC when counter_asset == Usdc
        expiry_height: 1_000_000,
        min_nim: 0,
        max_nim: u64::MAX,
        nim_pubkey: [0x07; INTENT_PUBKEY_LEN],
        nim_address: [0xA1; NIM_ADDRESS_LEN],
        btc_pubkey: {
            let mut k = [0x11; BTC_PUBKEY_LEN];
            k[0] = 0x02;
            k
        },
        btc_address: b"unused-for-usdc".to_vec(),
        evm_address,
        network_id: 5,
        signature: [0u8; INTENT_SIG_LEN],
    }
}

fn addr_hex(a: &[u8; 20]) -> String {
    format!("0x{}", bytes_to_hex(a))
}

#[test]
fn full_usdc_swap_dryrun_threads_discovery_sign_submit_settlement() {
    // --- keys + addresses ---
    let funder_key = LocalEvmKey::from_secret(&TEST_SECRET).unwrap();
    let usdc_funder_evm = funder_key.address(); // the USDC-giver funds from here (== the signing key)
    let nim_claimant_evm = [0xA1u8; 20]; // the NIM-giver receives USDC here

    // --- 1. discovery: a matched NIM⇄USDC pair (P5/P7) ---
    let nim_giver = intent(
        Asset::Nim,
        Asset::Usdc,
        200_000,
        100 * MICRO_USDC,
        nim_claimant_evm,
    );
    let usdc_giver = intent(
        Asset::Usdc,
        Asset::Usdc,
        180_000,
        100 * MICRO_USDC,
        usdc_funder_evm,
    );
    assert!(nim_giver.would_initiate_against(&usdc_giver));
    assert!(nim_giver.amount_compatible(&usdc_giver));

    // --- 2. leg selection (P6) ---
    assert_eq!(
        counterparty_leg_for(nim_giver.counter_asset),
        Some(CounterpartyLeg::Usdc)
    );
    // built from the carried addresses (P7): USDC-giver funds, NIM-giver claims.
    let mut usdc_leg = usdc_leg_for_pair(&nim_giver, &usdc_giver);
    assert_eq!(usdc_leg.sender(), usdc_funder_evm);
    assert_eq!(usdc_leg.receiver(), nim_claimant_evm);

    // the swap executes at the initiator's amounts.
    let give_nim = nim_giver.nim_amount;
    let take_usdc = nim_giver.btc_amount;
    let secret = [42u8; 32];
    let hashlock = sha256(&secret);

    // --- 3. ABI calldata (P3): newSwap escrows take_usdc for the NIM-giver ---
    let new_swap_data = htlc_new_swap(&nim_claimant_evm, take_usdc, &hashlock, T_B);
    assert_eq!(
        &new_swap_data[..4],
        &function_selector("newSwap(address,uint256,bytes32,uint256)")[..]
    );

    // --- 4+5. nonce/gas from the mock RPC (P8 codec, no network) ---
    let rpc = MockPolygonRpc::at_nonce(3);
    let nonce = rpc
        .get_transaction_count(&addr_hex(&usdc_funder_evm))
        .unwrap();
    let gas_price = rpc.gas_price().unwrap();
    assert_eq!(nonce, 3);
    assert!(gas_price > 0);

    // --- 4. assemble + sign the Amoy tx (P4): the USDC-giver funds newSwap on the HTLC contract ---
    let tx = LegacyTx::polygon_amoy(nonce, gas_price, 200_000, HTLC_CONTRACT, 0, &new_swap_data);
    assert_eq!(tx.chain_id, POLYGON_AMOY_CHAIN_ID); // 80002 (Amoy testnet), never mainnet 137
    let signed = tx.sign_with(&funder_key);
    assert!(!signed.is_empty());
    let raw_hex = format!("0x{}", bytes_to_hex(&signed));

    // --- 5. "broadcast" via the mock (P8): it records EXACTLY the signed bytes + acks a receipt ---
    let tx_hash = rpc.send_raw_transaction(&raw_hex).unwrap();
    assert_eq!(rpc.broadcasts(), vec![raw_hex.clone()]); // submitted exactly the signed tx
    assert!(tx_hash.starts_with("0x") && tx_hash.len() == 66); // a real keccak256 tx hash
    let receipt = rpc
        .get_transaction_receipt(&tx_hash)
        .unwrap()
        .expect("mined");
    assert!(receipt.success);

    // --- 6. settlement: the escrows + the state machine settle atomically ---
    let p = LadderParams::default();
    let terms = SwapTerms {
        nim_timeout: T_A,
        counterparty_timeout: T_B,
    };
    let mut alice = Swap::new(SwapRole::Initiator, terms); // NIM-giver
    let mut bob = Swap::new(SwapRole::Responder, terms); // USDC-giver
    let mut nim_leg = MockLeg::new();

    alice.accept(0, &p).unwrap();
    bob.accept(0, &p).unwrap();
    alice.fund(0, &p).unwrap();
    nim_leg
        .fund(HtlcParams {
            hashlock,
            amount: give_nim,
            timeout: T_A,
        })
        .unwrap();
    bob.observe_initiator_funded().unwrap();
    bob.fund(0, &p).unwrap();
    // the newSwap tx above represents this fund; model it on the leg.
    usdc_leg
        .fund(HtlcParams {
            hashlock,
            amount: take_usdc,
            timeout: T_B,
        })
        .unwrap();
    alice.observe_counterparty_funded().unwrap();
    bob.observe_counterparty_funded().unwrap();

    // the withdraw calldata that claims the USDC leg (P3) targets the on-chain swap_id.
    let withdraw_data = htlc_withdraw(&usdc_leg.swap_id().unwrap(), &secret);
    assert_eq!(
        &withdraw_data[..4],
        &function_selector("withdraw(bytes32,bytes32)")[..]
    );

    // Alice claims USDC (revealing S), Bob reads S off it and claims NIM.
    assert_eq!(
        alice.reveal_and_claim().unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Counterparty
        }
    );
    usdc_leg.claim(secret, 100).unwrap();
    alice.observe_settled().unwrap();
    let learned = usdc_leg
        .revealed_preimage()
        .expect("S public after withdraw");
    bob.observe_secret().unwrap();
    nim_leg.claim(learned, 100).unwrap();
    bob.observe_settled().unwrap();

    assert_eq!(alice.phase, SwapPhase::Settled);
    assert_eq!(bob.phase, SwapPhase::Settled);
    // no one-sided settlement: both legs claimed.
    assert_eq!(nim_leg.outcome(), LegOutcome::Claimed);
    assert_eq!(usdc_leg.outcome(), LegOutcome::Claimed);
    assert_eq!(usdc_leg.amount(), Some(take_usdc));
}
