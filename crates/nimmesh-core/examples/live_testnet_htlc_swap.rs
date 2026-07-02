//! # live_testnet_htlc_swap — prove the NIM HTLC leg end to end on the real TESTNET
//!
//! The one thing `@nimiq/core` JS could not validate: an HTLC **redeem (claim-with-preimage)**
//! proof. This tool settles it against the live Nimiq Albatross **testnet** using OUR core for
//! every byte:
//!
//! 1. generate a testnet keypair (seed stays in-process, never over FFI) + faucet-fund it;
//! 2. build + sign an **HTLC creation (funding)** tx that locks NIM to `H = SHA-256(secret)`
//!    with a future Unix-MS-timestamp timeout, recipient = self — broadcast it, await inclusion;
//! 3. build + sign an **HTLC redeem** tx that spends the contract by revealing the preimage
//!    (the `RegularTransfer` proof: `nimiq::htlc::regular_transfer_proof`) — broadcast it,
//!    await inclusion.
//!
//! If step 3 confirms on-chain, the resolve proof is byte-exact-correct — the network is the
//! authority `@nimiq/core` JS could not be (`docs/swap/F0-HTLC-FINDINGS.md`). Self-swap (funder =
//! claimant = us) so it needs only one funded key, but it exercises the exact funding + redeem
//! bytes a real cross-chain swap floods over the mesh.
//!
//! **TESTNET ONLY** (play money). The RPC constructor refuses any mainnet host. Built only with
//! `--features gateway-rpc`, so `cargo test` never runs it.
//!
//! Run: `cargo run --example live_testnet_htlc_swap --features gateway-rpc`
//! Env: `NIMIQ_NIMMESH_TESTNET_SEED` (32-byte hex, reuse a funded key) ·
//! `NIMIQ_NIMMESH_TESTNET_RPC_URL` · `NIMIQ_NIMMESH_USE_FAUCET=1`

use std::error::Error;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nimmesh_core::nimiq::address::Address;
use nimmesh_core::nimiq::hex::{bytes_to_hex, hex_to_bytes};
use nimmesh_core::nimiq::htlc::{
    regular_transfer_proof, HashAlgorithm, HtlcCreation, HtlcCreationData, HtlcRedeem,
};
use nimmesh_core::nimiq::signer::{EnclaveKey, InMemoryEnclaveKey};
use nimmesh_core::nimiq::tx::signature_proof_single_sig;
use nimmesh_core::rpc::{GatewayRpc, HttpGatewayRpc, DEFAULT_TESTNET_RPC_URL};
use nimmesh_core::swap_leg::sha256;
use nimmesh_core::NetworkId;

const DEFAULT_FAUCET_URL: &str = "https://faucet.pos.nimiq-testnet.com/tapit";
const EXPLORER_TX_BASE: &str = "https://nimiq-testnet.observer/transactions";
const INCLUSION_TIMEOUT: Duration = Duration::from_secs(150);
const FAUCET_TIMEOUT: Duration = Duration::from_secs(60);
/// Lock 2 NIM into the HTLC; redeem the full amount (fee 0).
const LOCK_LUNA: u64 = 200_000;
/// HTLC timeout is a **Unix timestamp in milliseconds** (compared against the block's
/// `block_state.time`, NOT the block height — `core-rs-albatross` htlc_contract.rs). Set it 1 hour
/// ahead so the claim path is open (a height-valued timeout reads as already-expired at birth).
const TIMEOUT_MS_AHEAD: u64 = 3_600_000;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

fn resolve_seed() -> Result<[u8; 32], Box<dyn Error>> {
    if let Some(hex) = env("NIMIQ_NIMMESH_TESTNET_SEED") {
        let seed: [u8; 32] = hex_to_bytes(hex.trim())?
            .try_into()
            .map_err(|_| "seed must be 32 bytes (64 hex chars)")?;
        println!("  seed: loaded from NIMIQ_NIMMESH_TESTNET_SEED");
        Ok(seed)
    } else {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed)?;
        println!(
            "  seed (hex, save to reuse a funded key): {}",
            bytes_to_hex(&seed)
        );
        Ok(seed)
    }
}

fn tap_faucet(rpc: &HttpGatewayRpc, address: &str) -> Result<(), Box<dyn Error>> {
    let faucet = env("NIMIQ_NIMMESH_FAUCET_URL").unwrap_or_else(|| DEFAULT_FAUCET_URL.to_string());
    println!("\n[faucet] tapping {faucet} for {address}");
    match ureq::post(&faucet).send_form(&[("address", address)]) {
        Ok(resp) => println!("[faucet] HTTP {}", resp.status()),
        Err(e) => println!("[faucet] WARN tap failed (fund manually if needed): {e}"),
    }
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
    println!("[faucet] WARN balance still 0; broadcast may be rejected");
    Ok(())
}

/// 32-byte public key + 64-byte signature over `content`, from the enclave key.
fn sign(key: &dyn EnclaveKey, content: &[u8]) -> Result<([u8; 32], [u8; 64]), Box<dyn Error>> {
    let pubkey: [u8; 32] = key.public_key().try_into().map_err(|_| "bad pubkey len")?;
    let sig: [u8; 64] = key
        .sign_content(content.to_vec())
        .try_into()
        .map_err(|_| "bad signature len")?;
    Ok((pubkey, sig))
}

