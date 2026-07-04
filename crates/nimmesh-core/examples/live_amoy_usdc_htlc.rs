//! # live_amoy_usdc_htlc — prove the USDC HTLC leg end to end on the real Polygon Amoy TESTNET
//!
//! The **G6 (#77)** round-trip: drive the DEPLOYED `NimmeshHtlc` (G5, `contracts/`) with OUR core
//! for every byte — [`nimmesh_core::evm_abi`] calldata, [`nimmesh_core::evm_rlp`] EIP-155 legacy
//! tx, [`nimmesh_core::evm_signer`] k256 signing, [`nimmesh_core::polygon_gateway`] JSON-RPC:
//!
//! 1. ask the REAL contract for `swapIdFor(…)` and assert it matches the Rust model's
//!    [`usdc_swap_id`] **byte-for-byte** (the G5 vector test, now against the live chain);
//! 2. `approve` → `newSwap` (escrow 1 USDC under `H = SHA-256(S)`, 1-hour timelock) →
//!    `getSwap` reads back **Live** with our exact terms;
//! 3. `withdraw(S)` — the **SHA-256 precompile (0x02)** verifies the cross-chain lock on the
//!    real chain — `getSwap` reads back **Claimed**, the escrow returns to us (self-swap);
//! 4. a second, short-timelock swap: a PREMATURE `refund` **reverts on-chain** (receipt status
//!    0 — `TimeoutNotReached`), then once the timelock passes, `refund` succeeds → **Refunded**.
//!
//! Self-swap (funder = recipient = us) so one funded key suffices, but every byte matches what a
//! real NIM⇄USDC counterparty exchange broadcasts.
//!
//! **TESTNET ONLY** (play money). [`HttpPolygonRpc`] refuses mainnet hosts; the tx builder only
//! emits the Amoy chain id. Built only with `--features polygon-gateway`, so `cargo test` never
//! runs it, and CI never needs the network or a key.
//!
//! Run: `cargo run --example live_amoy_usdc_htlc --features polygon-gateway`
//! Env: `AMOY_TEST_KEY` (32-byte hex) · `AMOY_HTLC_ADDRESS` · `AMOY_RPC_URL` (default the public
//! Amoy endpoint) · `AMOY_USDC_ADDRESS` (default Circle's Amoy USDC). See `docs/swap/AMOY.md`.

use std::error::Error;
use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nimmesh_core::evm::function_selector;
use nimmesh_core::evm_abi::{
    erc20_approve, htlc_new_swap, htlc_refund, htlc_withdraw, word_address, word_u256, WORD,
};
use nimmesh_core::evm_rlp::LegacyTx;
use nimmesh_core::evm_signer::LocalEvmKey;
use nimmesh_core::nimiq::hex::{bytes_to_hex, hex_to_bytes};
use nimmesh_core::polygon_gateway::{EvmReceipt, HttpPolygonRpc, DEFAULT_AMOY_RPC_URL};
use nimmesh_core::swap_leg::sha256;
use nimmesh_core::swap_usdc_leg::{usdc_swap_id, EvmAddress, MICRO_USDC};

/// Circle's canonical USDC on Polygon PoS Amoy (developers.circle.com → USDC contract addresses).
const DEFAULT_AMOY_USDC: &str = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582";

const EXPLORER_TX_BASE: &str = "https://amoy.polygonscan.com/tx";
const INCLUSION_TIMEOUT: Duration = Duration::from_secs(180);
/// The happy-path escrow: 1 USDC.
const CLAIM_AMOUNT: u64 = MICRO_USDC;
/// The refund-path escrow: 0.5 USDC.
const REFUND_AMOUNT: u64 = MICRO_USDC / 2;
/// Amoy's enforced minimum gas price is 25 gwei; pad a little for inclusion speed.
const MIN_GAS_PRICE: u64 = 30_000_000_000;
/// Cap the node's `eth_gasPrice` suggestion: Amoy spikes it 3–4× during spam bursts (a real run
/// saw 84 gwei vs the ~25 baseline), which triples the POL burn for zero benefit — txs at the
/// floor still mine within a few blocks.
const MAX_GAS_PRICE: u64 = 50_000_000_000;
/// Per-call gas limits — upper bounds sized for the REAL, proxied USDC (FiatTokenProxy adds a
/// delegatecall + extra cold reads over a bare ERC-20: a 60k approve limit died out-of-gas on
/// Amoy). The node's upfront balance check is `limit × price`, so keep them honest, not huge.
const GAS_APPROVE: u64 = 90_000;
const GAS_NEW_SWAP: u64 = 280_000;
const GAS_WITHDRAW: u64 = 170_000;
const GAS_REFUND: u64 = 140_000;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn parse_addr(s: &str) -> Result<EvmAddress, Box<dyn Error>> {
    let bytes = hex_to_bytes(s.trim())?;
    let arr: EvmAddress = bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("address must be 20 bytes: {s}"))?;
    Ok(arr)
}

