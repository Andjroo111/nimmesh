//! # live_amoy_verifier — prove the gateway-backed FundingVerifier against the REAL chain (#72 tail)
//!
//! `polygon_verifier::PolygonHtlcVerifier` is the first real-chain implementation of the G1
//! funding-verification seam — the gate that decides whether this node may fund/reveal. Its
//! logic is proven offline (#132); THIS run proves it against the live Amoy deployment, walking
//! one real escrow through all three verdicts:
//!
//! 1. fund a 1-USDC self-escrow on the forwarder-bound HTLC (our own stack, as always);
//! 2. `observe(expectation)` → **`Found { amount, timeout, confirmations }`** with REAL depth —
//!    poll until the depth clears [`ConfirmationPolicy::testnet_defaults`]'s USDC floor and
//!    [`require_funded`] passes (and assert the depth GROWS between reads: live blocks);
//! 3. observe with a WRONG hashlock while the escrow is live → **`Mismatch(Hashlock)`** — an
//!    escrow paying us under a different lock is never ours to advance on;
//! 4. `withdraw(S)`, re-observe → **`Absent`** — a resolved slot is not funding (the same
//!    stateless re-check that refuses a reorg-reburied escrow).
//!
//! **TESTNET ONLY.** Run: `cargo run --example live_amoy_verifier --features polygon-gateway`
//! Env: `AMOY_TEST_KEY` · `AMOY_HTLC2_ADDRESS` (or `AMOY_HTLC_ADDRESS`) · `AMOY_RPC_URL`/
//! `AMOY_USDC_ADDRESS` (defaults). See `docs/swap/AMOY.md`.

use std::error::Error;
use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nimmesh_core::evm::function_selector;
use nimmesh_core::evm_abi::{erc20_approve, htlc_new_swap, htlc_withdraw, word_address, WORD};
use nimmesh_core::evm_rlp::LegacyTx;
use nimmesh_core::evm_signer::LocalEvmKey;
use nimmesh_core::nimiq::hex::{bytes_to_hex, hex_to_bytes};
use nimmesh_core::polygon_gateway::{EvmReceipt, HttpPolygonRpc, DEFAULT_AMOY_RPC_URL};
use nimmesh_core::polygon_verifier::PolygonHtlcVerifier;
use nimmesh_core::swap_funding_verify::{
    require_funded, ConfirmationPolicy, FundingObservation, FundingVerifier, HtlcExpectation,
    MismatchReason,
};
use nimmesh_core::swap_intent::Asset;
use nimmesh_core::swap_leg::sha256;
use nimmesh_core::swap_usdc_leg::{usdc_swap_id, EvmAddress, MICRO_USDC};
use nimmesh_core::swap_wire::SwapLegId;

