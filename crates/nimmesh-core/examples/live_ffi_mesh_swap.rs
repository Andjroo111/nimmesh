//! # live_ffi_mesh_swap — the G10 live proof through the APP-FACING FFI constructors (G10c)
//!
//! Act 2 (`live_mesh_swap_nim_usdc`) proved the real NIM⇄USDC money path, but built its two
//! nodes through the RIG door (`MeshHarness::add_session_participant` → the caller-composed
//! session). This proof builds them through the EXACT `#[uniffi::export]` constructors the
//! shipping app + Mac node call:
//!
//! - the **initiator** (standing in for the phone) via
//!   [`MeshNode::new_live_swap_initiator`](nimmesh_core::node::MeshNode) — the same door
//!   `SwapMesh.swift`'s live path uses: the wallet enclave key funds the real NIM HTLC, the
//!   claimed USDC pays a derived receive address, a derived gas key lands `withdraw(S)`;
//! - the **responder** (the Mac rig) via
//!   [`MeshNode::new_live_swap_responder`] — the door `mac-node --swap-responder-live` uses.
//!
//! Both nodes are then `adopt`ed onto the deterministic mesh ether (BLE is already proven
//! live — this is about REAL MONEY through the REAL app constructors) and driven to
//! settlement, detected from CHAIN TRUTH (both claim addresses funded), with the C1/M3/M4
//! money-path safety baked into the constructors themselves.
//!
//! **TESTNET/AMOY ONLY.** Run:
//! `cargo run --example live_ffi_mesh_swap --features "polygon-gateway gateway-rpc"`
//! Env (same as A2c): `NIMMESH_NIM_SEED` (funded treasury), `AMOY_TEST_KEY`,
//! `AMOY_HTLC2_ADDRESS`; optional `AMOY_RPC_URL`, `AMOY_USDC_ADDRESS`,
//! `NIMIQ_NIMMESH_TESTNET_RPC_URL`, `NIMMESH_G10C_STATE`.

use std::error::Error;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nimmesh_core::evm::{function_selector, keccak256};
use nimmesh_core::evm_abi::{word_address, WORD};
use nimmesh_core::evm_rlp::LegacyTx;
use nimmesh_core::evm_signer::LocalEvmKey;
use nimmesh_core::live_swap_signer::AmoyChain;
use nimmesh_core::mock_radio::{MeshHarness, MockRadio};
use nimmesh_core::nimiq::address::Address;
use nimmesh_core::nimiq::hex::{bytes_to_hex, hex_to_bytes};
use nimmesh_core::nimiq::htlc::{timeout_resolve_proof, HtlcRedeem};
use nimmesh_core::nimiq::signer::{
    signed_transfer_wire, AppSigner, EnclaveKey, InMemoryEnclaveKey, TransferIntent,
};
use nimmesh_core::node::MeshNode;
use nimmesh_core::polygon_gateway::{HttpPolygonRpc, DEFAULT_AMOY_RPC_URL};
use nimmesh_core::rpc::{GatewayRpc, HttpGatewayRpc, DEFAULT_TESTNET_RPC_URL};
use nimmesh_core::swap_live_ffi::{FfiLiveInitiatorConfig, FfiLiveResponderConfig, LiveLockBook};
use nimmesh_core::swap_usdc_leg::EvmAddress;
use nimmesh_core::NetworkId;

/// 5 tNIM, in luna — what the phone-initiator gives.
const GIVE_LUNA: u64 = 500_000;
/// 1 USDC, in micro-USDC — what it takes.
const TAKE_MICRO_USDC: u64 = 1_000_000;
const DEFAULT_AMOY_USDC: &str = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582";
const TICK: Duration = Duration::from_secs(3);
const RUN_BUDGET_TICKS: u32 = 260;
const NIM_EXPLORER: &str = "https://nimiq-testnet.observer/transactions";
const AMOY_EXPLORER: &str = "https://amoy.polygonscan.com/tx";
const FAUCET_URL: &str = "https://faucet.pos.nimiq-testnet.com/tapit";

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}
fn parse_evm_addr(s: &str) -> Result<EvmAddress, Box<dyn Error>> {
    Ok(hex_to_bytes(s.trim())?
        .as_slice()
        .try_into()
        .map_err(|_| format!("address must be 20 bytes: {s}"))?)
}

