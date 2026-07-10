//! Offline tests for [`crate::live_swap_signer`] — every signing path byte-asserted against
//! the proven builders (`nimiq::htlc`, `evm_rlp`+`evm_signer`) over deterministic fakes, plus
//! the initiator-side Amoy verifier's anchor/found/mismatch/fail-closed behaviour. No network.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;
use crate::evm_abi::WORD;
use crate::nimiq::signer::InMemoryEnclaveKey;
use crate::polygon_gateway::{EvmLog, EvmRpcError};
use crate::rpc::MockRpc;
use crate::swap::SwapTerms;
use crate::swap_funding_verify::{
    FundingObservation, FundingVerifier, HtlcExpectation, MismatchReason,
};
use crate::swap_leg::sha256;

pub(crate) const SECRET: [u8; 32] = [0x42; 32];
pub(crate) const HTLC: EvmAddress = [0xDD; 20];
pub(crate) const USDC: EvmAddress = [0xCC; 20];
pub(crate) const ALICE_EVM_CLAIM: EvmAddress = [0xA5; 20];
pub(crate) const NOW_S: u64 = 1_000_000;
pub(crate) const NOW_MS: u64 = NOW_S * 1000;
pub(crate) const EVM_SECRET: [u8; 32] = [0x46; 32]; // the public EIP-155 spec key — never real/funded

pub(crate) fn hashlock() -> [u8; 32] {
    sha256(&SECRET)
}

pub(crate) fn ctx() -> SwapContext {
    SwapContext {
        swap_id: [0x7A; SWAP_ID_LEN],
        terms: SwapTerms {
            nim_timeout: 10_000,
            counterparty_timeout: 5_000,
        },
        hashlock: hashlock(),
        nim_address: [0x11; NIM_ADDRESS_LEN],
        btc_address: ALICE_EVM_CLAIM.to_vec(),
        btc_pubkey: {
            let mut k = [0x02; 33];
            k[0] = 0x02;
            k
        },
        give_amount: 500_000,   // 5 tNIM in luna
        take_amount: 1_000_000, // 1 USDC in micro-USDC
        network_id: crate::NetworkId::Testnet.wire_id(),
        term_anchor: 0,
    }
}

pub(crate) fn instant_poll() -> LivePollConfig {
    LivePollConfig {
        attempts: 3,
        interval_ms: 0,
    }
}

// --- a deterministic Amoy fake ---------------------------------------------------------------

#[derive(Default)]
pub(crate) struct MockAmoy {
    pub(crate) gas_price: u64,
    pub(crate) nonce: u64,
    pub(crate) balance: u128,
    /// Every raw broadcast, in order (byte-assert against locally built txs).
    pub(crate) broadcasts: Mutex<Vec<Vec<u8>>>,
    pub(crate) receipts: Mutex<HashMap<[u8; 32], EvmReceipt>>,
    /// When set, a broadcast is auto-mined successfully in this block.
    pub(crate) auto_mine_block: Option<u64>,
    /// swap_id → getSwap state word.
    pub(crate) swap_states: Mutex<HashMap<[u8; 32], u64>>,
    pub(crate) logs: Mutex<Vec<EvmLog>>,
    pub(crate) head: AtomicU64,
    pub(crate) broken: AtomicBool,
}

impl MockAmoy {
    fn err() -> EvmRpcError {
        EvmRpcError::Transport("mock: broken".to_string())
    }
    fn guard(&self) -> Result<(), EvmRpcError> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(Self::err());
        }
        Ok(())
    }
    pub(crate) fn add_receipt(&self, tx_hash: [u8; 32], success: bool, block_number: u64) {
        self.receipts.lock().unwrap().insert(
            tx_hash,
            EvmReceipt {
                tx_hash: format!("0x{}", bytes_to_hex(&tx_hash)),
                success,
                block_number,
            },
        );
    }
    pub(crate) fn broadcasts(&self) -> Vec<Vec<u8>> {
        self.broadcasts.lock().unwrap().clone()
    }
}