fn addr_hex(a: &EvmAddress) -> String {
    format!("0x{}", bytes_to_hex(a))
}

/// A read-only `eth_call`, returning the raw result bytes.
fn call(rpc: &HttpPolygonRpc, to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let out = rpc.eth_call(&addr_hex(to), &format!("0x{}", bytes_to_hex(data)))?;
    Ok(hex_to_bytes(&out)?)
}

/// The low 8 bytes of a 32-byte ABI word as `u64` (all our amounts/times/states fit).
fn word_to_u64(word: &[u8]) -> u64 {
    let mut be = [0u8; 8];
    be.copy_from_slice(&word[WORD - 8..WORD]);
    u64::from_be_bytes(be)
}

/// `balanceOf(address)` via the real token.
fn usdc_balance(
    rpc: &HttpPolygonRpc,
    usdc: &EvmAddress,
    owner: &EvmAddress,
) -> Result<u64, Box<dyn Error>> {
    let mut cd = function_selector("balanceOf(address)").to_vec();
    cd.extend_from_slice(&word_address(owner));
    let out = call(rpc, usdc, &cd)?;
    Ok(word_to_u64(&out[..WORD]))
}

/// One on-chain swap slot as `getSwap` returns it.
struct SwapView {
    sender: EvmAddress,
    receiver: EvmAddress,
    amount: u64,
    hashlock: [u8; 32],
    timelock: u64,
    /// 0 = None, 1 = Live, 2 = Claimed, 3 = Refunded.
    state: u64,
}

fn get_swap(
    rpc: &HttpPolygonRpc,
    htlc: &EvmAddress,
    swap_id: &[u8; 32],
) -> Result<SwapView, Box<dyn Error>> {
    let mut cd = function_selector("getSwap(bytes32)").to_vec();
    cd.extend_from_slice(swap_id);
    let out = call(rpc, htlc, &cd)?;
    if out.len() < 6 * WORD {
        return Err(format!("getSwap returned {} bytes, expected 192", out.len()).into());
    }
    let mut sender = [0u8; 20];
    sender.copy_from_slice(&out[12..32]);
    let mut receiver = [0u8; 20];
    receiver.copy_from_slice(&out[WORD + 12..2 * WORD]);
    let mut hashlock = [0u8; 32];
    hashlock.copy_from_slice(&out[3 * WORD..4 * WORD]);
    Ok(SwapView {
        sender,
        receiver,
        amount: word_to_u64(&out[2 * WORD..3 * WORD]),
        hashlock,
        timelock: word_to_u64(&out[4 * WORD..5 * WORD]),
        state: word_to_u64(&out[5 * WORD..6 * WORD]),
    })
}

