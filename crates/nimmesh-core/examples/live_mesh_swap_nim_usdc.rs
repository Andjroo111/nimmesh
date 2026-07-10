//! # live_mesh_swap_nim_usdc — a REAL two-node atomic swap over the REAL protocol (A2c)
//!
//! Act 2's headline proof: TWO participant [`MeshNode`](nimmesh_core::node::MeshNode)s over the
//! deterministic mesh harness drive a complete NIM⇄USDC atomic swap with **real money on both
//! testnets** — Albatross TESTNET on the NIM leg, Polygon **Amoy** on the USDC leg — end to end
//! through the shipping protocol, with no hand orchestration:
//!
//! 1. **discovery** — alice (NIM-giver) and bob (USDC-giver) advertise standing intents; alice's
//!    matcher initiates against bob's crossing intent → a signed `Propose` floods, bob `Accept`s;
//! 2. alice's [`LiveInitiatorSigner`] funds a REAL NIM HTLC (recipient = bob's protocol-carried
//!    claim address); bob's session is gated by the REAL [`NimHtlcVerifier`] — he funds NOTHING
//!    until the chain shows the exact creation at depth;
//! 3. bob's [`LiveResponderSigner`] escrows REAL USDC in the deployed `NimmeshHtlc` v2 (receiver
//!    = alice's protocol-carried EVM claim address); alice's [`AmoyHtlcSwapVerifier`] gates her
//!    reveal on the REAL escrow at depth;
//! 4. alice lands `withdraw(S)` on Amoy (the on-chain reveal); bob reads `S` off the
//!    `PreimageReveal` and claims the NIM HTLC; both nodes settle. Ground truth is then
//!    re-checked on BOTH chains, and bob's claimed NIM is swept home to the treasury wallet.
//!
//! **Re-runnable / never strands more than one swap:** every lock is persisted to a state file
//! the moment it exists; on startup an unresolved lock is refunded (once its timelock passed) or
//! the run refuses to start a new swap. BLE is already proven live — this proof is about REAL
//! MONEY over the REAL protocol, so the radio is the deterministic harness ether.
//!
//! **TESTNET/AMOY ONLY.** Run:
//! `cargo run --example live_mesh_swap_nim_usdc --features "polygon-gateway gateway-rpc"`
//! Env: `NIMMESH_NIM_SEED` (the funded treasury wallet) · `AMOY_TEST_KEY` ·
//! `AMOY_HTLC2_ADDRESS` (the forwarder-bound v2 deployment) · optional `AMOY_RPC_URL`,
//! `AMOY_USDC_ADDRESS`, `NIMIQ_NIMMESH_TESTNET_RPC_URL`, `NIMMESH_ACT2_STATE`.

use std::error::Error;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nimmesh_core::evm::function_selector;
use nimmesh_core::evm_abi::{htlc_refund, word_address, WORD};
use nimmesh_core::evm_rlp::LegacyTx;
use nimmesh_core::evm_signer::LocalEvmKey;
use nimmesh_core::live_swap_signer::{
    AmoyChain, AmoyHtlcSwapVerifier, EvmGasConfig, LiveInitiatorConfig, LiveInitiatorSigner,
    LivePollConfig, LiveResponderConfig, LiveResponderSigner, PeerBook, PolygonFundingStore,
};
use nimmesh_core::mock_radio::MeshHarness;
use nimmesh_core::nim_verifier::{NimFundingStore, NimHtlcVerifier};
use nimmesh_core::nimiq::address::Address;
use nimmesh_core::nimiq::hex::{bytes_to_hex, hex_to_bytes};
use nimmesh_core::nimiq::htlc::{timeout_resolve_proof, HtlcRedeem};
use nimmesh_core::nimiq::signer::{
    signed_transfer_wire, AppSigner, EnclaveKey, InMemoryEnclaveKey, TransferIntent,
};
use nimmesh_core::polygon_gateway::{HttpPolygonRpc, DEFAULT_AMOY_RPC_URL};
use nimmesh_core::rpc::{GatewayRpc, HttpGatewayRpc, DEFAULT_TESTNET_RPC_URL};
use nimmesh_core::swap::LadderParams;
use nimmesh_core::swap_coordinator::SwapContext;
use nimmesh_core::swap_intent::{sign_intent_ephemeral, Asset, FfiSwapPhase, SwapIntent};
use nimmesh_core::swap_leg::sha256;
use nimmesh_core::swap_session::{NodeIdentity, RatePolicy, SwapSession};
use nimmesh_core::swap_signer::SwapSigner;
use nimmesh_core::swap_usdc_leg::EvmAddress;
use nimmesh_core::swap_wire::{SwapLegId, NIM_ADDRESS_LEN, SWAP_ID_LEN};
use nimmesh_core::NetworkId;