fn seed32(hex: &str, what: &str) -> Result<[u8; 32], Box<dyn Error>> {
    Ok(hex_to_bytes(hex.trim())?
        .as_slice()
        .try_into()
        .map_err(|_| format!("{what} must be 32 bytes of hex"))?)
}

/// Derive a labelled 32-byte child secret (never printed; recoverable from the parent).
fn derive(parent: &[u8; 32], label: &str) -> [u8; 32] {
    let mut buf = parent.to_vec();
    buf.extend_from_slice(label.as_bytes());
    nimmesh_core::swap_leg::sha256(&buf)
}

fn nim_balance(rpc: &HttpGatewayRpc, addr: &str) -> u64 {
    for attempt in 0..3u8 {
        match rpc.get_account(addr) {
            Ok(acct) => return acct.map(|a| a.balance).unwrap_or(0),
            Err(e) => {
                eprintln!("[balance] read failed for {addr} (attempt {attempt}): {e}");
                sleep(Duration::from_secs(2));
            }
        }
    }
    0
}

fn usdc_balance(
    amoy: &HttpPolygonRpc,
    usdc: &EvmAddress,
    owner: &EvmAddress,
) -> Result<u64, Box<dyn Error>> {
    let mut cd = function_selector("balanceOf(address)").to_vec();
    cd.extend_from_slice(&word_address(owner));
    let out = AmoyChain::call(amoy, usdc, &cd)?;
    let mut be = [0u8; 8];
    be.copy_from_slice(&out[WORD - 8..WORD]);
    Ok(u64::from_be_bytes(be))
}

fn tap_faucet(rpc: &HttpGatewayRpc, address: &str) {
    println!("[faucet] tapping for {address} …");
    match ureq::post(FAUCET_URL).send_form(&[("address", address)]) {
        Ok(resp) => println!("[faucet] HTTP {}", resp.status()),
        Err(e) => println!("[faucet] WARN tap failed: {e}"),
    }
    for _ in 0..20 {
        if nim_balance(rpc, address) >= GIVE_LUNA + 100_000 {
            return;
        }
        sleep(Duration::from_secs(3));
    }
}

// --- the never-strand state file (mirrors A2c) -----------------------------------------------

fn state_path() -> std::path::PathBuf {
    env("NIMMESH_G10C_STATE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".nimmesh-g10c-state.json")
        })
}
fn save_state(v: &serde_json::Value) {
    if let Err(e) = std::fs::write(state_path(), v.to_string()) {
        eprintln!("WARN: could not persist the lock state: {e}");
    }
}
fn load_state() -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(state_path()).ok()?).ok()
}

