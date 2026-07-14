//! Offline tests for the standalone USDC send. Every chain hop runs through the [`AmoyChain`] seam
//! against [`SendMockAmoy`] — no network. Proves the armed gate, the balance/gas preflights, the
//! exact `transfer` calldata that gets signed + broadcast, the gas clamp, and the fallback limit.

use super::live::build_and_broadcast_usdc;
use super::*;

use std::sync::{Arc, Mutex};

use crate::amoy_swap_verifier::AmoyChain;
use crate::evm::keccak256;
use crate::evm_abi::erc20_transfer;
use crate::evm_rlp::POLYGON_MAINNET_CHAIN_ID;
use crate::evm_signer::LocalEvmKey;
use crate::nimiq::hex::bytes_to_hex;
use crate::polygon_gateway::{EvmLog, EvmReceipt, EvmRpcError};
use crate::swap_live_ffi::LiveSwapFfiError;
use crate::swap_usdc_leg::EvmAddress;

/// The PUBLIC EIP-155 spec key (address `9d8a62f6…855a4f`). Not a funded key — deterministic vector.
const TEST_SECRET: [u8; 32] = [0x46; 32];
const USDC_TOKEN: EvmAddress = [0x3c; 20];
const RECIPIENT: EvmAddress = [0x74; 20];

/// A configurable in-memory `AmoyChain` for the send path: answers `balanceOf` from `usdc_balance`,
/// `eth_getBalance` from `pol_balance`, a fixed nonce/gas-price, an optional estimate (`None` = the
/// node errored → the send falls back), and records every broadcast.
struct SendMockAmoy {
    usdc_balance: u128,
    pol_balance: u128,
    gas_price: u64,
    nonce: u64,
    estimate: Option<u64>,
    broadcasts: Mutex<Vec<Vec<u8>>>,
}

impl SendMockAmoy {
    fn funded() -> Self {
        SendMockAmoy {
            usdc_balance: 5_000_000,                // 5 USDC
            pol_balance: 1_000_000_000_000_000_000, // 1 POL
            gas_price: 45_000_000_000,              // 45 gwei — inside the band
            nonce: 3,
            estimate: Some(70_000),
            broadcasts: Mutex::new(Vec::new()),
        }
    }
    fn broadcasts(&self) -> Vec<Vec<u8>> {
        self.broadcasts.lock().unwrap().clone()
    }
}