/// A ONE-SHOT funding latch. Standing intents are continuous trade adverts by design (G37):
/// the moment a swap settles and is reaped, discovery happily matches the same counterparty
/// again and the signers would fund a SECOND real swap (run 1 proved it — see
/// docs/swap/ACT2-RECEIPTS.md). This rig run is a single-swap proof, so after the first
/// successful funding the wrapper refuses every further funding — the repeat negotiation then
/// dies unfunded (no money moves without a funding). Claims pass through untouched.
struct OneShot<S: SwapSigner> {
    inner: S,
    fired: std::sync::atomic::AtomicBool,
}

impl<S: SwapSigner> OneShot<S> {
    fn new(inner: S) -> Self {
        OneShot {
            inner,
            fired: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl<S: SwapSigner> SwapSigner for OneShot<S> {
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
        use std::sync::atomic::Ordering;
        if self.fired.load(Ordering::SeqCst) {
            return None; // one real swap per run — a repeat match stays unfunded
        }
        let out = self.inner.build_funding(ctx, leg);
        if out.is_some() {
            self.fired.store(true, Ordering::SeqCst);
        }
        out
    }
    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        self.inner.build_claim(ctx, secret)
    }
    fn note_peer(
        &self,
        swap_id: [u8; SWAP_ID_LEN],
        peer_nim_address: [u8; NIM_ADDRESS_LEN],
        peer_chain_address: &[u8],
    ) {
        self.inner
            .note_peer(swap_id, peer_nim_address, peer_chain_address);
    }
    fn is_live(&self) -> bool {
        self.inner.is_live() // C1: a wrapper must never hide a live signer
    }
}

/// 5 tNIM, in luna — what alice gives.
const GIVE_LUNA: u64 = 500_000;
/// 1 USDC, in micro-USDC — what alice takes.
const TAKE_MICRO_USDC: u64 = 1_000_000;
/// Circle's canonical Amoy USDC.
const DEFAULT_AMOY_USDC: &str = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582";
/// One protocol tick (both nodes' maintenance poll + phase read).
const TICK: Duration = Duration::from_secs(3);
/// The whole-run budget in ticks (~13 min) — loud failure + persisted state beyond it.
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

fn now_s() -> u64 {
    now_ms() / 1000
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
    sha256(&buf)
}

fn nim_addr_of(key: &dyn EnclaveKey) -> Result<[u8; 20], Box<dyn Error>> {
    let pk: [u8; 32] = key.public_key().try_into().map_err(|_| "bad pubkey")?;
    Ok(*Address::from_public_key(&pk).as_bytes())
}

// --- the never-strand state file ------------------------------------------------------------

fn state_path() -> std::path::PathBuf {
    env("NIMMESH_ACT2_STATE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".nimmesh-act2-state.json")
        })
}

fn save_state(v: &serde_json::Value) {
    if let Err(e) = std::fs::write(state_path(), v.to_string()) {
        eprintln!("WARN: could not persist the lock state: {e}");
    }
}

