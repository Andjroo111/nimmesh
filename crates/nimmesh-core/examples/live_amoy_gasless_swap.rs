//! # live_amoy_gasless_swap — prove relayer-sponsored (GASLESS) funding on the real Amoy testnet
//!
//! The **G7 (#78)** live proof of ADR-0006/0008: a USER who holds USDC and **zero POL** — an
//! account that has NEVER sent a transaction and never will — funds a real HTLC escrow, and a
//! RELAYER pays every drop of gas:
//!
//! 1. the user's identity is derived **in-process** (the repo's ephemeral-test-key pattern —
//!    the seed never leaves memory, is never persisted, never printed);
//! 2. the relayer sends the user 1 USDC (the user must OWN the tokens the permit spends);
//! 3. the user SIGNS (never submits): an EIP-2612 **permit** + an EIP-712 **ForwardRequest**
//!    wrapping `newSwapWithPermit` calldata ([`crate::evm_permit`] + [`crate::evm_forward`]);
//! 4. the relayer submits `NimmeshForwarder.execute(…)` — on-chain, `getSwap` shows the
//!    escrow's **funder == the USER** (ERC-2771 attribution), not the relayer or forwarder;
//! 5. the user hands `S` to the relayer (ADR-0006's sharp edge: that IS a reveal — the
//!    fallback rule applies), and the relayer lands the **caller-open** `withdraw(S)` directly
//!    — the claim path needs no forwarder at all (ADR-0007);
//! 6. **the proof**: `eth_getTransactionCount(user) == 0` at the END — the user funded and
//!    settled a real on-chain swap having never sent a single transaction.
//!
//! **TESTNET ONLY.** Run: `cargo run --example live_amoy_gasless_swap --features polygon-gateway`
//! Env: `AMOY_TEST_KEY` (the relayer) · `AMOY_FORWARDER_ADDRESS` · `AMOY_HTLC2_ADDRESS` (the
//! forwarder-bound HTLC) · `AMOY_RPC_URL`/`AMOY_USDC_ADDRESS` (defaults). See `docs/swap/AMOY.md`.

use std::error::Error;
use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nimmesh_core::evm::function_selector;
use nimmesh_core::evm_abi::{
    erc20_domain_separator, erc20_nonces, htlc_new_swap_with_permit, htlc_withdraw, word_address,
    word_u256, WORD,
};
use nimmesh_core::evm_forward::{forward_request_digest, forwarder_execute};
use nimmesh_core::evm_permit::{permit_digest, permit_sig_v};
use nimmesh_core::evm_rlp::{EvmSigner, LegacyTx};
use nimmesh_core::evm_signer::LocalEvmKey;
use nimmesh_core::nimiq::hex::{bytes_to_hex, hex_to_bytes};
use nimmesh_core::polygon_gateway::{EvmReceipt, HttpPolygonRpc, DEFAULT_AMOY_RPC_URL};
use nimmesh_core::swap_leg::sha256;
use nimmesh_core::swap_usdc_leg::{usdc_swap_id, EvmAddress, MICRO_USDC};

const DEFAULT_AMOY_USDC: &str = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582";
const EXPLORER_TX_BASE: &str = "https://amoy.polygonscan.com/tx";
const INCLUSION_TIMEOUT: Duration = Duration::from_secs(180);
const AMOUNT: u64 = MICRO_USDC; // 1 USDC through the whole gasless path
const MIN_GAS_PRICE: u64 = 30_000_000_000;
const MAX_GAS_PRICE: u64 = 50_000_000_000;
const GAS_TRANSFER: u64 = 90_000;
/// The inner call's gas (signed into the request) + the outer limit with forwarder overhead.
const FWD_INNER_GAS: u64 = 350_000;
const GAS_EXECUTE: u64 = 450_000;
const GAS_WITHDRAW: u64 = 170_000;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn parse_addr(s: &str) -> Result<EvmAddress, Box<dyn Error>> {
    let bytes = hex_to_bytes(s.trim())?;
    Ok(bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("address must be 20 bytes: {s}"))?)
}

fn addr_hex(a: &EvmAddress) -> String {
    format!("0x{}", bytes_to_hex(a))
}

fn call(rpc: &HttpPolygonRpc, to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let out = rpc.eth_call(&addr_hex(to), &format!("0x{}", bytes_to_hex(data)))?;
    Ok(hex_to_bytes(&out)?)
}

fn word_to_u64(word: &[u8]) -> u64 {
    let mut be = [0u8; 8];
    be.copy_from_slice(&word[WORD - 8..WORD]);
    u64::from_be_bytes(be)
}

fn word32(out: &[u8]) -> Result<[u8; 32], Box<dyn Error>> {
    Ok(out
        .get(..WORD)
        .ok_or("short word")?
        .try_into()
        .expect("32 bytes"))
}