impl AmoyChain for SendMockAmoy {
    fn gas_price(&self) -> Result<u64, EvmRpcError> {
        Ok(self.gas_price)
    }
    fn transaction_count(&self, _address: &EvmAddress) -> Result<u64, EvmRpcError> {
        Ok(self.nonce)
    }
    fn balance(&self, _address: &EvmAddress) -> Result<u128, EvmRpcError> {
        Ok(self.pol_balance)
    }
    fn send_raw(&self, raw: &[u8]) -> Result<String, EvmRpcError> {
        self.broadcasts.lock().unwrap().push(raw.to_vec());
        Ok(format!("0x{}", bytes_to_hex(&keccak256(raw))))
    }
    fn receipt(&self, _tx_hash: &[u8; 32]) -> Result<Option<EvmReceipt>, EvmRpcError> {
        Ok(None)
    }
    fn call(&self, _to: &EvmAddress, _data: &[u8]) -> Result<Vec<u8>, EvmRpcError> {
        // balanceOf(address) → a single 32-byte word, the balance right-aligned.
        let mut word = vec![0u8; 32];
        word[16..].copy_from_slice(&self.usdc_balance.to_be_bytes());
        Ok(word)
    }
    fn new_swap_logs_to(
        &self,
        _htlc: &EvmAddress,
        _recipient: &EvmAddress,
        _from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError> {
        Ok(vec![])
    }
    fn head(&self) -> Result<u64, EvmRpcError> {
        Ok(0)
    }
    fn estimate_gas(
        &self,
        _from: &EvmAddress,
        _to: &EvmAddress,
        _data: &[u8],
    ) -> Result<u64, EvmRpcError> {
        self.estimate.ok_or(EvmRpcError::BadResponse {
            method: "eth_estimateGas".to_string(),
        })
    }
}

fn key() -> LocalEvmKey {
    LocalEvmKey::from_secret(&TEST_SECRET).unwrap()
}

fn send(chain: SendMockAmoy, amount: u64) -> Result<FfiUsdcSendResult, LiveSwapFfiError> {
    let chain: Arc<dyn AmoyChain> = Arc::new(chain);
    build_and_broadcast_usdc(
        &chain,
        &key(),
        &USDC_TOKEN,
        &RECIPIENT,
        amount,
        POLYGON_MAINNET_CHAIN_ID,
    )
}

#[test]
fn the_armed_gate_refuses_when_the_master_switch_is_off() {
    // The send's arming predicate IS `swap_live_ffi::mainnet_htlc_if_armed` (the #213 helper); with
    // the flag off + no escrow it refuses — the whole send path is inert on an un-armed build.
    let err = crate::swap_live_ffi::mainnet_htlc_if_armed(false, "").unwrap_err();
    assert!(matches!(err, LiveSwapFfiError::Refused { .. }));
}

#[test]
fn refuses_a_zero_amount() {
    assert!(matches!(
        send(SendMockAmoy::funded(), 0),
        Err(LiveSwapFfiError::BadInput { .. })
    ));
}

#[test]
fn refuses_a_zero_recipient() {
    let chain: Arc<dyn AmoyChain> = Arc::new(SendMockAmoy::funded());
    let zero = [0u8; 20];
    let err = build_and_broadcast_usdc(
        &chain,
        &key(),
        &USDC_TOKEN,
        &zero,
        1_000_000,
        POLYGON_MAINNET_CHAIN_ID,
    )
    .unwrap_err();
    assert!(matches!(err, LiveSwapFfiError::BadInput { .. }));
}

#[test]
fn refuses_when_the_amount_exceeds_the_usdc_balance() {
    let mut chain = SendMockAmoy::funded();
    chain.usdc_balance = 1_000_000; // 1 USDC held
    let err = send(chain, 2_000_000).unwrap_err(); // try to send 2 USDC
    match err {
        LiveSwapFfiError::BadInput { reason } => assert!(reason.contains("exceeds USDC balance")),
        other => panic!("expected over-balance refusal, got {other:?}"),
    }
    // Nothing was broadcast.
}

#[test]
fn refuses_when_pol_cannot_cover_gas() {
    let mut chain = SendMockAmoy::funded();
    chain.pol_balance = 0; // no POL for gas
    let err = send(chain, 1_000_000).unwrap_err();
    match err {
        LiveSwapFfiError::BadInput { reason } => {
            assert!(reason.contains("insufficient POL for gas"))
        }
        other => panic!("expected insufficient-gas refusal, got {other:?}"),
    }
}

#[test]
fn happy_path_signs_the_exact_transfer_calldata_and_returns_its_hash() {
    let chain = Arc::new(SendMockAmoy::funded());
    let chain_dyn: Arc<dyn AmoyChain> = chain.clone();
    let amount = 2_500_000; // 2.5 USDC
    let out = build_and_broadcast_usdc(
        &chain_dyn,
        &key(),
        &USDC_TOKEN,
        &RECIPIENT,
        amount,
        POLYGON_MAINNET_CHAIN_ID,
    )
    .unwrap();

    // Exactly one broadcast, carrying the ERC-20 `transfer(recipient, amount)` calldata verbatim.
    let sent = chain.broadcasts();
    assert_eq!(sent.len(), 1);
    let raw = &sent[0];
    let calldata = erc20_transfer(&RECIPIENT, amount);
    assert!(
        raw.windows(calldata.len()).any(|w| w == &calldata[..]),
        "the broadcast tx must carry the transfer calldata"
    );
    // Signed to the USDC token (the tx `to`): rlp_bytes(20-byte addr) = 0x94 ++ addr.
    let mut to_field = vec![0x94u8];
    to_field.extend_from_slice(&USDC_TOKEN);
    assert!(raw.windows(21).any(|w| w == &to_field[..]));
    // EIP-155 chain id 137 is bound into the signed tx via `v = recovery + 137*2 + 35` = 309|310
    // (0x0135|0x0136), RLP-encoded as `82 01 35` | `82 01 36` — proof the reveal is mainnet-replay-bound.
    assert!(
        raw.windows(3).any(|w| w == [0x82, 0x01, 0x35])
            || raw.windows(3).any(|w| w == [0x82, 0x01, 0x36]),
        "the signed tx must carry the EIP-155 v for chain id 137"
    );

    // The returned hash is the real keccak256 of the signed bytes (what a node assigns).
    assert_eq!(out.tx_hash, format!("0x{}", bytes_to_hex(&keccak256(raw))));
    // The sender is the source secret's address.
    assert_eq!(
        out.from_address,
        format!("0x{}", bytes_to_hex(&key().address()))
    );
    assert_eq!(out.amount_micro, amount);
    // gas price 45 gwei is already inside [30, 100] → unchanged.
    assert_eq!(out.gas_price_wei, 45_000_000_000);
    // estimate 70 000 + 25 % buffer = 87 500 (< the 200 000 cap).
    assert_eq!(out.gas_limit, 87_500);
}

#[test]
fn the_gas_price_clamps_into_the_thirty_to_hundred_gwei_band() {
    // Below the floor clamps UP to 30 gwei.
    let mut low = SendMockAmoy::funded();
    low.gas_price = 7_000_000_000;
    assert_eq!(
        send(low, 1_000_000).unwrap().gas_price_wei,
        USDC_SEND_MIN_GAS_PRICE_WEI
    );
    // Above the cap clamps DOWN to 100 gwei (a live read saw ~280 gwei).
    let mut high = SendMockAmoy::funded();
    high.gas_price = 280_000_000_000;
    assert_eq!(
        send(high, 1_000_000).unwrap().gas_price_wei,
        USDC_SEND_MAX_GAS_PRICE_WEI
    );
}

#[test]
fn a_missing_estimate_falls_back_to_the_fixed_cap() {
    let mut chain = SendMockAmoy::funded();
    chain.estimate = None; // the node's eth_estimateGas errored
    let out = send(chain, 1_000_000).unwrap();
    assert_eq!(out.gas_limit, USDC_TRANSFER_GAS_FALLBACK);
}