/// Recover a NIM HTLC a previous run left locked: refund past its timeout, else refuse to
/// start (never more than one swap's funds in flight). The USDC escrow's own refund is the
/// A2c example's job; here the initiator's lock book IS the NIM refund source.
fn recover_previous_run(
    nim_rpc: &HttpGatewayRpc,
    treasury: &InMemoryEnclaveKey,
    treasury_addr: &Address,
) -> Result<(), Box<dyn Error>> {
    let Some(state) = load_state() else {
        return Ok(());
    };
    println!("[recover] a previous run left state at {:?}", state_path());
    if let Some(nim) = state
        .get("nim")
        .filter(|n| !n["resolved"].as_bool().unwrap_or(false))
    {
        let contract = Address::from_user_friendly(nim["contract"].as_str().unwrap_or(""))?;
        let timeout_ms = nim["timeout_ms"].as_u64().unwrap_or(0);
        let live = nim_rpc
            .get_account(&contract.to_user_friendly())?
            .map(|a| a.balance)
            .unwrap_or(0);
        if live == 0 {
            println!(
                "[recover] NIM HTLC {} already resolved",
                contract.to_user_friendly()
            );
        } else if now_ms() > timeout_ms + 60_000 {
            println!(
                "[recover] refunding the NIM HTLC {} …",
                contract.to_user_friendly()
            );
            let head = nim_rpc.block_number()?;
            let redeem = HtlcRedeem {
                contract,
                recipient: *treasury_addr,
                value: live,
                fee: 0,
                validity_start_height: head,
                network_id: NetworkId::Testnet.wire_id(),
            };
            let pk: [u8; 32] = treasury.public_key().try_into().map_err(|_| "pk")?;
            let sig: [u8; 64] = treasury
                .sign_content(redeem.serialize_content())
                .try_into()
                .map_err(|_| "sig")?;
            let wire = redeem.serialize_wire(&timeout_resolve_proof(&pk, &sig));
            let hash = nim_rpc.send_raw_transaction(&bytes_to_hex(&wire))?;
            println!("[recover] NIM refund broadcast: {NIM_EXPLORER}/{hash}");
            let mut emptied = false;
            for _ in 0..30 {
                sleep(Duration::from_secs(3));
                if nim_rpc
                    .get_account(&contract.to_user_friendly())?
                    .map(|a| a.balance)
                    .unwrap_or(0)
                    == 0
                {
                    emptied = true;
                    break;
                }
            }
            if !emptied {
                return Err("the NIM refund did not confirm — lock kept on file; re-run".into());
            }
            println!("[recover] NIM refund CONFIRMED (contract emptied)");
        } else {
            return Err(format!(
                "previous run's NIM HTLC still locked until ~{timeout_ms} (unix ms) — re-run after"
            )
            .into());
        }
    }
    let _ = std::fs::remove_file(state_path());
    println!("[recover] clean — previous run resolved");
    Ok(())
}

/// The `Transfer(address,address,uint256)` topic0.
fn transfer_topic0() -> [u8; 32] {
    keccak256(b"Transfer(address,address,uint256)")
}

/// Find the Amoy escrow (NewSwap → `receiver`) + the withdraw tx (USDC Transfer → `receiver`)
/// from the last `lookback` blocks — receipts without an indexer.
fn amoy_receipts(
    amoy: &HttpPolygonRpc,
    htlc: &EvmAddress,
    usdc: &EvmAddress,
    receiver: &EvmAddress,
    lookback: u64,
) -> (String, String, String) {
    let head = amoy.block_number().unwrap_or(0);
    let from = head.saturating_sub(lookback);
    // NewSwap → receiver (topic1 = swapId; the log carries its own tx).
    let mut swap_id = String::new();
    let mut new_swap_tx = String::new();
    if let Ok(logs) = amoy.new_swap_logs_to(htlc, receiver, from) {
        if let Some(l) = logs.last() {
            swap_id = format!("0x{}", bytes_to_hex(&l.topic1));
            new_swap_tx = l.transaction_hash.clone();
        }
    }
    // USDC Transfer → receiver = the withdraw's payout.
    let mut receiver_topic = [0u8; 32];
    receiver_topic[12..].copy_from_slice(receiver);
    let withdraw_tx = amoy
        .get_logs(
            &format!("0x{}", bytes_to_hex(usdc)),
            &format!("0x{}", bytes_to_hex(&transfer_topic0())),
            Some(&format!("0x{}", bytes_to_hex(&receiver_topic))),
            from,
        )
        .ok()
        .and_then(|logs| logs.into_iter().last())
        .map(|l| l.transaction_hash)
        .unwrap_or_default();
    (swap_id, new_swap_tx, withdraw_tx)
}

