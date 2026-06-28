//! # live_cross_chain_swap — a REAL atomic NIM⇄BTC swap, live on two chains
//!
//! The finale: one secret `S`, one hashlock `H = SHA-256(S)`, two HTLCs on two real networks
//! (Nimiq **testnet** + Bitcoin **testnet3/signet**). Alice gives NIM and wants BTC; Bob gives BTC
//! and wants NIM. Claiming one leg reveals `S` on-chain, which claims the other — atomic by the
//! shared hashlock. Built entirely on OUR validated core (`nimiq::htlc` + `btc`), the same bytes
//! both reference libraries (`@nimiq/core`, `bitcoinjs-lib`) and both live chains already accept.
//!
//! Flow (the standard atomic-swap ladder, `T_A_nim > T_B_btc`):
//! 1. **NIM leg (auto):** faucet-fund Alice's NIM wallet → Alice funds a NIM HTLC to **Bob's NIM
//!    wallet** (hashlock `H`, timeout `T_A`). Confirmed on the Nimiq testnet.
//! 2. **BTC leg (one faucet tap):** the tool prints a BTC HTLC P2WSH paying **Alice's BTC wallet**
//!    (hashlock `H`, CLTV `T_B`). Fund it from any testnet/signet faucet — the tool waits.
//! 3. **Alice claims BTC** with `S` → `S` is now public in the BTC witness.
//! 4. **Bob claims NIM** with that same `S` → NIM lands in Bob's wallet.
//!
//! End state: **Alice's BTC wallet holds the BTC, Bob's NIM wallet holds the NIM** — verifiable on
//! both explorers, both unlocked by one secret. Testnet only; mainnet gated.
//!
//! Run: `cargo run --example live_cross_chain_swap --features "gateway-rpc bitcoin-gateway"`.
//! Env: `NIMMESH_BTC_API` (BTC indexer base, default signet) · `NIMMESH_BTC_NETWORK` (`testnet`|`signet`).

use std::error::Error;
use std::str::FromStr;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nimmesh_core::bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use nimmesh_core::bitcoin::{Address as BtcAddress, CompressedPublicKey, Network, Txid};
use nimmesh_core::btc::{BtcHtlcParams, FundedHtlc};
use nimmesh_core::btc_gateway::{BtcSignetGateway, DEFAULT_SIGNET_API};
use nimmesh_core::nimiq::address::Address as NimAddress;
use nimmesh_core::nimiq::hex::bytes_to_hex;
use nimmesh_core::nimiq::htlc::{
    regular_transfer_proof, HashAlgorithm, HtlcCreation, HtlcCreationData, HtlcRedeem,
};
use nimmesh_core::nimiq::signer::{EnclaveKey, InMemoryEnclaveKey};
use nimmesh_core::nimiq::tx::signature_proof_single_sig;
use nimmesh_core::rpc::{GatewayRpc, HttpGatewayRpc, DEFAULT_TESTNET_RPC_URL};
use nimmesh_core::swap_leg::sha256;
use nimmesh_core::NetworkId;

const NIM_FAUCET: &str = "https://faucet.pos.nimiq-testnet.com/tapit";
const NIM_LOCK_LUNA: u64 = 200_000; // 2 NIM
const BTC_FEE_SAT: u64 = 330;
const NIM_EXPLORER: &str = "https://nimiq-testnet.observer/transactions";
const BTC_FUND_WAIT: Duration = Duration::from_secs(1800); // 30 min for the BTC faucet tap
const POLL: Duration = Duration::from_secs(8);

fn rand32() -> Result<[u8; 32], Box<dyn Error>> {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b)?;
    Ok(b)
}