fn usdc_balance(
    rpc: &HttpPolygonRpc,
    usdc: &EvmAddress,
    owner: &EvmAddress,
) -> Result<u64, Box<dyn Error>> {
    let mut cd = function_selector("balanceOf(address)").to_vec();
    cd.extend_from_slice(&word_address(owner));
    Ok(word_to_u64(&call(rpc, usdc, &cd)?[..WORD]))
}

#[allow(clippy::too_many_arguments)]
fn send(
    rpc: &HttpPolygonRpc,
    key: &LocalEvmKey,
    nonce: &mut u64,
    gas_price: u64,
    gas_limit: u64,
    to: &EvmAddress,
    data: &[u8],
    label: &str,
) -> Result<EvmReceipt, Box<dyn Error>> {
    let tx = LegacyTx::polygon_amoy(*nonce, gas_price, gas_limit, *to, 0, data);
    let raw = tx.sign_with(key);
    let hash = rpc.send_raw_transaction(&format!("0x{}", bytes_to_hex(&raw)))?;
    *nonce += 1;
    print!("   {label}: {EXPLORER_TX_BASE}/{hash} …");
    std::io::stdout().flush().ok();
    let started = Instant::now();
    loop {
        if let Some(receipt) = rpc.get_transaction_receipt(&hash)? {
            println!(
                " mined in block {} ({})",
                receipt.block_number,
                if receipt.success {
                    "success"
                } else {
                    "REVERTED"
                }
            );
            return Ok(receipt);
        }
        if started.elapsed() > INCLUSION_TIMEOUT {
            return Err(format!("{label}: tx {hash} not mined in time").into());
        }
        sleep(Duration::from_secs(2));
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
}

fn main() -> Result<(), Box<dyn Error>> {
    // ── identities: the funded RELAYER (env) + the gasless USER (derived in-process) ────────
    let relayer_secret: [u8; 32] =
        hex_to_bytes(env("AMOY_TEST_KEY").ok_or("AMOY_TEST_KEY not set")?.trim())?
            .as_slice()
            .try_into()
            .map_err(|_| "AMOY_TEST_KEY must be 32 bytes of hex")?;
    let relayer = LocalEvmKey::from_secret(&relayer_secret).map_err(|_| "bad relayer secret")?;

    // The user's key never touches disk or stdout — sha256(relayer_secret ‖ tag), the same
    // in-process ephemeral-identity pattern the NIM live examples use.
    let mut seed = relayer_secret.to_vec();
    seed.extend_from_slice(b"nimmesh-g7-gasless-user-v1");
    let user = LocalEvmKey::from_secret(&sha256(&seed)).map_err(|_| "bad derived secret")?;

    let rpc_url = env("AMOY_RPC_URL").unwrap_or_else(|| DEFAULT_AMOY_RPC_URL.to_string());
    let rpc = HttpPolygonRpc::new(rpc_url.clone()).map_err(|e| format!("{e:?}"))?;
    let fwd = parse_addr(&env("AMOY_FORWARDER_ADDRESS").ok_or("AMOY_FORWARDER_ADDRESS not set")?)?;
    let htlc = parse_addr(&env("AMOY_HTLC2_ADDRESS").ok_or("AMOY_HTLC2_ADDRESS not set")?)?;
    let usdc = parse_addr(&env("AMOY_USDC_ADDRESS").unwrap_or_else(|| DEFAULT_AMOY_USDC.into()))?;

    println!("live_amoy_gasless_swap — G7 relayer-sponsored funding on the REAL Amoy testnet");
    println!("   relayer: {}", addr_hex(&relayer.address()));
    println!(
        "   user:    {} (derived in-process; holds NO POL)",
        addr_hex(&user.address())
    );
    println!("   fwd:     {}", addr_hex(&fwd));
    println!("   htlc:    {}", addr_hex(&htlc));

    let user_nonce_before = rpc.get_transaction_count(&addr_hex(&user.address()))?;
    let user_pol = rpc.get_balance(&addr_hex(&user.address()))?;
    println!("   user account nonce: {user_nonce_before} · user POL: {user_pol} wei");

    let gas_price = rpc.gas_price()?.clamp(MIN_GAS_PRICE, MAX_GAS_PRICE);
    let mut relayer_nonce = rpc.get_transaction_count(&addr_hex(&relayer.address()))?;
    let budget = (GAS_TRANSFER + GAS_EXECUTE + GAS_WITHDRAW).saturating_mul(gas_price);
    let pol = rpc.get_balance(&addr_hex(&relayer.address()))?;
    if pol < u128::from(budget) {
        return Err(format!("relayer needs ~{budget} wei of POL, has {pol}").into());
    }

    // ── 1. the relayer stakes the user with the USDC the permit will spend ──────────────────
    println!("1) relayer sends the user {AMOUNT} micro-USDC (the user's tokens to escrow)");
    if usdc_balance(&rpc, &usdc, &user.address())? < AMOUNT {
        let mut cd = function_selector("transfer(address,uint256)").to_vec();
        cd.extend_from_slice(&word_address(&user.address()));
        cd.extend_from_slice(&word_u256(AMOUNT));
        let receipt = send(
            &rpc,
            &relayer,
            &mut relayer_nonce,
            gas_price,
            GAS_TRANSFER,
            &usdc,
            &cd,
            "transfer",
        )?;
        if !receipt.success {
            return Err("USDC transfer to the user reverted".into());
        }
    } else {
        println!("   (user already staked from a prior run)");
    }

    // ── 2. the user SIGNS: permit + forward request (no transactions, ever) ─────────────────
    let deadline = now_unix() + 900;
    let timelock = now_unix() + 3_600;
    let mut s_seed = sha256(&seed).to_vec();
    s_seed.extend_from_slice(&now_unix().to_be_bytes());
    let secret = sha256(&s_seed);
    let hashlock = sha256(&secret);

    // Read the LIVE domains + nonces (never reconstruct what the chain can tell us).
    let usdc_domain = word32(&call(&rpc, &usdc, &erc20_domain_separator())?)?;
    let fwd_domain = word32(&call(&rpc, &fwd, &erc20_domain_separator())?)?;
    let usdc_nonce = word_to_u64(&call(&rpc, &usdc, &erc20_nonces(&user.address()))?[..WORD]);
    let fwd_nonce = word_to_u64(&call(&rpc, &fwd, &erc20_nonces(&user.address()))?[..WORD]);

    let pd = permit_digest(
        &usdc_domain,
        &user.address(),
        &htlc,
        AMOUNT,
        usdc_nonce,
        deadline,
    );
    let (pr, ps, prec) = user.sign_hash(pd);
    let inner = htlc_new_swap_with_permit(
        &relayer.address(), // recipient: the relayer side of this demo swap
        AMOUNT,
        &hashlock,
        timelock,
        deadline,
        permit_sig_v(prec),
        &pr,
        &ps,
    );
    let rd = forward_request_digest(
        &fwd_domain,
        &user.address(),
        &htlc,
        0,
        FWD_INNER_GAS,
        fwd_nonce,
        deadline,
        &inner,
    );
    let (rr, rs, rrec) = user.sign_hash(rd);
    println!(
        "2) user signed the permit + the forward request (fwd nonce {fwd_nonce}) — 0 txs sent"
    );

    // ── 3. the relayer relays; the chain attributes the funder to the USER ──────────────────
    let cd = forwarder_execute(
        &user.address(),
        &htlc,
        0,
        FWD_INNER_GAS,
        fwd_nonce,
        deadline,
        &inner,
        permit_sig_v(rrec),
        &rr,
        &rs,
    );
    let receipt = send(
        &rpc,
        &relayer,
        &mut relayer_nonce,
        gas_price,
        GAS_EXECUTE,
        &fwd,
        &cd,
        "execute",
    )?;
    if !receipt.success {
        return Err("forwarder execute reverted".into());
    }
    let swap_id = usdc_swap_id(
        &user.address(),
        &relayer.address(),
        AMOUNT,
        &hashlock,
        timelock,
    );
    let mut cd = function_selector("getSwap(bytes32)").to_vec();
    cd.extend_from_slice(&swap_id);
    let view = call(&rpc, &htlc, &cd)?;
    if view.len() < 6 * WORD {
        return Err("getSwap short response".into());
    }
    let funder: EvmAddress = view[12..32].try_into().expect("20 bytes");
    let state = word_to_u64(&view[5 * WORD..6 * WORD]);
    if funder != user.address() {
        return Err(format!("funder is {} — attribution failed", addr_hex(&funder)).into());
    }
    if state != 1 {
        return Err("escrow not Live".into());
    }
    println!("3) getSwap: Live, funder == the USER (ERC-2771 attribution holds on-chain)");

    // ── 4. the user hands S to the relayer; the caller-open claim needs no forwarder ────────
    let receipt = send(
        &rpc,
        &relayer,
        &mut relayer_nonce,
        gas_price,
        GAS_WITHDRAW,
        &htlc,
        &htlc_withdraw(&swap_id, &secret),
        "withdraw",
    )?;
    if !receipt.success {
        return Err("withdraw reverted".into());
    }
    println!(
        "4) caller-open withdraw(S) by the relayer → Claimed (no forwarder on the claim path)"
    );

    // ── 5. THE PROOF: the user funded + settled a real swap and never sent a transaction ────
    let user_nonce_after = rpc.get_transaction_count(&addr_hex(&user.address()))?;
    let user_pol_after = rpc.get_balance(&addr_hex(&user.address()))?;
    if user_nonce_after != 0 || user_pol_after != 0 {
        return Err(format!(
            "gasless proof failed: user nonce {user_nonce_after}, POL {user_pol_after}"
        )
        .into());
    }
    println!("5) PROOF: user account nonce == 0 and POL == 0 — fully relayer-sponsored.");
    println!("\nG7 gasless round-trip COMPLETE — ADR-0006 implemented and live.");
    Ok(())
}