fn main() -> Result<(), Box<dyn Error>> {
    println!(
        "== live_ffi_mesh_swap — REAL NIM⇄USDC swap through the APP-FACING FFI ctors (G10c) ==\n"
    );

    // ── keys + chain clients ─────────────────────────────────────────────────────────────────
    let nim_seed = seed32(
        &env("NIMMESH_NIM_SEED").ok_or("NIMMESH_NIM_SEED not set")?,
        "NIMMESH_NIM_SEED",
    )?;
    let amoy_secret = seed32(
        &env("AMOY_TEST_KEY").ok_or("AMOY_TEST_KEY not set")?,
        "AMOY_TEST_KEY",
    )?;
    let htlc = parse_evm_addr(&env("AMOY_HTLC2_ADDRESS").ok_or("AMOY_HTLC2_ADDRESS not set")?)?;
    let usdc =
        parse_evm_addr(&env("AMOY_USDC_ADDRESS").unwrap_or_else(|| DEFAULT_AMOY_USDC.into()))?;

    let nim_rpc_url =
        env("NIMIQ_NIMMESH_TESTNET_RPC_URL").unwrap_or_else(|| DEFAULT_TESTNET_RPC_URL.to_string());
    let nim_rpc = HttpGatewayRpc::new(&nim_rpc_url, NetworkId::Testnet)?;
    let amoy_url = env("AMOY_RPC_URL").unwrap_or_else(|| DEFAULT_AMOY_RPC_URL.to_string());
    let amoy = HttpPolygonRpc::new(amoy_url.clone()).map_err(|e| format!("{e:?}"))?;

    // The treasury: funds the phone-initiator's NIM leg (its enclave key) + the refund dest.
    let treasury = InMemoryEnclaveKey::from_secret(&nim_seed);
    let treasury_pk: [u8; 32] = treasury.public_key().try_into().map_err(|_| "pk")?;
    let treasury_addr = Address::from_public_key(&treasury_pk);
    let treasury_nq = treasury_addr.to_user_friendly();

    // The funded Amoy wallet backs the responder's escrow + its own gas. The initiator's gas
    // key is a DERIVED account (distinct from the responder's) so the two never share a nonce.
    let evm_funded_addr = LocalEvmKey::from_secret(&amoy_secret)
        .map_err(|_| "key")?
        .address();
    let init_gas_secret = derive(&amoy_secret, "nimmesh-g10c-init-gas-v1");
    let init_gas_addr = LocalEvmKey::from_secret(&init_gas_secret)
        .map_err(|_| "gas key")?
        .address();
    // The initiator's USDC PAYOUT (claim) address — distinct, derived, recoverable.
    let init_claim_secret = derive(&amoy_secret, "nimmesh-g10c-init-claim-v1");
    let init_claim_addr = LocalEvmKey::from_secret(&init_claim_secret)
        .map_err(|_| "claim key")?
        .address();

    // The responder's NIM claim seed (its session identity; claimed NIM sweeps home).
    let resp_claim_seed = derive(&nim_seed, "nimmesh-g10c-resp-claim-v1");
    let resp_claim_key = InMemoryEnclaveKey::from_secret(&resp_claim_seed);
    let resp_claim_pk: [u8; 32] = resp_claim_key.public_key().try_into().map_err(|_| "pk")?;
    let resp_claim_nq = Address::from_public_key(&resp_claim_pk).to_user_friendly();

    println!("   NIM rpc:         {nim_rpc_url}");
    println!("   Amoy rpc:        {amoy_url}");
    println!("   treasury (NIM):  {treasury_nq}");
    println!("   responder claim: {resp_claim_nq}");
    println!("   Amoy funded:     0x{}", bytes_to_hex(&evm_funded_addr));
    println!("   init gas addr:   0x{}", bytes_to_hex(&init_gas_addr));
    println!("   init claim addr: 0x{}", bytes_to_hex(&init_claim_addr));
    println!("   HTLC v2:         0x{}\n", bytes_to_hex(&htlc));

    recover_previous_run(&nim_rpc, &treasury, &treasury_addr)?;

    // ── preflight balances + gas top-ups ─────────────────────────────────────────────────────
    let mut nim_before = nim_balance(&nim_rpc, &treasury_nq);
    if nim_before < GIVE_LUNA + 100_000 {
        tap_faucet(&nim_rpc, &treasury_nq);
        nim_before = nim_balance(&nim_rpc, &treasury_nq);
        if nim_before < GIVE_LUNA {
            return Err("treasury still short of tNIM after the faucet tap".into());
        }
    }
    let resp_nim_before = nim_balance(&nim_rpc, &resp_claim_nq);
    let usdc_funded_before = usdc_balance(&amoy, &usdc, &evm_funded_addr)?;
    let usdc_claim_before = usdc_balance(&amoy, &usdc, &init_claim_addr)?;
    let pol_funded = AmoyChain::balance(&amoy, &evm_funded_addr)?;
    let pol_gas = AmoyChain::balance(&amoy, &init_gas_addr)?;
    if usdc_funded_before < TAKE_MICRO_USDC {
        return Err("the funded Amoy wallet holds < 1 USDC".into());
    }
    if pol_funded < 20_000_000_000_000_000u128 {
        return Err(
            format!("the funded Amoy wallet is low on POL ({pol_funded} wei) — top it up").into(),
        );
    }
    // The initiator's DERIVED gas account needs its own POL to land withdraw(S). If it is
    // empty, seed it from the funded wallet (a plain value transfer), then STOP if that fails.
    if pol_gas < 10_000_000_000_000_000u128 {
        println!("[gas] initiator gas account low ({pol_gas} wei) — seeding 0.02 POL from the funded wallet …");
        let funded_key = LocalEvmKey::from_secret(&amoy_secret).map_err(|_| "key")?;
        let nonce = AmoyChain::transaction_count(&amoy, &evm_funded_addr)?;
        let gas_price = AmoyChain::gas_price(&amoy)?.clamp(30_000_000_000, 50_000_000_000);
        let tx = LegacyTx::polygon_amoy(
            nonce,
            gas_price,
            21_000,
            init_gas_addr,
            20_000_000_000_000_000,
            &[],
        );
        let raw = tx.sign_with(&funded_key);
        match AmoyChain::send_raw(&amoy, &raw) {
            Ok(h) => println!("[gas] seed tx: {AMOY_EXPLORER}/{h}"),
            Err(e) => return Err(format!("could not seed the initiator gas account: {e}").into()),
        }
        for _ in 0..20 {
            sleep(Duration::from_secs(3));
            if AmoyChain::balance(&amoy, &init_gas_addr)? >= 10_000_000_000_000_000u128 {
                break;
            }
        }
        if AmoyChain::balance(&amoy, &init_gas_addr)? < 10_000_000_000_000_000u128 {
            return Err(
                "initiator gas account still short after the seed — POL faucet needs a human"
                    .into(),
            );
        }
    }
    println!(
        "\nBEFORE  treasury {nim_before} luna · resp-claim {resp_nim_before} luna · funded {usdc_funded_before} µUSDC · init-claim {usdc_claim_before} µUSDC · funded {pol_funded} wei POL\n"
    );

    // ── the two live participants — BUILT THROUGH THE REAL FFI CONSTRUCTORS ───────────────────
    let head = u64::from(nim_rpc.block_number().unwrap_or(0));
    let mut h = MeshHarness::new();

    // Initiator (the phone's ctor): its NIM funding key IS the treasury enclave key.
    let init_radio = MockRadio::new("phone", h.ether());
    let mut intent_seed = [0u8; 32];
    getrandom::getrandom(&mut intent_seed)?;
    let lock_book = LiveLockBook::new();
    let init_cfg = FfiLiveInitiatorConfig {
        nim_luna: GIVE_LUNA,
        usdc_micro: TAKE_MICRO_USDC,
        expiry_height: if head > 0 {
            head + 20_000
        } else {
            u64::MAX / 2
        },
        intent_seed: intent_seed.to_vec(),
        evm_claim_address: init_claim_addr.to_vec(),
        evm_gas_secret: init_gas_secret.to_vec(),
        nim_rpc_url: nim_rpc_url.clone(),
        amoy_rpc_url: amoy_url.clone(),
        htlc_address: format!("0x{}", bytes_to_hex(&htlc)),
        delta_safe_blocks: 0,
        min_claim_window_blocks: 0,
    };
    let initiator = MeshNode::new_live_swap_initiator(
        b"phone".to_vec(),
        init_radio.clone(),
        Arc::new(InMemoryEnclaveKey::from_secret(&nim_seed)),
        lock_book.clone(),
        init_cfg,
        None,
    )
    .map_err(|e| format!("initiator ctor refused: {e}"))?;
    let initiator = h.adopt("phone", initiator, init_radio);

    // Responder (the Mac rig's ctor): funded Amoy wallet + the derived NIM claim seed.
    let resp_radio = MockRadio::new("mac", h.ether());
    let resp_cfg = FfiLiveResponderConfig {
        usdc_micro: TAKE_MICRO_USDC,
        nim_luna: GIVE_LUNA,
        expiry_height: if head > 0 {
            head + 20_000
        } else {
            u64::MAX / 2
        },
        nim_claim_seed: resp_claim_seed.to_vec(),
        evm_funding_secret: amoy_secret.to_vec(),
        nim_rpc_url: nim_rpc_url.clone(),
        amoy_rpc_url: amoy_url.clone(),
        htlc_address: format!("0x{}", bytes_to_hex(&htlc)),
        usdc_address: format!("0x{}", bytes_to_hex(&usdc)),
        delta_safe_blocks: 0,
        min_claim_window_blocks: 0,
    };
    let responder = MeshNode::new_live_swap_responder(
        b"mac".to_vec(),
        resp_radio.clone(),
        resp_cfg,
        Some(nim_rpc_url.clone()), // the Mac rig also beacons the testnet head
    )
    .map_err(|e| format!("responder ctor refused: {e}"))?;
    let responder = h.adopt("mac", responder, resp_radio);

    h.connect("phone", "mac");
    println!(
        "mesh up: phone ⇄ mac over the deterministic ether — both built by the LIVE FFI ctors\n"
    );

    // ── drive to settlement, detected from CHAIN TRUTH ───────────────────────────────────────
    let mut nim_lock_saved = false;
    let mut settled = false;
    for tick in 0..RUN_BUDGET_TICKS {
        initiator.poll_sync();
        responder.poll_sync();
        initiator.poll_beacon();
        responder.poll_beacon();
        sleep(TICK);

        // Persist the NIM lock the moment the initiator's book records it (refund data only).
        if !nim_lock_saved {
            if let Some(lock) = lock_book.locks().first() {
                let state = serde_json::json!({
                    "nim": {
                        "contract": lock.contract,
                        "value": lock.value,
                        "timeout_ms": lock.timeout_ms,
                        "funding_tx": lock.funding_tx_hash,
                        "resolved": false,
                    }
                });
                save_state(&state);
                nim_lock_saved = true;
                println!(
                    "[t{tick:03}] NIM HTLC funded: {} · {NIM_EXPLORER}/{}",
                    lock.contract, lock.funding_tx_hash
                );
            }
        }

        let a = initiator
            .active_swaps()
            .first()
            .map(|m| format!("{:?}", m.phase));
        let b = responder
            .active_swaps()
            .first()
            .map(|m| format!("{:?}", m.phase));
        if tick % 4 == 0 || a.is_some() || b.is_some() {
            println!("[t{tick:03}] phone {a:?} · mac {b:?}");
        }

        // Chain truth: the responder's NIM claim funded AND the initiator's USDC claim
        // funded ⇒ the atomic swap settled (both `S`-claims landed on-chain).
        if nim_lock_saved && tick % 2 == 0 {
            let resp_nim = nim_balance(&nim_rpc, &resp_claim_nq);
            let init_usdc = usdc_balance(&amoy, &usdc, &init_claim_addr)?;
            if resp_nim >= resp_nim_before + GIVE_LUNA
                && init_usdc >= usdc_claim_before + TAKE_MICRO_USDC
            {
                settled = true;
                break;
            }
        }
    }
    h.shutdown();
    if !settled {
        return Err(format!(
            "the swap did not settle within the budget; the NIM lock (if any) is persisted at {:?} \
             — re-run to refund after the timelock",
            state_path()
        )
        .into());
    }
    println!("\nboth legs claimed on-chain — collecting receipts …");

    // ── receipts on both chains ──────────────────────────────────────────────────────────────
    let nim_lock = lock_book
        .locks()
        .into_iter()
        .next()
        .ok_or("no NIM lock recorded")?;
    let nim_claim_tx = nim_rpc
        .get_transactions(&resp_claim_nq, 10)
        .ok()
        .and_then(|rows| rows.into_iter().find(|r| r.value == GIVE_LUNA))
        .map(|r| r.hash)
        .unwrap_or_else(|| "(query the explorer for the claim tx)".into());
    let (swap_id, new_swap_tx, withdraw_tx) =
        amoy_receipts(&amoy, &htlc, &usdc, &init_claim_addr, 45);
    let usdc_claim_after = usdc_balance(&amoy, &usdc, &init_claim_addr)?;

    println!("\n=== G10c receipts (the ACTUAL app-facing FFI ctors) ===");
    println!(
        "  NIM HTLC {}  ({} luna)",
        nim_lock.contract, nim_lock.value
    );
    println!("  NIM funding: {NIM_EXPLORER}/{}", nim_lock.funding_tx_hash);
    println!("  NIM claim:   {NIM_EXPLORER}/{nim_claim_tx}");
    println!("  USDC escrow swapId: {swap_id}");
    if !new_swap_tx.is_empty() {
        println!("  Amoy newSwap:  {AMOY_EXPLORER}/{new_swap_tx}");
    }
    if !withdraw_tx.is_empty() {
        println!("  Amoy withdraw: {AMOY_EXPLORER}/{withdraw_tx}");
    }
    println!(
        "  init-claim USDC balance: {usdc_claim_after} µUSDC (+{})",
        usdc_claim_after - usdc_claim_before
    );

    // Mark the lock resolved (the swap settled — the NIM HTLC was claimed by the responder).
    let mut state = load_state().unwrap_or_else(|| serde_json::json!({ "nim": {} }));
    state["nim"]["resolved"] = serde_json::json!(true);
    save_state(&state);

    // ── sweep the responder's claimed NIM home to the treasury ───────────────────────────────
    let sweep_value = nim_balance(&nim_rpc, &resp_claim_nq);
    if sweep_value > 0 {
        println!("\nsweeping the responder's {sweep_value} luna home to the treasury …");
        let sweep_head = nim_rpc.block_number()?;
        let signed = AppSigner::new(Arc::new(InMemoryEnclaveKey::from_secret(&resp_claim_seed)))
            .sign_transfer(TransferIntent {
                recipient: treasury_nq.clone(),
                value: sweep_value,
                validity_start_height: sweep_head,
                network: NetworkId::Testnet,
            })?;
        match nim_rpc.send_raw_transaction(&bytes_to_hex(&signed_transfer_wire(&signed)?)) {
            Ok(h) => println!("  sweep: {NIM_EXPLORER}/{h}"),
            Err(e) => println!("  WARN sweep failed (funds are safe on {resp_claim_nq}): {e}"),
        }
    }

    let _ = std::fs::remove_file(state_path());
    println!("\n✅ G10c COMPLETE — a real NIM⇄USDC swap driven entirely through the app-facing FFI ctors.");
    Ok(())
}
