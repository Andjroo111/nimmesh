//! # usdc_send_ffi — the standalone USDC **send** over UniFFI (OWNER-GATED, real mainnet funds)
//!
//! The money-path twin of the NIM `sendTransaction` bridge, for USDC on Polygon: the USER, on
//! device, initiates a plain ERC-20 `transfer` of their OWN USDC to a `0x` recipient. Nothing sends
//! autonomously — the app presents a native confirm BEFORE it calls this, exactly like the NIM send.
//!
//! Same trust model as [`crate::swap_live_ffi`]: the caller-derived secret enters the core ONCE and
//! never crosses back (only the signature + the public tx hash leave), and every guard is baked in:
//!
//! - **Armed gate.** Refuses with [`LiveSwapFfiError::Refused`] unless the mainnet swap path is fully
//!   armed ([`crate::mainnet_swap::mainnet_swap_armed`] — the same #213 predicate the swap uses), so
//!   on any un-armed build the whole send path is inert.
//! - **Native USDC pinned.** The token is the code-pinned [`NATIVE_USDC_POLYGON_MAINNET`], never
//!   caller-supplied — a caller can never point the send at the wrong token.
//! - **Balance + gas preflight.** Refuses a zero/malformed recipient or a zero amount; refuses when
//!   `amount > balanceOf(sender)`; and refuses when the sender's native POL can't cover the gas
//!   budget (`gas_limit × gas_price`) — the receipt-verified Amoy gas discipline (`docs/swap/AMOY.md`)
//!   carried to mainnet: estimate via `eth_estimateGas` (+ a 25 % buffer, a fixed fallback cap when
//!   the estimate fails), and clamp the node's gas-price suggestion into `[30, 100]` gwei.
//! - **No cap beyond balance.** The user sends their own funds (like a NIM send); there is no per-tx
//!   ceiling other than the balance itself.
//!
//! The featured guts (the live client + the `AmoyChain`-seam core) sit behind `polygon-gateway` +
//! `gateway-rpc`; the exported surface is always present and refuses honestly when the build lacks
//! them (the `gateway_ffi`/`swap_live_ffi` shared-bindings discipline).

/// The floor of the gas-price clamp (wei) — 30 gwei, the Amoy floor (`EvmGasConfig::min_gas_price`).
pub const USDC_SEND_MIN_GAS_PRICE_WEI: u64 = 30_000_000_000;
/// The cap of the gas-price clamp (wei) — 100 gwei. Polygon fee spikes are real (a live read saw the
/// node suggest ~280 gwei); the send clamps into a sane band instead of overpaying a momentary spike.
pub const USDC_SEND_MAX_GAS_PRICE_WEI: u64 = 100_000_000_000;
/// The gas limit used when `eth_estimateGas` fails — safely above the receipt-verified range for a
/// proxied-USDC `transfer` (a warm recipient estimates ~61 k, a fresh-recipient SSTORE pushes toward
/// ~85 k; `docs/swap/AMOY.md`), so a send is never under-provisioned when the estimate is unavailable.
pub const USDC_TRANSFER_GAS_FALLBACK: u64 = 120_000;
/// A hard ceiling on the estimate-derived gas limit — a hostile/oversized estimate can never inflate
/// the POL preflight past this. A plain `transfer` never legitimately needs more.
pub const USDC_TRANSFER_GAS_MAX: u64 = 200_000;

/// Everything the app hands the core to send USDC. `source_secret` enters ONCE and never crosses back
/// (the G45/G11 rule) — only the tx hash + public sender address are returned.
// `Debug` is hand-written in `crate::ffi_secret_redaction` — it MUST NOT be derived here: the
// derive renders `source_secret` as raw bytes at any `{:?}`.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiUsdcSendConfig {
    /// The 32-byte secp256k1 secret of the FUNDED source account (the wallet-derived claim/fund
    /// account the app picked because it holds ≥ the amount). Enters once; never returned.
    pub source_secret: Vec<u8>,
    /// The recipient EVM address, `0x`-hex (20 bytes).
    pub to_address: String,
    /// The amount to send, in micro-USDC (6 decimals).
    pub amount_micro: u64,
    /// An allow-listed Polygon **mainnet** JSON-RPC url (`guard_polygon_mainnet`).
    pub rpc_url: String,
}