const DEFAULT_AMOY_USDC: &str = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582";
const EXPLORER_TX_BASE: &str = "https://amoy.polygonscan.com/tx";
const INCLUSION_TIMEOUT: Duration = Duration::from_secs(180);
const AMOUNT: u64 = MICRO_USDC; // 1 USDC
const MIN_GAS_PRICE: u64 = 30_000_000_000;
const MAX_GAS_PRICE: u64 = 50_000_000_000;
// Limits from RECEIPTS, not estimates (a 190k newSwap limit died out-of-gas — the escrow
// writes six cold slots + the proxied transferFrom; the receipt says 236k):
// approve ~58k · newSwap ~236k · withdraw ~92k.
const GAS_APPROVE: u64 = 70_000;
const GAS_NEW_SWAP: u64 = 280_000;
const GAS_WITHDRAW: u64 = 120_000;
/// ⚠️ The public Amoy RPC caps `eth_getLogs` ranges HARD — empirically ~50 blocks works and
/// 100 already errors ("block range exceeds configured limit"; two real runs learned this).
/// So the scan window is anchored at the FUNDING block, not a lookback: the verifier is
/// constructed after `newSwap` mines, scanning from a few blocks before it. Production wiring
/// pages from the contract's deploy block in cap-sized chunks (or uses a provider with real
/// limits) — recorded on #72.
const FROM_BLOCK_MARGIN: u64 = 5;

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
            if !receipt.success {
                return Err(format!("{label} reverted").into());
            }
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
    let secret_key: [u8; 32] =
        hex_to_bytes(env("AMOY_TEST_KEY").ok_or("AMOY_TEST_KEY not set")?.trim())?
            .as_slice()
            .try_into()
            .map_err(|_| "AMOY_TEST_KEY must be 32 bytes of hex")?;
    let key = LocalEvmKey::from_secret(&secret_key).map_err(|_| "bad secret")?;
    let me = key.address();

    let rpc_url = env("AMOY_RPC_URL").unwrap_or_else(|| DEFAULT_AMOY_RPC_URL.to_string());
    let htlc = parse_addr(
        &env("AMOY_HTLC2_ADDRESS")
            .or_else(|| env("AMOY_HTLC_ADDRESS"))
            .ok_or("AMOY_HTLC2_ADDRESS not set")?,
    )?;
    let usdc = parse_addr(&env("AMOY_USDC_ADDRESS").unwrap_or_else(|| DEFAULT_AMOY_USDC.into()))?;

    println!("live_amoy_verifier — the #72 FundingVerifier against the REAL Amoy HTLC");
    println!("   us:   {}", addr_hex(&me));
    println!("   htlc: {}", addr_hex(&htlc));

    let rpc = HttpPolygonRpc::new(rpc_url.clone()).map_err(|e| format!("{e:?}"))?;
    let gas_price = rpc.gas_price()?.clamp(MIN_GAS_PRICE, MAX_GAS_PRICE);
    let mut nonce = rpc.get_transaction_count(&addr_hex(&me))?;
    let budget = (GAS_APPROVE + GAS_NEW_SWAP + GAS_WITHDRAW).saturating_mul(gas_price);
    if rpc.get_balance(&addr_hex(&me))? < u128::from(budget) {
        return Err(format!("need ~{budget} wei of POL for the flow").into());
    }

    let policy = ConfirmationPolicy::testnet_defaults().required(Asset::Usdc);
    println!("   required depth (testnet USDC): {policy}");

    // ── 1. a real escrow to verify ───────────────────────────────────────────────────────────
    let mut seed = secret_key.to_vec();
    seed.extend_from_slice(&now_unix().to_be_bytes());
    seed.extend_from_slice(b"verifier-proof");
    let secret = sha256(&seed);
    let hashlock = sha256(&secret);
    let timelock = now_unix() + 3_600;
    println!("1) fund a {AMOUNT} micro-USDC self-escrow (timelock +1h)");
    send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_APPROVE,
        &usdc,
        &erc20_approve(&htlc, AMOUNT),
        "approve",
    )?;
    let fund_receipt = send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_NEW_SWAP,
        &htlc,
        &htlc_new_swap(&me, AMOUNT, &hashlock, timelock),
        "newSwap",
    )?;
    let fund_block = fund_receipt.block_number;

    // The verifier under test — its own rpc client, scanning from just before the funding
    // block (the public RPC's tiny getLogs cap makes lookback windows useless — see above).
    let from_block = fund_block.saturating_sub(FROM_BLOCK_MARGIN);
    let verifier = PolygonHtlcVerifier::new(
        HttpPolygonRpc::new(rpc_url).map_err(|e| format!("{e:?}"))?,
        htlc,
        from_block,
    );
    println!("   verifier scans from block {from_block}");

    // Preflight the log query LOUDLY: the verifier itself is fail-closed (an RPC error reads
    // `Absent`, which can only delay a swap) — right for the money path, wrong for diagnosing a
    // demo. Surface range-cap/config errors here instead of looping on a silent Absent.
    use nimmesh_core::polygon_verifier::new_swap_topic0;
    rpc.get_logs(
        &addr_hex(&htlc),
        &format!("0x{}", bytes_to_hex(&new_swap_topic0())),
        Some(&format!("0x{}", bytes_to_hex(&word_address(&me)))),
        from_block,
    )
    .map_err(|e| format!("getLogs preflight failed (range cap? RPC?): {e:?}"))?;

    let expect = HtlcExpectation {
        leg: SwapLegId::Counterparty,
        hashlock,
        min_amount: AMOUNT,
        min_timeout: timelock,
        recipient: me.to_vec(),
        term_anchor: 0,
    };

    // ── 2. Found, with REAL depth, until the gate passes ────────────────────────────────────
    let mut last_depth = 0u32;
    let started = Instant::now();
    loop {
        match verifier.observe(&expect) {
            FundingObservation::Found {
                amount,
                timeout,
                confirmations,
            } => {
                if amount != AMOUNT || timeout != timelock {
                    return Err("Found with wrong terms".into());
                }
                if confirmations < last_depth {
                    return Err("depth went backwards without a reorg".into());
                }
                let gate = require_funded(&verifier.observe(&expect), &expect, policy);
                println!(
                    "   observe → Found {{ amount: {amount}, timeout: {timeout}, depth: {confirmations} }} · gate: {}",
                    if gate.is_ok() { "PASS" } else { "not yet (TooShallow)" }
                );
                if gate.is_ok() && confirmations > last_depth && last_depth > 0 {
                    println!("2) Found with growing real depth; require_funded PASSED at policy depth {policy}");
                    break;
                }
                last_depth = confirmations;
            }
            other => println!("   observe → {other:?} (log propagation…)"),
        }
        if started.elapsed() > Duration::from_secs(120) {
            return Err("verifier never reached the policy depth".into());
        }
        sleep(Duration::from_secs(4));
    }

    // ── 3. a wrong hashlock while the escrow is live → Mismatch ─────────────────────────────
    let mut wrong = expect.clone();
    wrong.hashlock = sha256(&[0xEE; 32]);
    match verifier.observe(&wrong) {
        FundingObservation::Mismatch(MismatchReason::Hashlock) => {
            println!(
                "3) wrong-hashlock expectation → Mismatch(Hashlock) — never ours to advance on"
            );
        }
        other => return Err(format!("expected Mismatch(Hashlock), got {other:?}").into()),
    }

    // ── 4. resolve the escrow; a resolved slot is not funding ───────────────────────────────
    let swap_id = usdc_swap_id(&me, &me, AMOUNT, &hashlock, timelock);
    send(
        &rpc,
        &key,
        &mut nonce,
        gas_price,
        GAS_WITHDRAW,
        &htlc,
        &htlc_withdraw(&swap_id, &secret),
        "withdraw",
    )?;
    match verifier.observe(&expect) {
        FundingObservation::Absent => {
            println!(
                "4) after withdraw → Absent — a resolved slot is not funding (stateless re-check)"
            );
        }
        other => return Err(format!("expected Absent after resolution, got {other:?}").into()),
    }

    // Sanity: the USDC came home (self-swap).
    let mut cd = function_selector("balanceOf(address)").to_vec();
    cd.extend_from_slice(&word_address(&me));
    let out = rpc.eth_call(&addr_hex(&usdc), &format!("0x{}", bytes_to_hex(&cd)))?;
    let bal = hex_to_bytes(&out)?;
    let mut be = [0u8; 8];
    be.copy_from_slice(&bal[WORD - 8..WORD]);
    println!("   balance: {} micro-USDC", u64::from_be_bytes(be));

    println!("\n#72 live verifier proof COMPLETE — the G1 gate reads the real chain correctly.");
    Ok(())
}