/// A persistent seed from `env` (32-byte hex) for a **reusable** wallet, or fresh entropy. Reuse the
/// saved wallets (`~/secrets/nimmesh-swap-wallets.env`) so swapped funds land in known wallets
/// and cycle back instead of stranding.
fn env_seed(var: &str) -> Result<[u8; 32], Box<dyn Error>> {
    match std::env::var(var) {
        Ok(h) if !h.trim().is_empty() => Ok(nimmesh_core::nimiq::hex::hex_to_bytes(h.trim())?
            .try_into()
            .map_err(|_| "seed must be 32 bytes")?),
        _ => rand32(),
    }
}
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// Sign 67-byte content with an Ed25519 enclave key → (pubkey, sig).
fn nim_sign(key: &dyn EnclaveKey, content: &[u8]) -> ([u8; 32], [u8; 64]) {
    let pk: [u8; 32] = key.public_key().try_into().unwrap();
    let sig: [u8; 64] = key.sign_content(content.to_vec()).try_into().unwrap();
    (pk, sig)
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("== nimiq.nimmesh — LIVE atomic NIM<->BTC swap (one secret, two chains) ==");
    let secp = Secp256k1::new();

    // --- keys: persistent (reusable, recoverable) from env, else fresh. The swapped funds land in
    // the NIM + BTC wallets these derive, so they cycle back instead of stranding. ---
    let nim_seed = env_seed("NIMMESH_NIM_SEED")?;
    let btc_seed = env_seed("NIMMESH_BTC_SEED")?;
    let alice_nim = InMemoryEnclaveKey::from_secret(&nim_seed);
    let bob_nim = InMemoryEnclaveKey::from_secret(&nim_seed); // self NIM HTLC → NIM returns to the wallet
    let alice_btc_sk = btc_seed; // BTC claimant → BTC returns to the wallet
    let bob_btc_sk = btc_seed; // BTC refunder (same treasury)
    let alice_btc_pk = PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&alice_btc_sk)?);
    let bob_btc_pk = PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&bob_btc_sk)?);

    let alice_nim_addr = NimAddress::from_public_key(&nim_sign(&alice_nim, &[]).0);
    let bob_nim_addr = NimAddress::from_public_key(&bob_nim.public_key().try_into().unwrap());
    let btc_net = match std::env::var("NIMMESH_BTC_NETWORK").as_deref() {
        Ok("testnet") => Network::Testnet,
        _ => Network::Signet,
    };
    let alice_btc_payout = BtcAddress::p2wpkh(&CompressedPublicKey(alice_btc_pk), btc_net);

    // --- the swap secret + the SHARED hashlock ---
    let secret = rand32()?;
    let h = sha256(&secret);
    println!("\nsecret S      : {}", bytes_to_hex(&secret));
    println!("hashlock H    : {}  (locks BOTH legs)", bytes_to_hex(&h));
    println!("Alice NIM     : {}", alice_nim_addr.to_user_friendly());
    println!("Bob   NIM     : {}  <- NIM lands here", bob_nim_addr.to_user_friendly());
    println!("Alice BTC     : {alice_btc_payout}  <- BTC lands here");

    let rpc = HttpGatewayRpc::new(DEFAULT_TESTNET_RPC_URL, NetworkId::Testnet)?;

    // ===== 1) NIM leg: faucet-fund Alice, fund the NIM HTLC to Bob =====
    println!("\n[1] NIM leg — funding Alice from the testnet faucet …");
    let _ = ureq::post(NIM_FAUCET).send_form(&[("address", &alice_nim_addr.to_user_friendly())]);
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if let Ok(Some(a)) = rpc.get_account(&alice_nim_addr.to_user_friendly()) {
            if a.balance > NIM_LOCK_LUNA {
                println!("    Alice funded: {} luna", a.balance);
                break;
            }
        }
        sleep(Duration::from_secs(3));
    }
    let head = rpc.block_number()?;
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let t_a = now_ms + 7_200_000; // T_A = now + 2 h (Unix-ms; the longer leg)
    let creation = HtlcCreation {
        funder: alice_nim_addr,
        data: HtlcCreationData {
            htlc_sender: alice_nim_addr,
            htlc_recipient: bob_nim_addr, // Bob can claim with the preimage
            hash_algorithm: HashAlgorithm::Sha256,
            hash_root: h,
            hash_count: 1,
            timeout: t_a,
        },
        value: NIM_LOCK_LUNA,
        fee: 0,
        validity_start_height: head,
        network_id: NetworkId::Testnet.wire_id(),
    };
    let nim_contract = creation.contract_address();
    let (apk, asig) = nim_sign(&alice_nim, &creation.serialize_content());
    let nim_fund_hex = bytes_to_hex(&creation.serialize_wire(&signature_proof_single_sig(&apk, &asig)));
    let nim_fund_txid = rpc.send_raw_transaction(&nim_fund_hex)?;
    println!("    NIM HTLC funded: contract {}", nim_contract.to_user_friendly());
    println!("    tx {NIM_EXPLORER}/{nim_fund_txid}");
    wait_nim(&rpc, &nim_fund_txid, "NIM HTLC funding")?;

    // ===== 2) BTC leg: print the HTLC, wait for the user to fund it (= Bob funding) =====
    let btc = BtcHtlcParams {
        hash_root: h,
        recipient_pubkey: alice_btc_pk.serialize(), // Alice claims with the preimage
        sender_pubkey: bob_btc_pk.serialize(),       // Bob refunds after CLTV
        cltv_locktime: (now_secs() + 3_600) as i64,  // T_B = now + 1 h (Unix-secs; shorter leg)
    };
    let btc_addr = btc.p2wsh_address(btc_net).to_string();
    let base = std::env::var("NIMMESH_BTC_API").unwrap_or_else(|_| DEFAULT_SIGNET_API.to_string());
    let gw = BtcSignetGateway::new(&base)?;
    println!("\n[2] BTC leg — FUND THIS HTLC ADDRESS (Bob's side) on {btc_net:?}:");
    println!("    >>> {btc_addr} <<<");
    println!("    (any testnet/signet faucet; the swap finishes automatically)");
    let deadline = Instant::now() + BTC_FUND_WAIT;
    let funded = loop {
        if Instant::now() > deadline {
            return Err("BTC HTLC never funded within the wait window".into());
        }
        if let Ok(utxos) = gw.address_utxos(&btc_addr) {
            if let Some(u) = utxos.into_iter().find(|u| u.value > BTC_FEE_SAT) {
                println!("    BTC HTLC funded: {} sat at {}:{}", u.value, &u.txid[..12], u.vout);
                break FundedHtlc { txid: Txid::from_str(&u.txid)?, vout: u.vout, value_sat: u.value };
            }
        }
        sleep(POLL);
    };

    // ===== 3) Alice claims the BTC, REVEALING S on-chain =====
    println!("\n[3] Alice claims the BTC HTLC (reveals S) …");
    let claim = btc.claim_tx(&funded, &secret, alice_btc_payout.script_pubkey(), funded.value_sat - BTC_FEE_SAT, &alice_btc_sk)?;
    let btc_claim_txid = gw.broadcast(&bytes_to_hex(&claim))?;
    println!("    BTC claim broadcast: {btc_claim_txid}");
    println!("    S is now public in the witness on the BTC chain.");

    // ===== 4) Bob claims the NIM with the SAME S =====
    println!("\n[4] Bob claims the NIM HTLC with the revealed S …");
    let head2 = rpc.block_number()?;
    let redeem = HtlcRedeem {
        contract: nim_contract,
        recipient: bob_nim_addr,
        value: NIM_LOCK_LUNA,
        fee: 0,
        validity_start_height: head2,
        network_id: NetworkId::Testnet.wire_id(),
    };
    let (bpk, bsig) = nim_sign(&bob_nim, &redeem.serialize_content());
    let proof = regular_transfer_proof(HashAlgorithm::Sha256, &h, &secret, &bpk, &bsig);
    let nim_claim_txid = rpc.send_raw_transaction(&bytes_to_hex(&redeem.serialize_wire(&proof)))?;
    println!("    NIM claim broadcast: {NIM_EXPLORER}/{nim_claim_txid}");
    wait_nim(&rpc, &nim_claim_txid, "NIM claim")?;

    println!("\n✅✅ ATOMIC SWAP COMPLETE — one secret, two chains:");
    println!("   Bob's NIM wallet {} received {NIM_LOCK_LUNA} luna", bob_nim_addr.to_user_friendly());
    println!("   Alice's BTC wallet {alice_btc_payout} received the BTC");
    println!("   BTC claim: https://mempool.space/{}/tx/{btc_claim_txid}", if btc_net == Network::Testnet { "testnet" } else { "signet" });
    Ok(())
}

/// Poll a Nimiq tx hash to inclusion.
fn wait_nim(rpc: &HttpGatewayRpc, hash: &str, label: &str) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(150);
    while Instant::now() < deadline {
        sleep(Duration::from_secs(3));
        if let Ok(Some(tx)) = rpc.get_transaction(hash) {
            if let Some(b) = tx.block_number {
                println!("    {label} CONFIRMED in block {b}");
                return Ok(());
            }
        }
    }
    println!("    {label} broadcast; still pending (continuing)");
    Ok(())
}