/// The receipt of a broadcast USDC send — the tx hash to show + the exact gas numbers used, so the
/// app can label the send honestly.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiUsdcSendResult {
    /// The broadcast transaction hash (lowercase `0x…`).
    pub tx_hash: String,
    /// The sender account the send went out from (`0x`-hex) — the address of `source_secret`.
    pub from_address: String,
    /// The micro-USDC sent (echoed back for the success screen).
    pub amount_micro: u64,
    /// The gas limit the tx carried.
    pub gas_limit: u64,
    /// The clamped gas price (wei) the tx carried.
    pub gas_price_wei: u64,
}

/// **OWNER-GATED (real mainnet funds): the standalone USDC send.** Signs an ERC-20 `transfer` of the
/// caller's own native Polygon-mainnet USDC with `source_secret` and broadcasts it, returning the tx
/// hash. Refuses unless the mainnet path is armed (see module docs), on a zero/over-balance amount, a
/// malformed/zero recipient, or when the sender's POL can't cover gas. Requires `polygon-gateway` +
/// `gateway-rpc`; refuses [`LiveSwapFfiError::Unsupported`] otherwise.
#[uniffi::export]
pub fn send_usdc_mainnet(
    config: FfiUsdcSendConfig,
) -> Result<FfiUsdcSendResult, crate::swap_live_ffi::LiveSwapFfiError> {
    #[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
    {
        live::send(config)
    }
    #[cfg(not(all(feature = "polygon-gateway", feature = "gateway-rpc")))]
    {
        let _ = config;
        Err(crate::swap_live_ffi::LiveSwapFfiError::Unsupported)
    }
}

// --- the featured guts (behind polygon-gateway + gateway-rpc) ----------------------------------
#[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
pub(crate) mod live {
    use super::*;
    use std::sync::Arc;

    use crate::amoy_swap_verifier::AmoyChain;
    use crate::evm_abi::{erc20_balance_of, erc20_transfer};
    use crate::evm_rlp::{LegacyTx, POLYGON_MAINNET_CHAIN_ID};
    use crate::evm_signer::LocalEvmKey;
    use crate::nimiq::hex::{bytes_to_hex, hex_to_bytes};
    use crate::polygon_gateway::{HttpPolygonRpc, NATIVE_USDC_POLYGON_MAINNET};
    use crate::swap_live_ffi::LiveSwapFfiError;
    use crate::swap_usdc_leg::EvmAddress;

    fn bad(reason: impl Into<String>) -> LiveSwapFfiError {
        LiveSwapFfiError::BadInput {
            reason: reason.into(),
        }
    }
    fn refused(reason: impl std::fmt::Display) -> LiveSwapFfiError {
        LiveSwapFfiError::Refused {
            reason: reason.to_string(),
        }
    }
    fn transport(e: impl std::fmt::Display) -> LiveSwapFfiError {
        LiveSwapFfiError::Transport {
            reason: e.to_string(),
        }
    }

    fn seed32(bytes: &[u8]) -> Result<[u8; 32], LiveSwapFfiError> {
        bytes
            .try_into()
            .map_err(|_| bad("source_secret must be 32 bytes"))
    }

    fn parse_evm_addr(s: &str, what: &str) -> Result<EvmAddress, LiveSwapFfiError> {
        hex_to_bytes(s.trim())
            .ok()
            .and_then(|b| EvmAddress::try_from(b.as_slice()).ok())
            .ok_or_else(|| bad(format!("{what} must be a 20-byte 0x-hex address")))
    }

    /// Decode a 32-byte ABI word to `u128` — fail-closed if the high 16 bytes are non-zero (a USDC
    /// balance never overflows `u128`; a hostile oversized word reads `None` rather than truncating).
    fn word_to_u128(word: &[u8]) -> Option<u128> {
        if word.len() != 32 || word[..16].iter().any(|&b| b != 0) {
            return None;
        }
        let mut be = [0u8; 16];
        be.copy_from_slice(&word[16..32]);
        Some(u128::from_be_bytes(be))
    }

