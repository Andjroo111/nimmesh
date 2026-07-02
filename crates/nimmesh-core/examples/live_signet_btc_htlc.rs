//! # live_signet_btc_htlc — prove the BTC HTLC leg on **Bitcoin signet** (fund → claim-w/-preimage)
//!
//! The BTC analog of `live_testnet_htlc_swap` (which proved the NIM leg on-chain). Uses OUR
//! `crate::btc` to build the HTLC P2WSH + the BIP143-signed claim, and `crate::btc_gateway` to
//! broadcast + confirm via the public signet indexer (mempool.space) — no node.
//!
//! 1. derive a secp256k1 key (seed in-process) + its p2wpkh payout address;
//! 2. build a self-HTLC (recipient = sender = us) locked to `H = SHA-256(secret)` with a **future
//!    Unix-seconds CLTV** so the claim path is open; print the P2WSH **funding address**;
//! 3. **wait for funds** — tap a signet faucet (e.g. <https://signetfaucet.com>) for the printed
//!    address; the tool polls the indexer until a UTXO appears (the one manual step — signet
//!    faucets are captcha-gated; everything else is automatic);
//! 4. build + **broadcast the claim** (reveals the preimage), poll until it confirms on signet.
//!
//! **Signet only** (play money); the gateway refuses a mainnet base. Built only with
//! `--features bitcoin-gateway`. Run: `cargo run --example live_signet_btc_htlc --features bitcoin-gateway`.
//! Env: `NIMMESH_BTC_SEED` (32-byte hex, reuse a funded key) · `NIMMESH_BTC_API` (indexer base).

use std::error::Error;
use std::str::FromStr;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nimmesh_core::bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use nimmesh_core::bitcoin::{Address, CompressedPublicKey, Network, Txid};
use nimmesh_core::btc::{BtcHtlcParams, FundedHtlc};
use nimmesh_core::btc_gateway::{BtcSignetGateway, DEFAULT_SIGNET_API};
use nimmesh_core::nimiq::hex::{bytes_to_hex, hex_to_bytes};
use nimmesh_core::swap_leg::sha256;

const CLTV_SECS_AHEAD: i64 = 3_600; // 1 h — claim path stays open
const FEE_SAT: u64 = 330; // a small signet fee
const FUND_TIMEOUT: Duration = Duration::from_secs(900); // 15 min to tap the faucet
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(600);
const EXPLORER: &str = "https://mempool.space/signet/tx";

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.trim().is_empty())
}

fn resolve_seed() -> Result<[u8; 32], Box<dyn Error>> {
    if let Some(hex) = env("NIMMESH_BTC_SEED") {
        Ok(hex_to_bytes(hex.trim())?
            .try_into()
            .map_err(|_| "seed must be 32 bytes")?)
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

fn main() -> Result<(), Box<dyn Error>> {
    println!("== nimiq.nimmesh — live SIGNET BTC HTLC (fund -> claim-with-preimage) ==");

    // 1) key + payout address
    let secp = Secp256k1::new();
    let seed = resolve_seed()?;
    let sk = SecretKey::from_slice(&seed)?;
    let pk = PublicKey::from_secret_key(&secp, &sk);
    let pk_compressed: [u8; 33] = pk.serialize();
    let payout = Address::p2wpkh(&CompressedPublicKey(pk), Network::Signet);
    println!("  our key / payout (p2wpkh): {payout}");

    // 2) the HTLC (self-swap: recipient = sender = us) + the future CLTV
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret)?;
    let hash_root = sha256(&secret);
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let params = BtcHtlcParams {
        hash_root,
        recipient_pubkey: pk_compressed,
        sender_pubkey: pk_compressed,
        cltv_locktime: now + CLTV_SECS_AHEAD,
    };
    let htlc_addr = params.p2wsh_address(Network::Signet);
    println!("  secret (hex): {}", bytes_to_hex(&secret));
    println!("  hashlock H  : {}", bytes_to_hex(&hash_root));

    // 3) gateway + wait for funds
    let base = env("NIMMESH_BTC_API").unwrap_or_else(|| DEFAULT_SIGNET_API.to_string());
    let gw = BtcSignetGateway::new(&base)?;
    println!("\n  FUND THIS HTLC ADDRESS with signet BTC (faucet: https://signetfaucet.com):");
    println!("    >>> {htlc_addr} <<<");
    println!("  (the one manual step — then this completes automatically)");
    println!("  waiting up to {FUND_TIMEOUT:?} for a UTXO …");

    let htlc_addr_s = htlc_addr.to_string();
    let deadline = Instant::now() + FUND_TIMEOUT;
    let funded = loop {
        if Instant::now() > deadline {
            return Err(
                "no funding UTXO arrived — tap the faucet and re-run with NIMMESH_BTC_SEED".into(),
            );
        }
        match gw.address_utxos(&htlc_addr_s) {
            Ok(utxos) => {
                if let Some(u) = utxos.into_iter().find(|u| u.value > FEE_SAT) {
                    println!(
                        "  funded: {} sat at {}:{} (confirmed={})",
                        u.value,
                        &u.txid[..12],
                        u.vout,
                        u.confirmed
                    );
                    break FundedHtlc {
                        txid: Txid::from_str(&u.txid)?,
                        vout: u.vout,
                        value_sat: u.value,
                    };
                }
            }
            Err(e) => println!("  … utxo poll (transient): {e}"),
        }
        sleep(Duration::from_secs(10));
    };

    // 4) build + broadcast the claim (reveals the preimage)
    let claim = params.claim_tx(
        &funded,
        &secret,
        payout.script_pubkey(),
        funded.value_sat - FEE_SAT,
        &seed,
    )?;
    println!("\n  claim tx ({} bytes) — broadcasting …", claim.len());
    let txid = gw.broadcast(&bytes_to_hex(&claim))?;
    println!("  claim broadcast; txid {txid}");
    println!("  explorer: {EXPLORER}/{txid}");

    let cdeadline = Instant::now() + CONFIRM_TIMEOUT;
    while Instant::now() < cdeadline {
        sleep(Duration::from_secs(10));
        match gw.tx_block_height(&txid) {
            Ok(Some(h)) => {
                println!("\n✅ SUCCESS — BTC HTLC funded AND claimed-with-preimage on signet (block {h}).");
                println!("   The Rust BitcoinLeg's HTLC + BIP143 claim is on-chain-valid.");
                return Ok(());
            }
            Ok(None) => println!("  … claim pending"),
            Err(e) => println!("  … confirm poll (transient): {e}"),
        }
    }
    println!("  claim broadcast but not yet confirmed — check {EXPLORER}/{txid}");
    Ok(())
}
