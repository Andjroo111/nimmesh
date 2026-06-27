//! # live_testnet_broadcast — the G8 live TESTNET broadcast tool (money-path)
//!
//! Proves the whole offline-origination → online-broadcast path end to end against the
//! **real Nimiq Albatross TESTNET**, using OUR core for every money-critical step:
//!
//! 1. generate (or load) a **testnet** Ed25519 keypair via the core's `InMemoryEnclaveKey`
//!    (the seed stays in-process — it is NEVER sent over FFI);
//! 2. print the user-friendly `NQ…` address (derived by the core's address codec);
//! 3. optionally tap the **testnet faucet** to fund that address;
//! 4. fetch the head height (`block_number`) over the testnet JSON-RPC;
//! 5. build + **sign** a basic transfer with the core's G3 signer (`AppSigner`);
//! 6. **broadcast** it (`sendRawTransaction(rawHex)`) and poll `getTransactionByHash`
//!    until it is included, printing the tx hash + block + explorer URL.
//!
//! ## Money-path safety
//!
//! - **TESTNET ONLY.** The intent + the RPC client are both `NetworkId::Testnet`; the RPC
//!   constructor refuses any mainnet host (`rpc::guard_testnet`). There is no mainnet path.
//! - The seed is read from `NIMIQ_NIMMESH_TESTNET_SEED` (hex) or freshly randomised; it
//!   lives only in this process and is never broadcast or sent over FFI.
//! - The RPC URL is injectable via `NIMIQ_NIMMESH_TESTNET_RPC_URL` (defaults to the public
//!   testnet endpoint).
//! - Built ONLY with `--features gateway-rpc`, so a plain `cargo test` never compiles or
//!   runs it — no broadcast can happen inside the test suite.
//!
//! Run:
//! ```text
//! cargo run --example live_testnet_broadcast --features gateway-rpc
//! ```
//! Env knobs (all optional):
//! - `NIMIQ_NIMMESH_TESTNET_RPC_URL`  testnet JSON-RPC base (default public endpoint)
//! - `NIMIQ_NIMMESH_TESTNET_SEED`     32-byte hex seed (reuse a funded key across runs)
//! - `NIMIQ_NIMMESH_RECIPIENT`        `NQ…` recipient (default: send to self)
//! - `NIMIQ_NIMMESH_VALUE_LUNA`       amount in luna (default 100000 = 1 NIM)
//! - `NIMIQ_NIMMESH_FAUCET_URL`       testnet faucet endpoint (default tapit)
//! - `NIMIQ_NIMMESH_USE_FAUCET=1`     opt in to tapping the faucet for this address

use std::error::Error;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use nimmesh_core::nimiq::hex::{bytes_to_hex, hex_to_bytes};
use nimmesh_core::nimiq::signer::{AppSigner, EnclaveKey, InMemoryEnclaveKey, TransferIntent};
use nimmesh_core::rpc::{GatewayRpc, HttpGatewayRpc, DEFAULT_TESTNET_RPC_URL};
use nimmesh_core::NetworkId;

/// Default public testnet faucet (POST `address=NQ…`, ~10_000 NIM, no rate limit).
const DEFAULT_FAUCET_URL: &str = "https://faucet.pos.nimiq-testnet.com/tapit";
/// Testnet explorer base for the final receipt link.
const EXPLORER_TX_BASE: &str = "https://nimiq-testnet.observer/transactions";
/// How long to poll for inclusion before giving up (the tx is still in the mempool).
const INCLUSION_TIMEOUT: Duration = Duration::from_secs(150);
/// How long to wait for the faucet to credit the address.
const FAUCET_TIMEOUT: Duration = Duration::from_secs(60);

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Resolve the 32-byte seed: an explicit hex env var (reuse a funded key) or fresh entropy.
fn resolve_seed() -> Result<[u8; 32], Box<dyn Error>> {
    if let Some(hex) = env("NIMIQ_NIMMESH_TESTNET_SEED") {
        let bytes = hex_to_bytes(hex.trim())?;
        let seed: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "NIMIQ_NIMMESH_TESTNET_SEED must be exactly 32 bytes (64 hex chars)")?;
        println!("  seed: loaded from NIMIQ_NIMMESH_TESTNET_SEED");
        Ok(seed)
    } else {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed)?;
        println!("  seed: freshly randomised (set NIMIQ_NIMMESH_TESTNET_SEED to reuse it)");
        println!(
            "  seed (hex, save to reuse a funded key): {}",
            bytes_to_hex(&seed)
        );
        Ok(seed)
    }
}