    /// The armed-gate entry point (production): refuse unless `mainnet_swap_armed`, resolve the live
    /// client + signer, then hand off to [`build_and_broadcast_usdc`]. The token is the code-pinned
    /// native USDC; the chain id is `137`. On any un-armed build `mainnet_htlc_if_armed` returns
    /// `Err`, so nothing here reaches the network.
    pub(crate) fn send(config: FfiUsdcSendConfig) -> Result<FfiUsdcSendResult, LiveSwapFfiError> {
        // Reuse the #213 arming predicate (the same the swap constructors consult).
        let _htlc = crate::swap_live_ffi::mainnet_htlc_if_armed(
            crate::mainnet_swap::live_swap_allowed(crate::NetworkId::Mainnet),
            crate::mainnet_swap::MAINNET_HTLC_ADDRESS,
        )?;
        let secret = seed32(&config.source_secret)?;
        let key = LocalEvmKey::from_secret(&secret)
            .map_err(|_| bad("source_secret is not a valid secp256k1 scalar"))?;
        let to = parse_evm_addr(&config.to_address, "to_address")?;
        // The token is CODE-PINNED native Circle USDC on Polygon mainnet — never caller-supplied.
        let usdc = parse_evm_addr(NATIVE_USDC_POLYGON_MAINNET, "NATIVE_USDC_POLYGON_MAINNET")?;
        let chain: Arc<dyn AmoyChain> = Arc::new(
            HttpPolygonRpc::new_mainnet(config.rpc_url.clone())
                .map_err(|e| refused(e.to_string()))?,
        );
        build_and_broadcast_usdc(
            &chain,
            &key,
            &usdc,
            &to,
            config.amount_micro,
            POLYGON_MAINNET_CHAIN_ID,
        )
    }

    /// The offline-testable core: preflight (recipient, amount, USDC balance, POL gas), build + sign +
    /// broadcast the `transfer`, return the hash + gas numbers. Every chain hop goes through the
    /// [`AmoyChain`] seam, so a fake proves the whole path without a network. `chain_id` selects the
    /// EIP-155 network the tx is replay-bound to (`137` for the mainnet send).
    pub(crate) fn build_and_broadcast_usdc(
        chain: &Arc<dyn AmoyChain>,
        key: &LocalEvmKey,
        usdc_token: &EvmAddress,
        to: &EvmAddress,
        amount_micro: u64,
        chain_id: u64,
    ) -> Result<FfiUsdcSendResult, LiveSwapFfiError> {
        if amount_micro == 0 {
            return Err(bad("amount must be non-zero"));
        }
        if *to == [0u8; 20] {
            return Err(bad("recipient address must not be zero"));
        }
        let from = key.address();

        // 1. USDC balance preflight: amount ≤ balanceOf(sender). No cap beyond the balance itself.
        let bal_word = chain
            .call(usdc_token, &erc20_balance_of(&from))
            .map_err(transport)?;
        let balance = word_to_u128(&bal_word).ok_or_else(|| bad("malformed balanceOf response"))?;
        if u128::from(amount_micro) > balance {
            return Err(bad(format!(
                "amount {amount_micro} exceeds USDC balance {balance}"
            )));
        }

        // 2. nonce + gas price (clamped into the sane band).
        let nonce = chain.transaction_count(&from).map_err(transport)?;
        let gas_price = chain
            .gas_price()
            .map_err(transport)?
            .clamp(USDC_SEND_MIN_GAS_PRICE_WEI, USDC_SEND_MAX_GAS_PRICE_WEI);

        // 3. gas limit: estimate + 25 % buffer, capped; fixed fallback when the estimate is missing.
        let data = erc20_transfer(to, amount_micro);
        let gas_limit = match chain.estimate_gas(&from, usdc_token, &data) {
            Ok(est) => est
                .saturating_add(est / 4)
                .clamp(21_000, USDC_TRANSFER_GAS_MAX),
            Err(_) => USDC_TRANSFER_GAS_FALLBACK,
        };

        // 4. POL gas preflight: the sender's native balance must cover gas_limit × gas_price.
        let gas_budget = u128::from(gas_limit).saturating_mul(u128::from(gas_price));
        let pol = chain.balance(&from).map_err(transport)?;
        if pol < gas_budget {
            return Err(bad(format!(
                "insufficient POL for gas: need {gas_budget} wei, have {pol}"
            )));
        }

        // 5. build + sign + broadcast. The named constructor makes the chain-id source auditable.
        let tx = if chain_id == POLYGON_MAINNET_CHAIN_ID {
            LegacyTx::polygon_mainnet(nonce, gas_price, gas_limit, *usdc_token, 0, &data)
        } else {
            LegacyTx::polygon_amoy(nonce, gas_price, gas_limit, *usdc_token, 0, &data)
        };
        let raw = tx.sign_with(key);
        let tx_hash = chain.send_raw(&raw).map_err(transport)?;

        Ok(FfiUsdcSendResult {
            tx_hash,
            from_address: format!("0x{}", bytes_to_hex(&from)),
            amount_micro,
            gas_limit,
            gas_price_wei: gas_price,
        })
    }
}

#[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
#[cfg(test)]
#[path = "usdc_send_ffi_tests.rs"]
mod tests;