/// Broadcast `raw_hex` and poll until it is included, printing the explorer link.
fn broadcast_and_wait(
    rpc: &HttpGatewayRpc,
    label: &str,
    raw_hex: &str,
) -> Result<(), Box<dyn Error>> {
    println!("\n[{label}] broadcasting ({} bytes) …", raw_hex.len() / 2);
    let hash = rpc.send_raw_transaction(raw_hex)?;
    println!("[{label}] accepted into mempool; tx {hash}");
    println!("[{label}] explorer: {EXPLORER_TX_BASE}/{hash}");
    let deadline = Instant::now() + INCLUSION_TIMEOUT;
    while Instant::now() < deadline {
        sleep(Duration::from_secs(3));
        match rpc.get_transaction(&hash) {
            Ok(Some(tx)) if tx.block_number.is_some() => {
                println!("[{label}] CONFIRMED in block {}", tx.block_number.unwrap());
                return Ok(());
            }
            Ok(_) => println!("[{label}] … pending"),
            Err(e) => println!("[{label}] … poll error (transient): {e}"),
        }
    }
    Err(format!("[{label}] not included within {INCLUSION_TIMEOUT:?} (hash {hash})").into())
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("== nimiq.nimmesh — live TESTNET HTLC swap (fund -> claim-with-preimage) ==");

    // --- key + RPC ---
    println!("\n[1] testnet keypair (seed stays in-process)");
    let seed = resolve_seed()?;
    let key = InMemoryEnclaveKey::from_secret(&seed);
    let pubkey: [u8; 32] = key.public_key().try_into().map_err(|_| "bad pubkey")?;
    let me = Address::from_public_key(&pubkey);
    let me_user = me.to_user_friendly();
    println!("  address: {me_user}");

    let rpc_url =
        env("NIMIQ_NIMMESH_TESTNET_RPC_URL").unwrap_or_else(|| DEFAULT_TESTNET_RPC_URL.to_string());
    let rpc = HttpGatewayRpc::new(&rpc_url, NetworkId::Testnet)?;
    println!("  rpc: {rpc_url}");

    if env("NIMIQ_NIMMESH_USE_FAUCET").as_deref() == Some("1") {
        tap_faucet(&rpc, &me_user)?;
    } else if let Ok(Some(acct)) = rpc.get_account(&me_user) {
        println!(
            "  balance: {} luna (set NIMIQ_NIMMESH_USE_FAUCET=1 to tap the faucet)",
            acct.balance
        );
    }

    // --- the swap secret + hashlock ---
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret)?;
    let hash_root = sha256(&secret); // H = SHA-256(secret); hash_count = 1
    println!(
        "\n[2] hashlock H = SHA-256(secret): {}",
        bytes_to_hex(&hash_root)
    );

    // --- 1) HTLC creation (funding) ---
    let head = rpc.block_number()?;
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let timeout = now_ms + TIMEOUT_MS_AHEAD; // Unix-MS timestamp in the future (claim path open)
    let creation = HtlcCreation {
        funder: me,
        data: HtlcCreationData {
            htlc_sender: me,    // refund -> us after timeout
            htlc_recipient: me, // claim  -> us with the preimage (self-swap)
            hash_algorithm: HashAlgorithm::Sha256,
            hash_root,
            hash_count: 1,
            timeout,
        },
        value: LOCK_LUNA,
        fee: 0,
        validity_start_height: head,
        network_id: NetworkId::Testnet.wire_id(),
    };
    let contract = creation.contract_address();
    println!("\n[3] HTLC funding: lock {LOCK_LUNA} luna, timeout @block {timeout}");
    println!("  contract address: {}", contract.to_user_friendly());
    println!("  secret (hex): {}", bytes_to_hex(&secret));
    let (pk, sig) = sign(&key, &creation.serialize_content())?;
    let funding_wire = creation.serialize_wire(&signature_proof_single_sig(&pk, &sig));
    println!("  funding raw_hex: {}", bytes_to_hex(&funding_wire));
    broadcast_and_wait(&rpc, "fund", &bytes_to_hex(&funding_wire))?;
    if let Ok(Some(acct)) = rpc.get_account(&contract.to_user_friendly()) {
        println!(
            "  contract after funding: {} luna, type {}",
            acct.balance, acct.account_type
        );
    }

    // --- 2) HTLC redeem (claim with the preimage) — THE proof under test ---
    let head2 = rpc.block_number()?;
    let redeem = HtlcRedeem {
        contract,
        recipient: me,
        value: LOCK_LUNA, // withdraw the full locked amount (fee 0)
        fee: 0,
        validity_start_height: head2,
        network_id: NetworkId::Testnet.wire_id(),
    };
    let (rpk, rsig) = sign(&key, &redeem.serialize_content())?;
    let proof = regular_transfer_proof(HashAlgorithm::Sha256, &hash_root, &secret, &rpk, &rsig);
    let redeem_wire = redeem.serialize_wire(&proof);
    println!("\n[4] HTLC redeem: claim {LOCK_LUNA} luna by revealing the preimage");
    println!("  redeem raw_hex: {}", bytes_to_hex(&redeem_wire));
    let outcome = broadcast_and_wait(&rpc, "redeem", &bytes_to_hex(&redeem_wire));
    // Whatever the poll said, the ground truth is the contract balance: 0 = claimed.
    if let Ok(Some(acct)) = rpc.get_account(&contract.to_user_friendly()) {
        println!("\n  contract balance after redeem: {} luna", acct.balance);
        if acct.balance == 0 {
            println!("✅ SUCCESS — the HTLC was funded AND claimed-with-preimage on testnet.");
            println!(
                "   The redeem proof (RegularTransfer) is byte-exact: the network accepted it."
            );
            return Ok(());
        }
    }
    outcome?;
    Ok(())
}