/// Best-effort faucet tap: POST `address=NQ…`, then poll the account balance until funded.
fn tap_faucet(rpc: &HttpGatewayRpc, address: &str) -> Result<(), Box<dyn Error>> {
    let faucet = env("NIMIQ_NIMMESH_FAUCET_URL").unwrap_or_else(|| DEFAULT_FAUCET_URL.to_string());
    println!("\n[faucet] tapping {faucet} for {address}");
    match ureq::post(&faucet).send_form(&[("address", address)]) {
        Ok(resp) => println!("[faucet] response: HTTP {}", resp.status()),
        Err(e) => println!("[faucet] WARN tap failed (fund manually if needed): {e}"),
    }
    println!("[faucet] waiting for the balance to credit …");
    let deadline = Instant::now() + FAUCET_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(Some(acct)) = rpc.get_account(address) {
            if acct.balance > 0 {
                println!("[faucet] funded: balance = {} luna", acct.balance);
                return Ok(());
            }
        }
        sleep(Duration::from_secs(3));
    }
    println!(
        "[faucet] WARN balance still 0 after {FAUCET_TIMEOUT:?}; the broadcast may be rejected"
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("== nimiq.nimmesh G8 — live TESTNET broadcast (money-path, testnet only) ==");

    // --- 1. keypair + address (via OUR core) ------------------------------------------
    println!("\n[1/6] generating testnet keypair (seed stays in-process, never over FFI)");
    let seed = resolve_seed()?;
    let key: Arc<dyn EnclaveKey> = Arc::new(InMemoryEnclaveKey::from_secret(&seed));
    let signer = AppSigner::new(key);
    let address = signer.address()?;
    println!("  address: {address}");

    // --- 2. RPC client (TESTNET-guarded) ----------------------------------------------
    let rpc_url =
        env("NIMIQ_NIMMESH_TESTNET_RPC_URL").unwrap_or_else(|| DEFAULT_TESTNET_RPC_URL.to_string());
    println!("\n[2/6] connecting to testnet RPC: {rpc_url}");
    // Constructor refuses any non-testnet network or known mainnet host.
    let rpc = HttpGatewayRpc::new(&rpc_url, NetworkId::Testnet)?;

    // --- 3. optional faucet -----------------------------------------------------------
    if env("NIMIQ_NIMMESH_USE_FAUCET").as_deref() == Some("1") {
        tap_faucet(&rpc, &address)?;
    } else {
        println!("\n[3/6] faucet: skipped (set NIMIQ_NIMMESH_USE_FAUCET=1 to tap it)");
        if let Ok(Some(acct)) = rpc.get_account(&address) {
            println!("  current balance: {} luna", acct.balance);
        }
    }

    // --- 4. head height ---------------------------------------------------------------
    println!("\n[4/6] fetching head height");
    let head = rpc.block_number()?;
    println!("  head: {head}");

    // --- 5. build + sign (via OUR core, G3 signer) ------------------------------------
    let recipient = env("NIMIQ_NIMMESH_RECIPIENT").unwrap_or_else(|| address.clone());
    let value: u64 = env("NIMIQ_NIMMESH_VALUE_LUNA")
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(100_000); // 1 NIM
    println!("\n[5/6] signing a basic transfer (testnet)");
    println!("  recipient: {recipient}");
    println!("  value: {value} luna");
    let intent = TransferIntent {
        recipient,
        value,
        validity_start_height: head,
        network: NetworkId::Testnet,
    };
    let signed = signer.sign_transfer(intent)?;
    println!("  tx_hash: {}", signed.tx_hash);
    println!("  raw_hex: {} bytes", signed.raw_hex.len() / 2);
    println!(
        "  validity window: [{}, {})",
        signed.validity_start_height, signed.valid_until_height
    );

    // --- 6. broadcast + poll inclusion ------------------------------------------------
    println!("\n[6/6] broadcasting via sendRawTransaction …");
    let hash = rpc.send_raw_transaction(&signed.raw_hex)?;
    println!("  accepted into mempool; tx hash: {hash}");
    println!("  explorer: {EXPLORER_TX_BASE}/{hash}");

    println!("  polling getTransactionByHash for inclusion …");
    let deadline = Instant::now() + INCLUSION_TIMEOUT;
    while Instant::now() < deadline {
        sleep(Duration::from_secs(3));
        match rpc.get_transaction(&hash) {
            Ok(Some(tx)) => {
                if let Some(block) = tx.block_number {
                    println!("\n  CONFIRMED — included in block {block}");
                    println!("  tx hash: {hash}");
                    println!("  explorer: {EXPLORER_TX_BASE}/{hash}");
                    return Ok(());
                }
                println!("  … still pending in the mempool");
            }
            Ok(None) => println!("  … node does not know the tx yet"),
            Err(e) => println!("  … poll error (transient): {e}"),
        }
    }

    println!("\n  still pending after {INCLUSION_TIMEOUT:?} — it is broadcast and in the mempool.");
    println!("  check the explorer: {EXPLORER_TX_BASE}/{hash}");
    Ok(())
}