impl AmoyChain for MockAmoy {
    fn gas_price(&self) -> Result<u64, EvmRpcError> {
        self.guard()?;
        Ok(self.gas_price)
    }
    fn transaction_count(&self, _address: &EvmAddress) -> Result<u64, EvmRpcError> {
        self.guard()?;
        Ok(self.nonce)
    }
    fn balance(&self, _address: &EvmAddress) -> Result<u128, EvmRpcError> {
        self.guard()?;
        Ok(self.balance)
    }
    fn send_raw(&self, raw: &[u8]) -> Result<String, EvmRpcError> {
        self.guard()?;
        self.broadcasts.lock().unwrap().push(raw.to_vec());
        let hash = keccak256(raw);
        if let Some(block) = self.auto_mine_block {
            self.add_receipt(hash, true, block);
        }
        Ok(format!("0x{}", bytes_to_hex(&hash)))
    }
    fn receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<EvmReceipt>, EvmRpcError> {
        self.guard()?;
        Ok(self.receipts.lock().unwrap().get(tx_hash).cloned())
    }
    fn call(&self, _to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, EvmRpcError> {
        self.guard()?;
        // getSwap(bytes32): a 6-word tuple with only the state word meaningful.
        let mut id = [0u8; 32];
        id.copy_from_slice(&data[4..36]);
        let state = self
            .swap_states
            .lock()
            .unwrap()
            .get(&id)
            .copied()
            .unwrap_or(0);
        let mut out = vec![0u8; 6 * WORD];
        out[5 * WORD + 24..].copy_from_slice(&state.to_be_bytes());
        Ok(out)
    }
    fn new_swap_logs_to(
        &self,
        _htlc: &EvmAddress,
        _recipient: &EvmAddress,
        _from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError> {
        self.guard()?;
        Ok(self.logs.lock().unwrap().clone())
    }
    fn head(&self) -> Result<u64, EvmRpcError> {
        self.guard()?;
        Ok(self.head.load(Ordering::Relaxed))
    }
}

pub(crate) fn new_swap_log(
    swap_id: [u8; 32],
    amount: u64,
    hashlock: [u8; 32],
    timelock: u64,
    block: u64,
) -> EvmLog {
    let mut data = vec![0u8; 3 * WORD];
    data[24..32].copy_from_slice(&amount.to_be_bytes());
    data[32..64].copy_from_slice(&hashlock);
    data[88..96].copy_from_slice(&timelock.to_be_bytes());
    EvmLog {
        topic1: swap_id,
        data,
        block_number: block,
    }
}

// --- responder: fund_usdc --------------------------------------------------------------------

pub(crate) fn responder(
    amoy: Arc<MockAmoy>,
    book: Arc<PeerBook>,
    store: Arc<NimFundingStore>,
) -> LiveResponderSigner {
    LiveResponderSigner::new(LiveResponderConfig {
        evm_key: LocalEvmKey::from_secret(&EVM_SECRET).unwrap(),
        amoy,
        htlc: HTLC,
        usdc: USDC,
        peer_book: book,
        nim_claim_key: Arc::new(InMemoryEnclaveKey::from_secret(&[0x52; 32])),
        nim_rpc: Arc::new(MockRpc::new(500)),
        nim_store: store,
        gas: EvmGasConfig::default(),
        poll: instant_poll(),
    })
    .with_clock(Box::new(|| NOW_S))
}

#[test]
fn responder_funding_is_byte_exact_and_returns_the_new_swap_wire() {
    let amoy = Arc::new(MockAmoy {
        gas_price: 7_000_000_000, // below the floor — must clamp UP to 30 gwei
        nonce: 7,
        balance: u128::MAX,
        auto_mine_block: Some(1000),
        ..MockAmoy::default()
    });
    let book = Arc::new(PeerBook::new());
    let signer = responder(amoy.clone(), book.clone(), Arc::new(NimFundingStore::new()));

    // The peer's addressing arrives over the protocol (note_peer), exactly as the driver feeds it.
    signer.note_peer(ctx().swap_id, [0xA1; NIM_ADDRESS_LEN], &ALICE_EVM_CLAIM);
    let (wire, tx_id) = signer
        .build_funding(&ctx(), SwapLegId::Counterparty)
        .expect("funding builds");

    // Rebuild both txs independently: same key, clamped gas, sequential nonces (RFC-6979
    // signing is deterministic, so the bytes must match exactly).
    let key = LocalEvmKey::from_secret(&EVM_SECRET).unwrap();
    let gas = EvmGasConfig::default();
    let expected_approve = LegacyTx::polygon_amoy(
        7,
        gas.min_gas_price,
        gas.gas_approve,
        USDC,
        0,
        &erc20_approve(&HTLC, 1_000_000),
    )
    .sign_with(&key);
    let timelock = NOW_S + 5_000; // the ADR-0010 seconds mapping of T_B at the fixed clock
    let expected_new_swap = LegacyTx::polygon_amoy(
        8,
        gas.min_gas_price,
        gas.gas_new_swap,
        HTLC,
        0,
        &htlc_new_swap(&ALICE_EVM_CLAIM, 1_000_000, &hashlock(), timelock),
    )
    .sign_with(&key);

    assert_eq!(
        amoy.broadcasts(),
        vec![expected_approve, expected_new_swap.clone()]
    );
    assert_eq!(wire, expected_new_swap);
    assert_eq!(tx_id, keccak256(&expected_new_swap));
    // The wire must ride the swap TLV (one-byte value length).
    assert!(wire.len() <= 255, "newSwap wire is {} bytes", wire.len());
}

#[test]
fn responder_refuses_without_peer_addressing_or_budget() {
    // No note_peer yet → no receiver → nothing broadcast.
    let amoy = Arc::new(MockAmoy {
        gas_price: 30_000_000_000,
        balance: u128::MAX,
        auto_mine_block: Some(1000),
        ..MockAmoy::default()
    });
    let signer = responder(
        amoy.clone(),
        Arc::new(PeerBook::new()),
        Arc::new(NimFundingStore::new()),
    );
    assert!(signer
        .build_funding(&ctx(), SwapLegId::Counterparty)
        .is_none());
    assert!(amoy.broadcasts().is_empty());

    // Budget short of approve+newSwap+refund reserve → refused BEFORE any broadcast.
    let poor = Arc::new(MockAmoy {
        gas_price: 30_000_000_000,
        balance: 1_000, // dust
        auto_mine_block: Some(1000),
        ..MockAmoy::default()
    });
    let book = Arc::new(PeerBook::new());
    let signer = responder(poor.clone(), book, Arc::new(NimFundingStore::new()));
    signer.note_peer(ctx().swap_id, [0xA1; NIM_ADDRESS_LEN], &ALICE_EVM_CLAIM);
    assert!(signer
        .build_funding(&ctx(), SwapLegId::Counterparty)
        .is_none());
    assert!(poor.broadcasts().is_empty());

    // And the responder never funds the NIM leg.
    let amoy2 = Arc::new(MockAmoy::default());
    let signer = responder(
        amoy2.clone(),
        Arc::new(PeerBook::new()),
        Arc::new(NimFundingStore::new()),
    );
    assert!(signer.build_funding(&ctx(), SwapLegId::Nim).is_none());
}

#[test]
fn responder_nim_claim_is_byte_exact_against_the_proven_redeem_builder() {
    // The verifier noted the initiator's funding wire; the claim must redeem exactly that
    // contract to the session's NIM address, signed by the key that owns it.
    let claim_key = InMemoryEnclaveKey::from_secret(&[0x52; 32]);
    let claim_pk: [u8; 32] = EnclaveKey::public_key(&claim_key).try_into().unwrap();
    let claim_address = *Address::from_public_key(&claim_pk).as_bytes();

    let creation = HtlcCreation {
        funder: Address::from_bytes([0xA1; 20]),
        data: HtlcCreationData {
            htlc_sender: Address::from_bytes([0xA1; 20]),
            htlc_recipient: Address::from_bytes(claim_address),
            hash_algorithm: HashAlgorithm::Sha256,
            hash_root: hashlock(),
            hash_count: 1,
            timeout: NOW_MS + 10_000_000,
        },
        value: 500_000,
        fee: 0,
        validity_start_height: 400,
        network_id: crate::NetworkId::Testnet.wire_id(),
    };
    let store = Arc::new(NimFundingStore::new());
    store.note_wire(&creation.serialize_wire(&[0u8; 98]));

    let nim_rpc = Arc::new(MockRpc::new(500));
    let signer = LiveResponderSigner::new(LiveResponderConfig {
        evm_key: LocalEvmKey::from_secret(&EVM_SECRET).unwrap(),
        amoy: Arc::new(MockAmoy::default()),
        htlc: HTLC,
        usdc: USDC,
        peer_book: Arc::new(PeerBook::new()),
        nim_claim_key: Arc::new(InMemoryEnclaveKey::from_secret(&[0x52; 32])),
        nim_rpc: nim_rpc.clone(),
        nim_store: store,
        gas: EvmGasConfig::default(),
        poll: instant_poll(),
    });

    let mut c = ctx();
    c.nim_address = claim_address;
    let (wire, tx_id) = signer.build_claim(&c, SECRET).expect("claim builds");

    // Rebuild the redeem independently (deterministic Ed25519).
    let redeem = HtlcRedeem {
        contract: creation.contract_address(),
        recipient: Address::from_bytes(claim_address),
        value: 500_000,
        fee: 0,
        validity_start_height: 500, // the MockRpc head
        network_id: crate::NetworkId::Testnet.wire_id(),
    };
    let signature: [u8; 64] = claim_key
        .sign_content(redeem.serialize_content())
        .try_into()
        .unwrap();
    let proof = regular_transfer_proof(
        HashAlgorithm::Sha256,
        &hashlock(),
        &SECRET,
        &claim_pk,
        &signature,
    );
    let expected = redeem.serialize_wire(&proof);

    assert_eq!(nim_rpc.broadcasts(), vec![bytes_to_hex(&expected)]);
    assert_eq!(wire, expected);
    assert_eq!(tx_id, redeem.tx_hash());

    // A session address the claim key does NOT own is refused (misconfig guard).
    let mut wrong = ctx();
    wrong.nim_address = [0xEE; NIM_ADDRESS_LEN];
    assert!(signer.build_claim(&wrong, SECRET).is_none());
}

// --- initiator: fund_nim + claim_usdc ---------------------------------------------------------

pub(crate) fn initiator(
    nim_rpc: Arc<MockRpc>,
    amoy: Arc<MockAmoy>,
    book: Arc<PeerBook>,
    store: Arc<PolygonFundingStore>,
) -> LiveInitiatorSigner {
    LiveInitiatorSigner::new(LiveInitiatorConfig {
        nim_key: Arc::new(InMemoryEnclaveKey::from_secret(&[0x51; 32])),
        nim_rpc,
        evm_gas_key: LocalEvmKey::from_secret(&EVM_SECRET).unwrap(),
        amoy,
        htlc: HTLC,
        store,
        peer_book: book,
        gas: EvmGasConfig::default(),
        poll: instant_poll(),
    })
    .with_clock(Box::new(|| NOW_MS))
}

#[test]
fn initiator_nim_funding_is_byte_exact_against_the_proven_creation_builder() {
    let nim_rpc = Arc::new(MockRpc::new(777));
    let book = Arc::new(PeerBook::new());
    let signer = initiator(
        nim_rpc.clone(),
        Arc::new(MockAmoy::default()),
        book,
        Arc::new(PolygonFundingStore::new()),
    );
    // The responder's NIM claim address arrives via its Accept.
    signer.note_peer(ctx().swap_id, [0xB2; NIM_ADDRESS_LEN], b"unused-here");
    let (wire, tx_id) = signer
        .build_funding(&ctx(), SwapLegId::Nim)
        .expect("funding builds");

    // Rebuild the creation independently.
    let key = InMemoryEnclaveKey::from_secret(&[0x51; 32]);
    let pk: [u8; 32] = EnclaveKey::public_key(&key).try_into().unwrap();
    let funder = Address::from_public_key(&pk);
    let creation = HtlcCreation {
        funder,
        data: HtlcCreationData {
            htlc_sender: funder,
            htlc_recipient: Address::from_bytes([0xB2; NIM_ADDRESS_LEN]),
            hash_algorithm: HashAlgorithm::Sha256,
            hash_root: hashlock(),
            hash_count: 1,
            timeout: NOW_MS + 10_000 * 1000, // the ADR-0010 ms mapping of T_A
        },
        value: 500_000,
        fee: 0,
        validity_start_height: 777,
        network_id: crate::NetworkId::Testnet.wire_id(),
    };
    let signature: [u8; 64] = key
        .sign_content(creation.serialize_content())
        .try_into()
        .unwrap();
    let expected = creation.serialize_wire(&signature_proof_single_sig(&pk, &signature));

    assert_eq!(nim_rpc.broadcasts(), vec![bytes_to_hex(&expected)]);
    assert_eq!(wire, expected);
    assert_eq!(wire.len(), 248); // the signed HTLC creation's exact size — rides the TLV
    assert_eq!(tx_id, creation.tx_hash());
}

#[test]
fn initiator_refuses_non_testnet_and_missing_peer() {
    let nim_rpc = Arc::new(MockRpc::new(777));
    let book = Arc::new(PeerBook::new());
    let signer = initiator(
        nim_rpc.clone(),
        Arc::new(MockAmoy::default()),
        book,
        Arc::new(PolygonFundingStore::new()),
    );
    // No Accept seen yet → no recipient → refuse.
    assert!(signer.build_funding(&ctx(), SwapLegId::Nim).is_none());
    // A context stamped for any other network is never signed.
    signer.note_peer(ctx().swap_id, [0xB2; NIM_ADDRESS_LEN], b"x");
    let mut mainnet = ctx();
    mainnet.network_id = 24;
    assert!(signer.build_funding(&mainnet, SwapLegId::Nim).is_none());
    assert!(nim_rpc.broadcasts().is_empty());
    // And the initiator never funds the counterparty leg.
    assert!(signer
        .build_funding(&ctx(), SwapLegId::Counterparty)
        .is_none());
}

#[test]
fn initiator_claim_carries_s_first_and_matches_the_withdraw_tx() {
    let amoy = Arc::new(MockAmoy {
        gas_price: 90_000_000_000, // above the cap — must clamp DOWN to 50 gwei
        nonce: 3,
        balance: u128::MAX,
        auto_mine_block: Some(2000),
        head: AtomicU64::new(2001), // depth 2 — past the M3 reveal hold
        ..MockAmoy::default()
    });
    let store = Arc::new(PolygonFundingStore::new());
    let escrow = FoundEscrow {
        swap_id: [0x9F; 32],
        amount: 1_000_000,
        timelock_s: NOW_S + 5_000,
    };
    store.record_found(hashlock(), escrow);
    let signer = initiator(
        Arc::new(MockRpc::new(1)),
        amoy.clone(),
        Arc::new(PeerBook::new()),
        store,
    );

    let (wire, tx_id) = signer.build_claim(&ctx(), SECRET).expect("claim lands");

    let key = LocalEvmKey::from_secret(&EVM_SECRET).unwrap();
    let gas = EvmGasConfig::default();
    let expected = LegacyTx::polygon_amoy(
        3,
        gas.max_gas_price,
        gas.gas_withdraw,
        HTLC,
        0,
        &htlc_withdraw(&[0x9F; 32], &SECRET),
    )
    .sign_with(&key);

    assert_eq!(&wire[..32], &SECRET); // S first — what the responder reads off the reveal
    assert_eq!(&wire[32..], expected.as_slice());
    assert_eq!(tx_id, keccak256(&expected));
    assert!(wire.len() <= 255, "claim wire is {} bytes", wire.len());
    assert_eq!(amoy.broadcasts(), vec![expected]);
}

#[test]
fn initiator_claim_requires_a_verified_escrow_and_a_mined_success() {
    // No verified escrow recorded → nothing to withdraw from → None, no broadcast.
    let amoy = Arc::new(MockAmoy {
        gas_price: 30_000_000_000,
        balance: u128::MAX,
        auto_mine_block: Some(2000),
        ..MockAmoy::default()
    });
    let signer = initiator(
        Arc::new(MockRpc::new(1)),
        amoy.clone(),
        Arc::new(PeerBook::new()),
        Arc::new(PolygonFundingStore::new()),
    );
    assert!(signer.build_claim(&ctx(), SECRET).is_none());
    assert!(amoy.broadcasts().is_empty());

    // A withdraw that never mines within the poll budget must NOT report a reveal.
    let stuck = Arc::new(MockAmoy {
        gas_price: 30_000_000_000,
        balance: u128::MAX,
        auto_mine_block: None, // broadcast accepted, never mined
        ..MockAmoy::default()
    });
    let store = Arc::new(PolygonFundingStore::new());
    store.record_found(
        hashlock(),
        FoundEscrow {
            swap_id: [0x9F; 32],
            amount: 1_000_000,
            timelock_s: NOW_S + 5_000,
        },
    );
    let signer = initiator(
        Arc::new(MockRpc::new(1)),
        stuck,
        Arc::new(PeerBook::new()),
        store,
    );
    assert!(signer.build_claim(&ctx(), SECRET).is_none());
}

// --- the initiator-side Amoy verifier ----------------------------------------------------------

fn amoy_verifier(chain: Arc<MockAmoy>, store: Arc<PolygonFundingStore>) -> AmoyHtlcSwapVerifier {
    AmoyHtlcSwapVerifier::new(chain, HTLC, ALICE_EVM_CLAIM, store).with_clock(Box::new(|| NOW_S))
}

pub(crate) fn counterparty_expect() -> HtlcExpectation {
    HtlcExpectation {
        leg: SwapLegId::Counterparty,
        hashlock: hashlock(),
        min_amount: 1_000_000,
        min_timeout: 5_000, // term units — the verifier maps the on-chain seconds back
        recipient: ctx().btc_pubkey.to_vec(), // the session's 33-byte key bytes — IGNORED by design
        term_anchor: 0,
    }
}

#[test]
fn amoy_verifier_is_absent_until_the_funding_proof_names_a_mined_tx() {
    let chain = Arc::new(MockAmoy {
        head: AtomicU64::new(200),
        ..MockAmoy::default()
    });
    let store = Arc::new(PolygonFundingStore::new());
    let v = amoy_verifier(chain.clone(), store.clone());
    // Nothing noted → no anchor → Absent (fail-closed: the capped RPC can't scan blind).
    assert_eq!(
        v.observe(&counterparty_expect()),
        FundingObservation::Absent
    );

    // A FundingProof names the newSwap tx, but it hasn't mined yet → still Absent.
    let raw_funding = vec![0xF0; 100];
    v.note_funding_wire(SwapLegId::Counterparty, &raw_funding);
    assert_eq!(
        v.observe(&counterparty_expect()),
        FundingObservation::Absent
    );

    // Once it mines AND the matching live escrow log is visible, the gate opens with real depth
    // — and the found swapId is recorded for the claim.
    chain.add_receipt(keccak256(&raw_funding), true, 100);
    chain.logs.lock().unwrap().push(new_swap_log(
        [0x9F; 32],
        1_000_000,
        hashlock(),
        NOW_S + 5_000,
        100,
    ));
    chain.swap_states.lock().unwrap().insert([0x9F; 32], 1); // Live
    let obs = v.observe(&counterparty_expect());
    assert_eq!(
        obs,
        FundingObservation::Found {
            amount: 1_000_000,
            timeout: 5_000 + DEFAULT_TIMELOCK_SLACK_S, // mapped back to term units + slack
            confirmations: 101,                        // 200 - 100 + 1
        }
    );
    assert_eq!(
        store.found(&hashlock()),
        Some(FoundEscrow {
            swap_id: [0x9F; 32],
            amount: 1_000_000,
            timelock_s: NOW_S + 5_000,
        })
    );
    // And the full gate passes at the testnet USDC depth.
    use crate::swap_funding_verify::require_funded;
    assert!(require_funded(&obs, &counterparty_expect(), 5).is_ok());
}

#[test]
fn amoy_verifier_mismatch_resolved_and_failure_semantics() {
    // An escrow paying us under a DIFFERENT hashlock → Mismatch(Hashlock).
    let chain = Arc::new(MockAmoy {
        head: AtomicU64::new(200),
        ..MockAmoy::default()
    });
    let store = Arc::new(PolygonFundingStore::new());
    let v = amoy_verifier(chain.clone(), store);
    let raw = vec![0xF1; 80];
    v.note_funding_wire(SwapLegId::Counterparty, &raw);
    chain.add_receipt(keccak256(&raw), true, 100);
    chain.logs.lock().unwrap().push(new_swap_log(
        [0x9E; 32],
        1_000_000,
        sha256(&[0xEE; 32]),
        NOW_S + 5_000,
        100,
    ));
    chain.swap_states.lock().unwrap().insert([0x9E; 32], 1);
    assert_eq!(
        v.observe(&counterparty_expect()),
        FundingObservation::Mismatch(MismatchReason::Hashlock)
    );

    // A resolved (claimed) slot is not funding — with no other escrow paying us, it reads
    // Absent (the stateless re-check that also refuses a reorg-reburied escrow).
    *chain.logs.lock().unwrap() = vec![new_swap_log(
        [0x9D; 32],
        1_000_000,
        hashlock(),
        NOW_S + 5_000,
        101,
    )];
    chain.swap_states.lock().unwrap().insert([0x9D; 32], 2); // Claimed
    assert_eq!(
        v.observe(&counterparty_expect()),
        FundingObservation::Absent
    );

    // Transport failure reads Absent (fail-closed), and the NIM leg is not this chain.
    chain.broken.store(true, Ordering::Relaxed);
    assert_eq!(
        v.observe(&counterparty_expect()),
        FundingObservation::Absent
    );
    chain.broken.store(false, Ordering::Relaxed);
    let mut nim = counterparty_expect();
    nim.leg = SwapLegId::Nim;
    assert_eq!(v.observe(&nim), FundingObservation::Absent);
}

#[test]
fn peer_book_is_first_report_wins() {
    let book = PeerBook::new();
    let id = [1u8; SWAP_ID_LEN];
    book.note(
        id,
        PeerAddressing {
            nim_address: [0xB2; NIM_ADDRESS_LEN],
            chain_address: ALICE_EVM_CLAIM.to_vec(),
        },
    );
    // A later (possibly forged) report must not displace the first — mirroring the
    // coordinator's first-Accept-wins.
    book.note(
        id,
        PeerAddressing {
            nim_address: [0xEE; NIM_ADDRESS_LEN],
            chain_address: vec![0xEE; 20],
        },
    );
    assert_eq!(book.get(&id).unwrap().nim_address, [0xB2; NIM_ADDRESS_LEN]);
}

#[test]
fn the_seconds_mapping_round_trips_with_slack() {
    assert_eq!(usdc_timelock_s(5_000, 0, NOW_S), NOW_S + 5_000);
    assert_eq!(
        term_equivalent_of_timelock_s(NOW_S + 5_000, NOW_S + 500, 0, 900),
        5_400
    );
    // A non-zero anchor shifts both sides identically.
    assert_eq!(usdc_timelock_s(5_000, 2_000, NOW_S), NOW_S + 3_000);
    assert_eq!(
        term_equivalent_of_timelock_s(NOW_S + 3_000, NOW_S + 500, 2_000, 900),
        5_400
    );
}