fn load_state() -> Option<serde_json::Value> {
    let bytes = std::fs::read(state_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Recover any lock a previous run left behind: refund what is past its timelock, refuse to
/// start while something is still locked (never more than one swap's funds in flight).
#[allow(clippy::too_many_arguments)]
fn recover_previous_run(
    nim_rpc: &HttpGatewayRpc,
    treasury: &InMemoryEnclaveKey,
    treasury_addr: &Address,
    amoy: &HttpPolygonRpc,
    evm_key: &LocalEvmKey,
    htlc: &EvmAddress,
    gas: &EvmGasConfig,
) -> Result<(), Box<dyn Error>> {
    let Some(state) = load_state() else {
        return Ok(());
    };
    println!("[recover] a previous run left state at {:?}", state_path());
    let mut blocked = false;

    // The NIM leg: refund via TimeoutResolve once the ms-timeout passed.
    if let Some(nim) = state
        .get("nim")
        .filter(|n| !n["resolved"].as_bool().unwrap_or(false))
    {
        let contract = Address::from_user_friendly(nim["contract"].as_str().unwrap_or(""))?;
        let value = nim["value"].as_u64().unwrap_or(0);
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
                value,
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
            // Only a VERIFIED refund (the contract actually emptied) lets the state entry go —
            // a mempool-lost refund must keep the lock on file, never silently forget it.
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
            println!(
                "[recover] NIM HTLC still locked until ~{} (unix ms) — re-run after that",
                timeout_ms
            );
            blocked = true;
        }
    }

    // The USDC leg: `refund(swapId)` once the seconds-timelock passed.
    if let Some(u) = state
        .get("usdc")
        .filter(|u| !u["resolved"].as_bool().unwrap_or(false))
    {
        let swap_id: [u8; 32] = hex_to_bytes(u["swap_id"].as_str().unwrap_or(""))?
            .as_slice()
            .try_into()
            .map_err(|_| "bad swap_id in state")?;
        let timelock_s = u["timelock_s"].as_u64().unwrap_or(0);
        let mut cd = function_selector("getSwap(bytes32)").to_vec();
        cd.extend_from_slice(&swap_id);
        let out = AmoyChain::call(amoy, htlc, &cd)?;
        let live = out.len() >= 6 * WORD && {
            let mut be = [0u8; 8];
            be.copy_from_slice(&out[5 * WORD + 24..6 * WORD]);
            u64::from_be_bytes(be) == 1
        };
        if !live {
            println!("[recover] USDC escrow already resolved");
        } else if now_s() > timelock_s + 8 {
            println!("[recover] refunding the USDC escrow …");
            let me = evm_key.address();
            let nonce = AmoyChain::transaction_count(amoy, &me)?;
            let gas_price = AmoyChain::gas_price(amoy)?.clamp(gas.min_gas_price, gas.max_gas_price);
            let refund_cd = htlc_refund(&swap_id);
            let tx = LegacyTx::polygon_amoy(
                nonce,
                gas_price,
                gas.gas_refund_reserve,
                *htlc,
                0,
                &refund_cd,
            );
            let raw = tx.sign_with(evm_key);
            let hash = AmoyChain::send_raw(amoy, &raw)?;
            println!("[recover] USDC refund broadcast: {AMOY_EXPLORER}/{hash}");
        } else {
            println!(
                "[recover] USDC escrow still locked until ~{timelock_s} (unix s) — re-run after"
            );
            blocked = true;
        }
    }

    if blocked {
        return Err(
            "previous run's funds are still time-locked — refusing to start a new swap".into(),
        );
    }
    let _ = std::fs::remove_file(state_path());
    println!("[recover] clean — previous run fully resolved");
    Ok(())
}

// --- chain readbacks --------------------------------------------------------------------------

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

fn nim_balance(rpc: &HttpGatewayRpc, addr: &str) -> u64 {
    // Retry transient RPC failures — a single blip must not render as "balance 0" in the
    // run report (run 2's AFTER line taught this the hard way).
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

/// Build + ephemerally sign a standing intent for this run.
#[allow(clippy::too_many_arguments)]
fn standing_intent(
    gives: Asset,
    nim_amount: u64,
    counter_amount: u64,
    seed: &[u8; 32],
    btc_address: Vec<u8>,
    evm_address: EvmAddress,
) -> SwapIntent {
    let mut intent = SwapIntent {
        gives,
        counter_asset: Asset::Usdc,
        nim_amount,
        btc_amount: counter_amount,
        expiry_height: u64::MAX, // the harness is beacon-silent (head 0) — never expires
        min_nim: 0,
        max_nim: u64::MAX,
        nim_pubkey: [0; 32],
        nim_address: [0; 20],
        btc_pubkey: {
            let mut k = [0x02u8; 33];
            k[1] = seed[0]; // distinct per party; unused on this pair
            k
        },
        btc_address,
        evm_address,
        network_id: NetworkId::Testnet.wire_id(),
        signature: [0; 64],
    };
    sign_intent_ephemeral(&mut intent, seed);
    intent
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("== live_mesh_swap_nim_usdc — REAL NIM⇄USDC atomic swap over the REAL protocol ==\n");

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
    let nim_rpc = Arc::new(HttpGatewayRpc::new(&nim_rpc_url, NetworkId::Testnet)?);
    let amoy_url = env("AMOY_RPC_URL").unwrap_or_else(|| DEFAULT_AMOY_RPC_URL.to_string());
    let amoy = Arc::new(HttpPolygonRpc::new(amoy_url.clone()).map_err(|e| format!("{e:?}"))?);

    // The treasury: funds alice's NIM leg and is every refund/sweep destination.
    let treasury = InMemoryEnclaveKey::from_secret(&nim_seed);
    let treasury_pk: [u8; 32] = treasury.public_key().try_into().map_err(|_| "pk")?;
    let treasury_addr = Address::from_public_key(&treasury_pk);
    let treasury_nq = treasury_addr.to_user_friendly();

    // The funded Amoy wallet: bob's escrow funder + alice's claim GAS payer (caller-open
    // withdraw, ADR-0007/0010). Alice's PAYOUT address is a distinct derived account.
    let evm_key = LocalEvmKey::from_secret(&amoy_secret).map_err(|_| "bad AMOY_TEST_KEY")?;
    let evm_funded_addr = evm_key.address();
    let alice_evm_claim =
        LocalEvmKey::from_secret(&derive(&amoy_secret, "nimmesh-act2-alice-claim-v1"))
            .map_err(|_| "bad derived key")?
            .address();

    // Session identities (deterministic labels → every payout stays recoverable) + bob's NIM
    // claim key, which IS his session identity key.
    let alice_session_key = Arc::new(InMemoryEnclaveKey::from_secret(&derive(
        &nim_seed,
        "nimmesh-act2-alice-session-v1",
    )));
    let bob_claim_key = Arc::new(InMemoryEnclaveKey::from_secret(&derive(
        &nim_seed,
        "nimmesh-act2-bob-claim-v1",
    )));
    let alice_nim_id = nim_addr_of(alice_session_key.as_ref())?;
    let bob_nim_claim = nim_addr_of(bob_claim_key.as_ref())?;
    let bob_claim_nq = Address::from_bytes(bob_nim_claim).to_user_friendly();

    let gas = EvmGasConfig::default();

    println!("   NIM rpc:        {nim_rpc_url}");
    println!("   Amoy rpc:       {amoy_url}");
    println!("   treasury (NIM): {treasury_nq}");
    println!("   bob NIM claim:  {bob_claim_nq}");
    println!("   Amoy funded:    0x{}", bytes_to_hex(&evm_funded_addr));
    println!("   alice EVM claim: 0x{}", bytes_to_hex(&alice_evm_claim));
    println!("   HTLC v2:        0x{}", bytes_to_hex(&htlc));

    // ── recover anything a dead run left locked BEFORE starting a new swap ───────────────────
    recover_previous_run(
        &nim_rpc,
        &treasury,
        &treasury_addr,
        &amoy,
        &evm_key,
        &htlc,
        &gas,
    )?;

    // ── preflight balances (BEFORE snapshot) ─────────────────────────────────────────────────
    let mut nim_before = nim_balance(&nim_rpc, &treasury_nq);
    if nim_before < GIVE_LUNA + 100_000 {
        tap_faucet(&nim_rpc, &treasury_nq);
        nim_before = nim_balance(&nim_rpc, &treasury_nq);
        if nim_before < GIVE_LUNA {
            return Err("treasury still short of tNIM after the faucet tap".into());
        }
    }
    let bob_nim_before = nim_balance(&nim_rpc, &bob_claim_nq);
    let usdc_funded_before = usdc_balance(&amoy, &usdc, &evm_funded_addr)?;
    let usdc_alice_before = usdc_balance(&amoy, &usdc, &alice_evm_claim)?;
    let pol_before = AmoyChain::balance(amoy.as_ref(), &evm_funded_addr)?;
    let budget = (gas.gas_approve + gas.gas_new_swap + gas.gas_withdraw + gas.gas_refund_reserve)
        .saturating_mul(gas.max_gas_price);
    if pol_before < u128::from(budget) {
        return Err(format!("need ~{budget} wei POL for the full flow, have {pol_before}").into());
    }
    if usdc_funded_before < TAKE_MICRO_USDC {
        return Err("the funded Amoy wallet holds < 1 USDC".into());
    }
    println!("\nBEFORE  treasury {nim_before} luna · bob-claim {bob_nim_before} luna · funded {usdc_funded_before} µUSDC · alice-claim {usdc_alice_before} µUSDC · {pol_before} wei POL\n");

    // ── the two live participants ────────────────────────────────────────────────────────────
    let alice_seed = derive(&nim_seed, "nimmesh-act2-alice-session-v1");
    let bob_seed = derive(&nim_seed, "nimmesh-act2-bob-claim-v1");
    let alice_intent = standing_intent(
        Asset::Nim,
        GIVE_LUNA,
        TAKE_MICRO_USDC,
        &alice_seed,
        alice_evm_claim.to_vec(),
        alice_evm_claim,
    );
    let bob_intent = standing_intent(
        Asset::Usdc,
        GIVE_LUNA,
        TAKE_MICRO_USDC,
        &bob_seed,
        evm_funded_addr.to_vec(),
        evm_funded_addr,
    );

    // A REAL per-run secret master: fresh CSPRNG entropy (never derived, never persisted).
    let mut secret_master = [0u8; 32];
    getrandom::getrandom(&mut secret_master)?;

    let alice_peer_book = Arc::new(PeerBook::new());
    let bob_peer_book = Arc::new(PeerBook::new());
    let polygon_store = Arc::new(PolygonFundingStore::new());
    let nim_store = Arc::new(NimFundingStore::new());
    let ladder = LadderParams::default();

    let alice_identity = NodeIdentity {
        nim_address: alice_nim_id,
        btc_address: alice_evm_claim.to_vec(),
        btc_pubkey: alice_intent.btc_pubkey,
        rate_policy: RatePolicy::accept_all(),
        max_concurrent_swaps: 4,
        standing_intent: Some(alice_intent),
    };
    let alice_session = SwapSession::new(alice_identity, ladder)
        .with_propose_signer(alice_session_key.clone())
        .with_secret_source(Box::new(move |swap_id| {
            let mut buf = secret_master.to_vec();
            buf.extend_from_slice(swap_id);
            buf.extend_from_slice(b"nimmesh-act2-secret-v1");
            sha256(&buf)
        }))
        .with_funding_verifier(Box::new(AmoyHtlcSwapVerifier::new(
            amoy.clone(),
            htlc,
            alice_evm_claim,
            polygon_store.clone(),
        )))
        .with_counterparty_chain(Asset::Usdc);
    let alice_signer = LiveInitiatorSigner::new(LiveInitiatorConfig {
        nim_key: Arc::new(InMemoryEnclaveKey::from_secret(&nim_seed)),
        nim_rpc: nim_rpc.clone() as Arc<dyn GatewayRpc>,
        evm_gas_key: LocalEvmKey::from_secret(&amoy_secret).map_err(|_| "key")?,
        amoy: amoy.clone(),
        htlc,
        store: polygon_store.clone(),
        peer_book: alice_peer_book,
        gas,
        poll: LivePollConfig::default(),
    });

    let bob_identity = NodeIdentity {
        nim_address: bob_nim_claim,
        btc_address: evm_funded_addr.to_vec(),
        btc_pubkey: bob_intent.btc_pubkey,
        rate_policy: RatePolicy::accept_all(),
        max_concurrent_swaps: 4,
        standing_intent: Some(bob_intent),
    };
    let bob_session = SwapSession::new(bob_identity, ladder)
        .with_funding_verifier(Box::new(NimHtlcVerifier::new(
            nim_rpc.clone() as Arc<dyn GatewayRpc>,
            nim_store.clone(),
        )))
        .with_counterparty_chain(Asset::Usdc);
    let bob_signer = LiveResponderSigner::new(LiveResponderConfig {
        evm_key: LocalEvmKey::from_secret(&amoy_secret).map_err(|_| "key")?,
        amoy: amoy.clone(),
        htlc,
        usdc,
        peer_book: bob_peer_book,
        nim_claim_key: bob_claim_key.clone(),
        nim_rpc: nim_rpc.clone() as Arc<dyn GatewayRpc>,
        nim_store: nim_store.clone(),
        gas,
        poll: LivePollConfig::default(),
    });

    let mut h = MeshHarness::new();
    let alice = h.add_session_participant(
        "alice",
        &[0xA1],
        alice_session,
        Box::new(OneShot::new(alice_signer)),
    );
    let bob = h.add_session_participant(
        "bob",
        &[0xB2],
        bob_session,
        Box::new(OneShot::new(bob_signer)),
    );
    h.connect("alice", "bob");
    println!("mesh up: alice ⇄ bob over the deterministic ether — driving discovery …\n");

    // ── the drive loop: tick both nodes, mirror phases, persist locks as they appear ─────────
    let mut last_a: Option<FfiSwapPhase> = None;
    let mut last_b: Option<FfiSwapPhase> = None;
    let mut state = serde_json::json!({});
    let mut settled = false;
    for tick in 0..RUN_BUDGET_TICKS {
        alice.poll_sync();
        bob.poll_sync();
        sleep(TICK);
        let a = alice.active_swaps().first().map(|m| m.phase);
        let b = bob.active_swaps().first().map(|m| m.phase);
        if a != last_a {
            if let Some(p) = a {
                println!("[t{tick:03}] alice → {p:?}");
            }
            last_a = a;
        }
        if b != last_b {
            if let Some(p) = b {
                println!("[t{tick:03}] bob   → {p:?}");
            }
            last_b = b;
        }

        // Persist each leg's lock the moment it is observable (refund data, never `S`).
        if state.get("nim").is_none() {
            if let Some(rec) = nim_store.records().first() {
                state["nim"] = serde_json::json!({
                    "contract": rec.contract.to_user_friendly(),
                    "value": rec.creation.value,
                    "timeout_ms": rec.creation.data.timeout,
                    "funding_tx": rec.tx_hash_hex,
                    "resolved": false,
                });
                save_state(&state);
                println!(
                    "[t{tick:03}] NIM HTLC observed: {} · {NIM_EXPLORER}/{}",
                    rec.contract.to_user_friendly(),
                    rec.tx_hash_hex
                );
            }
        }
        if state.get("usdc").is_none() {
            if let Some(escrow) = polygon_store.found_all().first() {
                state["usdc"] = serde_json::json!({
                    "swap_id": bytes_to_hex(&escrow.swap_id),
                    "timelock_s": escrow.timelock_s,
                    "resolved": false,
                });
                save_state(&state);
                println!(
                    "[t{tick:03}] USDC escrow verified: swapId 0x{}",
                    bytes_to_hex(&escrow.swap_id)
                );
            }
        }

        // Done on CHAIN TRUTH, not phase sampling (the phase mirror is reaped within a tick
        // of settling, so sampling races): the escrow reads Claimed AND the NIM HTLC is
        // emptied — both claims landed. Checked once both legs are known.
        if state.get("nim").is_some() && state.get("usdc").is_some() && tick % 2 == 0 {
            let escrow = polygon_store.found_all()[0];
            let mut cd = function_selector("getSwap(bytes32)").to_vec();
            cd.extend_from_slice(&escrow.swap_id);
            let claimed = AmoyChain::call(amoy.as_ref(), &htlc, &cd)
                .ok()
                .filter(|out| out.len() >= 6 * WORD)
                .map(|out| {
                    let mut be = [0u8; 8];
                    be.copy_from_slice(&out[5 * WORD + 24..6 * WORD]);
                    u64::from_be_bytes(be) == 2
                })
                .unwrap_or(false);
            let nim_contract = nim_store.records()[0].contract.to_user_friendly();
            if claimed && nim_balance(&nim_rpc, &nim_contract) == 0 {
                settled = true;
                break;
            }
        }
    }
    h.shutdown();
    if !settled {
        return Err(format!(
            "the swap did not settle within the budget; locks (if any) are persisted at {:?} \
             — re-run to refund after the timelocks",
            state_path()
        )
        .into());
    }
    println!("\nboth nodes SETTLED — re-checking ground truth on BOTH chains …");

    // ── ground truth on both chains ──────────────────────────────────────────────────────────
    let nim_rec = nim_store
        .records()
        .first()
        .cloned()
        .ok_or("no NIM funding record")?;
    let contract_nq = nim_rec.contract.to_user_friendly();
    let contract_balance = nim_balance(&nim_rpc, &contract_nq);
    if contract_balance != 0 {
        return Err(format!("NIM HTLC {contract_nq} still holds {contract_balance} luna").into());
    }
    let bob_nim_after = nim_balance(&nim_rpc, &bob_claim_nq);
    if bob_nim_after < bob_nim_before + GIVE_LUNA {
        return Err(
            format!("bob's claim address holds {bob_nim_after} luna — claim missing").into(),
        );
    }
    let escrow = polygon_store
        .found_all()
        .first()
        .copied()
        .ok_or("no verified escrow")?;
    let mut cd = function_selector("getSwap(bytes32)").to_vec();
    cd.extend_from_slice(&escrow.swap_id);
    let view = AmoyChain::call(amoy.as_ref(), &htlc, &cd)?;
    let escrow_state = {
        let mut be = [0u8; 8];
        be.copy_from_slice(&view[5 * WORD + 24..6 * WORD]);
        u64::from_be_bytes(be)
    };
    if escrow_state != 2 {
        return Err(format!("USDC escrow state is {escrow_state}, expected Claimed(2)").into());
    }
    let usdc_alice_after = usdc_balance(&amoy, &usdc, &alice_evm_claim)?;
    if usdc_alice_after < usdc_alice_before + TAKE_MICRO_USDC {
        return Err("alice's EVM claim address did not receive the USDC".into());
    }
    // Fetch bob's claim tx hash off the chain for the receipts.
    let bob_claim_tx = nim_rpc
        .get_transactions(&bob_claim_nq, 10)
        .ok()
        .and_then(|rows| rows.into_iter().find(|r| r.value == GIVE_LUNA))
        .map(|r| r.hash)
        .unwrap_or_else(|| "(query the explorer for the claim tx)".into());

    println!("   NIM  HTLC {contract_nq}: balance 0 (claimed)");
    println!("   NIM  funding: {NIM_EXPLORER}/{}", nim_rec.tx_hash_hex);
    println!("   NIM  claim:   {NIM_EXPLORER}/{bob_claim_tx}");
    println!(
        "   USDC escrow 0x{}: Claimed",
        bytes_to_hex(&escrow.swap_id)
    );
    println!("   alice claim address holds {usdc_alice_after} µUSDC");

    state["nim"]["resolved"] = serde_json::json!(true);
    state["usdc"]["resolved"] = serde_json::json!(true);
    save_state(&state);

    // ── settle up: sweep bob's FULL claim balance home to the treasury ───────────────────────
    let sweep_value = nim_balance(&nim_rpc, &bob_claim_nq);
    println!("\nsweeping bob's {sweep_value} luna home to the treasury …");
    let head = nim_rpc.block_number()?;
    let signed = AppSigner::new(bob_claim_key.clone()).sign_transfer(TransferIntent {
        recipient: treasury_nq.clone(),
        value: sweep_value,
        validity_start_height: head,
        network: NetworkId::Testnet,
    })?;
    let sweep_hash =
        nim_rpc.send_raw_transaction(&bytes_to_hex(&signed_transfer_wire(&signed)?))?;
    println!("   sweep: {NIM_EXPLORER}/{sweep_hash}");
    for _ in 0..30 {
        if nim_balance(&nim_rpc, &bob_claim_nq) == 0 {
            break;
        }
        sleep(Duration::from_secs(3));
    }

    // ── AFTER snapshot ────────────────────────────────────────────────────────────────────────
    let nim_after = nim_balance(&nim_rpc, &treasury_nq);
    let usdc_funded_after = usdc_balance(&amoy, &usdc, &evm_funded_addr)?;
    let pol_after = AmoyChain::balance(amoy.as_ref(), &evm_funded_addr)?;
    println!(
        "\nAFTER   treasury {nim_after} luna · bob-claim {} luna · funded {usdc_funded_after} µUSDC · alice-claim {usdc_alice_after} µUSDC · {pol_after} wei POL",
        nim_balance(&nim_rpc, &bob_claim_nq)
    );
    let _ = std::fs::remove_file(state_path());

    println!(
        "\n✅ ACT 2 COMPLETE — a real NIM⇄USDC atomic swap, two nodes, one secret, both chains."
    );
    println!("   (USDC intentionally rests on alice's derived claim address — recoverable, see docs/swap/ACT2-RECEIPTS.md)");
    Ok(())
}