/// Sign + broadcast one legacy tx through our own stack and poll until mined. Returns the
/// receipt WITHOUT asserting success — the premature-refund step wants the revert.
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
            return Err(
                format!("{label}: tx {hash} not mined within {INCLUSION_TIMEOUT:?}").into(),
            );
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
    // ── setup: key, endpoints, addresses ─────────────────────────────────────────────────────
    let secret_hex = env("AMOY_TEST_KEY").ok_or("AMOY_TEST_KEY not set (32-byte hex)")?;
    let secret: [u8; 32] = hex_to_bytes(secret_hex.trim())?
        .as_slice()
        .try_into()
        .map_err(|_| "AMOY_TEST_KEY must be 32 bytes of hex")?;
    let key = LocalEvmKey::from_secret(&secret).map_err(|_| "invalid secp256k1 secret")?;
    let me = key.address();

    let rpc_url = env("AMOY_RPC_URL").unwrap_or_else(|| DEFAULT_AMOY_RPC_URL.to_string());
    let rpc = HttpPolygonRpc::new(rpc_url.clone()).map_err(|e| format!("{e:?}"))?;
    let htlc = parse_addr(&env("AMOY_HTLC_ADDRESS").ok_or("AMOY_HTLC_ADDRESS not set")?)?;
    let usdc = parse_addr(&env("AMOY_USDC_ADDRESS").unwrap_or_else(|| DEFAULT_AMOY_USDC.into()))?;

    println!("live_amoy_usdc_htlc — the G6 round-trip on the REAL Polygon Amoy testnet");
    println!("   rpc:    {rpc_url}");
    println!("   us:     {}", addr_hex(&me));
    println!("   htlc:   {}", addr_hex(&htlc));
    println!("   usdc:   {}", addr_hex(&usdc));

    let start_balance = usdc_balance(&rpc, &usdc, &me)?;
    println!("   balance: {start_balance} micro-USDC");
    if start_balance < CLAIM_AMOUNT + REFUND_AMOUNT {
        return Err("need at least 1.5 USDC on the test key (faucet.circle.com)".into());
    }

    let suggested = rpc.gas_price()?;
    let gas_price = suggested.clamp(MIN_GAS_PRICE, MAX_GAS_PRICE);
    if suggested > MAX_GAS_PRICE {
        println!("   (node suggested {suggested} wei — clamped to {MAX_GAS_PRICE}; Amoy spike)");
    }
    let mut nonce = rpc.get_transaction_count(&addr_hex(&me))?;
    println!("   gas price: {gas_price} wei · starting nonce: {nonce}");

    // POL preflight: the node checks `limit × price` upfront PER TX, and the whole flow is
    // 7 sends — fail here with a number instead of dying mid-run with a swap left Live.
    let budget: u64 = (2 * GAS_APPROVE + 2 * GAS_NEW_SWAP + GAS_WITHDRAW + 2 * GAS_REFUND)
        .saturating_mul(gas_price);
    let pol = rpc.get_balance(&addr_hex(&me))?;
    if pol < u128::from(budget) {
        return Err(format!(
            "need ~{budget} wei of POL for the full flow at {gas_price} wei gas, have {pol} — \
             top up at faucet.polygon.technology"
        )
        .into());
    }

    // The revealed-on-chain secret; unique per run AND unpredictable to observers — it mixes the
    // PRIVATE key into the seed (time + address + nonce alone are readable off the chain, and a
    // hashlock anyone can preimage is no lock at all; a self-swap survives that, a real one doesn't).
    let mut seed = Vec::with_capacity(68);
    seed.extend_from_slice(&secret);
    seed.extend_from_slice(&now_unix().to_be_bytes());
    seed.extend_from_slice(&me);
    seed.extend_from_slice(&nonce.to_be_bytes());
    let swap_secret = sha256(&seed);
    let hashlock = sha256(&swap_secret);

    // ── 1. the swap-id byte-match, now against the LIVE contract ────────────────────────────
    let claim_timelock = now_unix() + 3_600;
    let local_id = usdc_swap_id(&me, &me, CLAIM_AMOUNT, &hashlock, claim_timelock);
    let mut cd = function_selector("swapIdFor(address,address,uint256,bytes32,uint256)").to_vec();
    cd.extend_from_slice(&word_address(&me));
    cd.extend_from_slice(&word_address(&me));
    cd.extend_from_slice(&word_u256(CLAIM_AMOUNT));
    cd.extend_from_slice(&hashlock);
    cd.extend_from_slice(&word_u256(claim_timelock));
    let chain_id = call(&rpc, &htlc, &cd)?;
    if chain_id.as_slice() != local_id.as_slice() {
        return Err("swapIdFor on the LIVE contract diverged from usdc_swap_id".into());
    }
    println!(
        "1) swap-id byte-match vs the live contract: 0x{}",
        bytes_to_hex(&local_id)
    );

    // ── 2. approve + newSwap → Live ──────────────────────────────────────────────────────────
    println!("2) escrow {CLAIM_AMOUNT} micro-USDC under H = SHA-256(S), timelock +1h");
    let receipt = send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_APPROVE,
        &usdc,
        &erc20_approve(&htlc, CLAIM_AMOUNT),
        "approve",
    )?;
    if !receipt.success {
        return Err("approve reverted".into());
    }
    let receipt = send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_NEW_SWAP,
        &htlc,
        &htlc_new_swap(&me, CLAIM_AMOUNT, &hashlock, claim_timelock),
        "newSwap",
    )?;
    if !receipt.success {
        return Err("newSwap reverted".into());
    }
    let view = get_swap(&rpc, &htlc, &local_id)?;
    if view.state != 1 || view.amount != CLAIM_AMOUNT || view.hashlock != hashlock {
        return Err("getSwap does not read back our Live terms".into());
    }
    if view.sender != me || view.receiver != me || view.timelock != claim_timelock {
        return Err("getSwap parties/timelock mismatch".into());
    }
    println!("   getSwap: Live, terms match");

    // ── 3. withdraw(S) — the 0x02 precompile on the real chain ──────────────────────────────
    let receipt = send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_WITHDRAW,
        &htlc,
        &htlc_withdraw(&local_id, &swap_secret),
        "withdraw",
    )?;
    if !receipt.success {
        return Err("withdraw reverted — SHA-256 lock failed on-chain".into());
    }
    let view = get_swap(&rpc, &htlc, &local_id)?;
    if view.state != 2 {
        return Err("swap not Claimed after withdraw".into());
    }
    println!("3) withdraw(S) verified by the SHA-256 precompile → Claimed");

    // ── 4. the refund path: premature refund REVERTS, post-timelock refund succeeds ─────────
    let refund_timelock = now_unix() + 75;
    let refund_id = usdc_swap_id(&me, &me, REFUND_AMOUNT, &hashlock, refund_timelock);
    println!("4) refund path: escrow {REFUND_AMOUNT} micro-USDC, timelock +75s");
    let receipt = send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_APPROVE,
        &usdc,
        &erc20_approve(&htlc, REFUND_AMOUNT),
        "approve",
    )?;
    if !receipt.success {
        return Err("approve (refund path) reverted".into());
    }
    let receipt = send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_NEW_SWAP,
        &htlc,
        &htlc_new_swap(&me, REFUND_AMOUNT, &hashlock, refund_timelock),
        "newSwap",
    )?;
    if !receipt.success {
        return Err("newSwap (refund path) reverted".into());
    }

    // Premature: the boundary second still belongs to the claimer → must revert on-chain.
    let receipt = send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_REFUND,
        &htlc,
        &htlc_refund(&refund_id),
        "refund (premature — expecting revert)",
    )?;
    if receipt.success {
        return Err("premature refund SUCCEEDED — timelock guard failed on-chain".into());
    }
    println!("   premature refund correctly REVERTED (TimeoutNotReached)");

    // Wait out the timelock (chain time tracks wall clock on Polygon), then refund for real.
    let wait_until = refund_timelock + 8;
    while now_unix() < wait_until {
        sleep(Duration::from_secs(4));
    }
    let receipt = send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_REFUND,
        &htlc,
        &htlc_refund(&refund_id),
        "refund",
    )?;
    if !receipt.success {
        return Err("post-timelock refund reverted".into());
    }
    let view = get_swap(&rpc, &htlc, &refund_id)?;
    if view.state != 3 {
        return Err("swap not Refunded after refund".into());
    }
    println!("   post-timelock refund → Refunded");

    // ── settle up: self-swap, so the full escrow came home (minus nothing in USDC) ───────────
    let end_balance = usdc_balance(&rpc, &usdc, &me)?;
    println!("   balance: {end_balance} micro-USDC (started {start_balance})");
    if end_balance != start_balance {
        return Err("USDC did not fully round-trip back to us".into());
    }

    println!(
        "\nG6 round-trip COMPLETE — the deployed contract enforces the swap on the real chain."
    );
    Ok(())
}
