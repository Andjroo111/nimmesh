//! Offline tests for [`crate::swap_live_ffi`] — the live-participant door's validation,
//! guard pinning, the C1 gate at the FFI surface, the one-shot latch, the lock book, and the
//! refunder — all against deterministic fakes; no network. The bindings-parity refusals
//! (`Unsupported` without the features) get their own `cfg(not(...))` twins.

use super::*;

// --- shared bits (always compiled) ---------------------------------------------------------------

fn lock() -> FfiNimLock {
    FfiNimLock {
        contract: "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".into(),
        value: 500_000,
        timeout_ms: 1_000_000_000,
        funding_tx_hash: "aa".repeat(32),
    }
}

#[test]
fn the_lock_book_dedupes_and_bounds() {
    let book = LiveLockBook::new();
    assert!(book.locks().is_empty());
    book.record(lock());
    book.record(lock()); // same contract → deduped
    assert_eq!(book.locks().len(), 1);
    for i in 0..40u32 {
        let mut l = lock();
        l.contract = format!("NQ{i:02} FAKE");
        book.record(l);
    }
    assert!(book.locks().len() <= 32, "the book is bounded");
}

#[test]
fn the_mainnet_arm_gate_refuses_unless_flag_and_address() {
    // Disabled master switch → refused regardless of the address.
    assert!(matches!(
        mainnet_htlc_if_armed(false, ""),
        Err(LiveSwapFfiError::Refused { .. })
    ));
    assert!(matches!(
        mainnet_htlc_if_armed(false, "0xb3B3703E07AC897B7E3e864C113a2Fa547D76736"),
        Err(LiveSwapFfiError::Refused { .. })
    ));
    // Enabled but no recorded HTLC → refused (a flag without an address does nothing).
    assert!(matches!(
        mainnet_htlc_if_armed(true, ""),
        Err(LiveSwapFfiError::Refused { .. })
    ));
    assert!(matches!(
        mainnet_htlc_if_armed(true, "   "),
        Err(LiveSwapFfiError::Refused { .. })
    ));
    // Fully armed (both true) → Ok(trimmed address).
    assert_eq!(
        mainnet_htlc_if_armed(true, " 0xF00Bar ").unwrap(),
        "0xF00Bar"
    );
}

#[test]
fn the_shipped_mainnet_probes_are_honest() {
    // State-agnostic: the aggregate probe is exactly flag AND address, and the reason string
    // matches the state — the app's labels can trust both, on unarmed main OR the arming release.
    let armed = crate::mainnet_swap::MAINNET_SWAP_ENABLED
        && !crate::mainnet_swap::MAINNET_HTLC_ADDRESS.is_empty();
    assert_eq!(mainnet_swap_armed(), armed);
    let reason = mainnet_swap_reason().to_lowercase();
    if armed {
        assert!(
            reason.contains("armed") && reason.contains("escrow"),
            "an armed build's reason names the armed state + escrow, got: {reason}"
        );
    } else {
        assert!(
            reason.contains("disabled") || reason.contains("no deployed"),
            "an unarmed build's reason names what is missing, got: {reason}"
        );
    }
}

// --- without the live features: every door refuses honestly (bindings parity) --------------------

#[cfg(not(all(feature = "polygon-gateway", feature = "gateway-rpc")))]
mod unsupported {
    use super::*;
    use crate::mock_radio::{MockEther, MockRadio};

    fn initiator_config() -> FfiLiveInitiatorConfig {
        FfiLiveInitiatorConfig {
            nim_luna: 500_000,
            usdc_micro: 1_000_000,
            expiry_height: 1_000_000,
            intent_seed: crate::swap_secret::test_seed(1).to_vec(),
            evm_claim_address: vec![0xA5; 20],
            evm_gas_secret: crate::swap_secret::test_seed(3).to_vec(),
            nim_rpc_url: "https://rpc.testnet.nimiqwatch.com".into(),
            amoy_rpc_url: "https://rpc-amoy.polygon.technology".into(),
            htlc_address: "0xb3B3703E07AC897B7E3e864C113a2Fa547D76736".into(),
            delta_safe_blocks: 0,
            min_claim_window_blocks: 0,
        }
    }

    #[test]
    fn every_live_door_refuses_without_the_features() {
        let radio = MockRadio::new("u", MockEther::new());
        let got = crate::node::MeshNode::new_live_swap_initiator(
            b"u".to_vec(),
            radio,
            std::sync::Arc::new(crate::nimiq::signer::InMemoryEnclaveKey::from_secret(
                &[1; 32],
            )),
            LiveLockBook::new(),
            initiator_config(),
            None,
        );
        assert!(matches!(got, Err(LiveSwapFfiError::Unsupported)));

        // The MAINNET doors keep bindings parity: without the live features they refuse with
        // `Unsupported` (the arm gate is never even consulted) — and the probe reports unarmed.
        let radio = MockRadio::new("um", MockEther::new());
        let got = crate::node::MeshNode::new_live_swap_initiator_mainnet(
            b"um".to_vec(),
            radio,
            std::sync::Arc::new(crate::nimiq::signer::InMemoryEnclaveKey::from_secret(
                &[1; 32],
            )),
            LiveLockBook::new(),
            initiator_config(),
            None,
        );
        assert!(matches!(got, Err(LiveSwapFfiError::Unsupported)));
        // Armed or not, a featureless build refuses — the Unsupported assert IS the invariant.
    }

    #[cfg(not(feature = "polygon-leg"))]
    #[test]
    fn evm_address_derivation_refuses_without_polygon_leg() {
        assert!(matches!(
            evm_address_for_secret(vec![0x46; 32]),
            Err(LiveSwapFfiError::Unsupported)
        ));
    }

    #[cfg(not(feature = "gateway-rpc"))]
    #[test]
    fn the_refunder_refuses_without_gateway_rpc() {
        let key = std::sync::Arc::new(crate::nimiq::signer::InMemoryEnclaveKey::from_secret(
            &[1; 32],
        ));
        assert!(matches!(
            NimHtlcRefunder::new(key, "https://rpc.testnet.nimiqwatch.com".into()),
            Err(LiveSwapFfiError::Unsupported)
        ));
    }

    #[cfg(not(feature = "gateway-rpc"))]
    #[test]
    fn the_mainnet_refunder_refuses_without_gateway_rpc() {
        let key = std::sync::Arc::new(crate::nimiq::signer::InMemoryEnclaveKey::from_secret(
            &[1; 32],
        ));
        assert!(matches!(
            NimHtlcRefunder::new_mainnet(key, "https://rpc.nimiqwatch.com".into()),
            Err(LiveSwapFfiError::Unsupported)
        ));
    }
}

// --- with the live features: the full door -------------------------------------------------------

#[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
mod live {
    use std::sync::Arc;

    use super::live_impl::{build_live_intent, LockRecordingSigner, OneShotSigner};
    use super::*;
    use crate::mock_radio::{MockEther, MockRadio};
    use crate::nimiq::address::Address;
    use crate::nimiq::hex::bytes_to_hex;
    use crate::nimiq::htlc::{
        timeout_resolve_proof, HashAlgorithm, HtlcCreation, HtlcCreationData, HtlcRedeem,
    };
    use crate::nimiq::signer::{EnclaveKey, InMemoryEnclaveKey};
    use crate::node::MeshNode;
    use crate::radio::BleRadio;
    use crate::rpc::{MockRpc, RpcAccount};
    use crate::swap_coordinator::SwapContext;
    use crate::swap_intent::Asset;
    use crate::swap_signer::SwapSigner;
    use crate::swap_wire::{SwapLegId, NIM_ADDRESS_LEN, SWAP_ID_LEN};

    const HTLC_HEX: &str = "0xb3B3703E07AC897B7E3e864C113a2Fa547D76736";
    const USDC_HEX: &str = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582";

    pub(super) fn radio(id: &str) -> Arc<dyn BleRadio> {
        MockRadio::new(id, MockEther::new())
    }

    pub(super) fn wallet_key() -> Arc<dyn EnclaveKey> {
        Arc::new(InMemoryEnclaveKey::from_secret(&[0x51; 32]))
    }

    pub(super) fn init_cfg() -> FfiLiveInitiatorConfig {
        FfiLiveInitiatorConfig {
            nim_luna: 500_000,
            usdc_micro: 1_000_000,
            expiry_height: 1_000_000,
            intent_seed: crate::swap_secret::test_seed(1).to_vec(),
            evm_claim_address: vec![0xA5; 20],
            evm_gas_secret: crate::swap_secret::test_seed(3).to_vec(),
            nim_rpc_url: "https://rpc.testnet.nimiqwatch.com".into(),
            amoy_rpc_url: "https://rpc-amoy.polygon.technology".into(),
            htlc_address: HTLC_HEX.into(),
            delta_safe_blocks: 0,
            min_claim_window_blocks: 0,
        }
    }

    pub(super) fn resp_cfg() -> FfiLiveResponderConfig {
        FfiLiveResponderConfig {
            usdc_micro: 1_000_000,
            nim_luna: 500_000,
            expiry_height: 1_000_000,
            nim_claim_seed: crate::swap_secret::test_seed(2).to_vec(),
            evm_funding_secret: crate::swap_secret::test_seed(4).to_vec(),
            nim_rpc_url: "https://rpc.testnet.nimiqwatch.com".into(),
            amoy_rpc_url: "https://rpc-amoy.polygon.technology".into(),
            htlc_address: HTLC_HEX.into(),
            usdc_address: USDC_HEX.into(),
            delta_safe_blocks: 0,
            min_claim_window_blocks: 0,
        }
    }

    fn build_initiator(
        cfg: FfiLiveInitiatorConfig,
        gw: Option<String>,
    ) -> Result<Arc<MeshNode>, LiveSwapFfiError> {
        MeshNode::new_live_swap_initiator(
            b"ph".to_vec(),
            radio("i"),
            wallet_key(),
            LiveLockBook::new(),
            cfg,
            gw,
        )
    }

    /// G11 (#82): every 32-byte secret at the live door is entropy-gated, not just length-checked.
    /// `intent_seed`/`nim_claim_seed` are Ed25519 seeds — every 32-byte value is valid, so nothing
    /// downstream rejects a zero-filled buffer left by a swallowed CSPRNG error. (The secp256k1
    /// secrets were incidentally protected: zero is not a valid scalar. That accident does not
    /// cover a stuck-byte value, which is why the gate applies to all four.)
    #[test]
    fn the_live_doors_refuse_secrets_with_no_entropy() {
        for (label, mutate) in [
            (
                "intent_seed zeros",
                (|c: &mut FfiLiveInitiatorConfig| c.intent_seed = vec![0u8; 32])
                    as fn(&mut FfiLiveInitiatorConfig),
            ),
            (
                "intent_seed stuck byte",
                |c: &mut FfiLiveInitiatorConfig| c.intent_seed = vec![7u8; 32],
            ),
            (
                "evm_gas_secret stuck byte",
                |c: &mut FfiLiveInitiatorConfig| c.evm_gas_secret = vec![0x46u8; 32],
            ),
        ] {
            let mut c = init_cfg();
            mutate(&mut c);
            assert!(
                matches!(
                    build_initiator(c, None),
                    Err(LiveSwapFfiError::BadInput { .. })
                ),
                "the initiator door must refuse: {label}"
            );
        }

        let mut c = resp_cfg();
        c.nim_claim_seed = vec![0u8; 32];
        assert!(
            matches!(
                MeshNode::new_live_swap_responder(b"ph".to_vec(), radio("r"), c, None),
                Err(LiveSwapFfiError::BadInput { .. })
            ),
            "the responder door must refuse a zero nim_claim_seed"
        );
    }

    #[test]
    fn the_initiator_door_validates_every_field() {
        let mut c = init_cfg();
        c.intent_seed = vec![7; 16];
        assert!(matches!(
            build_initiator(c, None),
            Err(LiveSwapFfiError::BadInput { .. })
        ));

        let mut c = init_cfg();
        c.evm_claim_address = vec![0xA5; 19];
        assert!(matches!(
            build_initiator(c, None),
            Err(LiveSwapFfiError::BadInput { .. })
        ));

        let mut c = init_cfg();
        c.evm_gas_secret = vec![0; 32]; // zero is not a valid secp256k1 scalar
        assert!(matches!(
            build_initiator(c, None),
            Err(LiveSwapFfiError::BadInput { .. })
        ));

        let mut c = init_cfg();
        c.nim_luna = 0;
        assert!(matches!(
            build_initiator(c, None),
            Err(LiveSwapFfiError::BadInput { .. })
        ));

        let mut c = init_cfg();
        c.htlc_address = "0x1234".into();
        assert!(matches!(
            build_initiator(c, None),
            Err(LiveSwapFfiError::BadInput { .. })
        ));
    }

    #[test]
    fn the_guards_pin_both_chains_at_the_door() {
        // A known NIM mainnet host is refused outright…
        let mut c = init_cfg();
        c.nim_rpc_url = "https://rpc.nimiqwatch.com".into();
        assert!(matches!(
            build_initiator(c, None),
            Err(LiveSwapFfiError::Refused { .. })
        ));
        // …and so is a Polygon MAINNET host (guard_amoy).
        let mut c = init_cfg();
        c.amoy_rpc_url = "https://polygon-rpc.com".into();
        assert!(matches!(
            build_initiator(c, None),
            Err(LiveSwapFfiError::Refused { .. })
        ));
        // The gateway hop is testnet-guarded too.
        assert!(matches!(
            build_initiator(init_cfg(), Some("https://rpc.nimiqwatch.com".into())),
            Err(LiveSwapFfiError::Refused { .. })
        ));
    }

    #[test]
    fn both_live_doors_construct_and_shut_down() {
        // No network IO at construction: the HTTP clients only guard/parse here. The C1
        // gate passes by construction (chain-backed verifiers + PRF secret sources) — this
        // IS the review's safe-entry-point requirement, exercised.
        let node = build_initiator(init_cfg(), None).expect("initiator constructs");
        assert!(node.active_swaps().is_empty());
        node.shutdown();

        let node = MeshNode::new_live_swap_responder(
            b"mac".to_vec(),
            radio("r"),
            resp_cfg(),
            Some("https://rpc.testnet.nimiqwatch.com".into()),
        )
        .expect("responder constructs (with the gateway hop)");
        node.shutdown();
    }

    #[test]
    fn the_live_intents_carry_the_evm_addressing_and_verify() {
        let seed = [7u8; 32];
        let claim: [u8; 20] = [0xA5; 20];
        let intent = build_live_intent(Asset::Nim, 500_000, 1_000_000, 1_000, &seed, claim)
            .expect("initiator intent");
        assert!(intent.verify_authentic());
        assert_eq!(intent.gives, Asset::Nim);
        assert_eq!(intent.counter_asset, Asset::Usdc);
        assert_eq!(intent.evm_address, claim);
        assert_eq!(intent.btc_address, claim.to_vec()); // the protocol-carried payout bytes
        assert_eq!(intent.network_id, crate::NetworkId::Testnet.wire_id());

        // Degenerate configs are refused; the pair is NIM⇄USDC only.
        assert!(build_live_intent(Asset::Nim, 0, 1, 1_000, &seed, claim).is_err());
        assert!(build_live_intent(Asset::Nim, 1, 1, 0, &seed, claim).is_err());
        assert!(build_live_intent(Asset::Btc, 1, 1, 1_000, &seed, claim).is_err());

        // Two adverts with distinct seeds do not link (fresh identity + fresh filler key).
        let other = build_live_intent(Asset::Nim, 500_000, 1_000_000, 1_000, &[8; 32], claim)
            .expect("second advert");
        assert_ne!(intent.nim_address, other.nim_address);
        assert_ne!(intent.btc_pubkey, other.btc_pubkey);
    }

    // --- the wrappers ---------------------------------------------------------------------------

    /// A fake signer with scriptable funding output + live flag.
    struct FakeSigner {
        wire: Option<Vec<u8>>,
        live: bool,
        fundings: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SwapSigner for FakeSigner {
        fn build_funding(
            &self,
            _ctx: &SwapContext,
            _leg: SwapLegId,
        ) -> Option<(Vec<u8>, [u8; 32])> {
            self.fundings
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.wire.clone().map(|w| (w, [0xC1; 32]))
        }
        fn build_claim(&self, _ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
            Some((secret.to_vec(), [0xC3; 32]))
        }
        fn is_live(&self) -> bool {
            self.live
        }
    }

    fn sctx() -> SwapContext {
        SwapContext {
            swap_id: [0x7A; SWAP_ID_LEN],
            terms: crate::swap::SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            hashlock: [0xC7; 32],
            nim_address: [0x11; NIM_ADDRESS_LEN],
            btc_address: vec![0x22; 20],
            btc_pubkey: [0x02; 33],
            give_amount: 500_000,
            take_amount: 1_000_000,
            network_id: 5,
            term_anchor: 0,
        }
    }

    fn creation_wire() -> Vec<u8> {
        HtlcCreation {
            funder: Address::from_bytes([0xA1; 20]),
            data: HtlcCreationData {
                htlc_sender: Address::from_bytes([0xA1; 20]),
                htlc_recipient: Address::from_bytes([0xB2; 20]),
                hash_algorithm: HashAlgorithm::Sha256,
                hash_root: [0xC7; 32],
                hash_count: 1,
                timeout: 1_000_000_000,
            },
            value: 500_000,
            fee: 0,
            validity_start_height: 400,
            network_id: crate::NetworkId::Testnet.wire_id(),
        }
        .serialize_wire(&[0u8; 98])
    }

    #[test]
    fn the_recorder_books_exactly_what_the_nim_leg_broadcast() {
        let book = LiveLockBook::new();
        let signer = LockRecordingSigner::new(
            FakeSigner {
                wire: Some(creation_wire()),
                live: true,
                fundings: Arc::default(),
            },
            book.clone(),
        );
        // The counterparty leg never books.
        signer.build_funding(&sctx(), SwapLegId::Counterparty);
        assert!(book.locks().is_empty());
        // The NIM leg books the DECODED broadcast wire (contract, value, timeout, tx hash).
        signer.build_funding(&sctx(), SwapLegId::Nim);
        let locks = book.locks();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].value, 500_000);
        assert_eq!(locks[0].timeout_ms, 1_000_000_000);
        assert!(SwapSigner::is_live(&signer)); // C1: the wrapper forwards
    }

    #[test]
    fn a_failed_funding_books_nothing() {
        let book = LiveLockBook::new();
        let signer = LockRecordingSigner::new(
            FakeSigner {
                wire: None,
                live: true,
                fundings: Arc::default(),
            },
            book.clone(),
        );
        assert!(signer.build_funding(&sctx(), SwapLegId::Nim).is_none());
        assert!(book.locks().is_empty());
    }

    #[test]
    fn the_one_shot_latch_permits_exactly_one_funding() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let signer = OneShotSigner::new(FakeSigner {
            wire: Some(vec![0x11; 8]),
            live: true,
            fundings: count.clone(),
        });
        assert!(signer.build_funding(&sctx(), SwapLegId::Nim).is_some());
        // The latch fired — a repeat match stays unfunded, the inner signer is not even asked.
        assert!(signer.build_funding(&sctx(), SwapLegId::Nim).is_none());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Claims still pass through, and the live flag forwards (C1).
        assert!(signer.build_claim(&sctx(), [0x42; 32]).is_some());
        assert!(SwapSigner::is_live(&signer));
    }

    #[test]
    fn a_failed_first_funding_does_not_burn_the_latch() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let signer = OneShotSigner::new(FakeSigner {
            wire: None,
            live: false,
            fundings: count.clone(),
        });
        assert!(signer.build_funding(&sctx(), SwapLegId::Nim).is_none());
        assert!(signer.build_funding(&sctx(), SwapLegId::Nim).is_none());
        // Both attempts reached the inner signer — a failure must stay retryable.
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    // --- the refunder -----------------------------------------------------------------------

    const NOW_MS: u64 = 2_000_000_000;

    fn refunder_over(rpc: Arc<MockRpc>) -> NimHtlcRefunder {
        NimHtlcRefunder::from_parts(Arc::new(InMemoryEnclaveKey::from_secret(&[0x51; 32])), rpc)
            .with_clock(|| NOW_MS)
    }

    #[test]
    fn the_refunder_is_idempotent_and_byte_exact() {
        let key = InMemoryEnclaveKey::from_secret(&[0x51; 32]);
        let pk: [u8; 32] = EnclaveKey::public_key(&key).try_into().unwrap();
        let funder = Address::from_public_key(&pk);
        let contract = Address::from_bytes([0xCA; 20]);
        let contract_nq = contract.to_user_friendly();
        let mut lock = super::tests_lock_for(&contract_nq);

        // Still time-locked → StillLocked, nothing broadcast.
        let rpc = Arc::new(MockRpc::new(500));
        rpc.set_account(
            &contract_nq,
            RpcAccount {
                balance: 500_000,
                account_type: "htlc".into(),
                address: Some(contract_nq.clone()),
            },
        );
        lock.timeout_ms = NOW_MS + 10_000;
        let r = refunder_over(rpc.clone());
        assert_eq!(
            r.refund(lock.clone()).unwrap(),
            FfiRefundOutcome::StillLocked {
                until_ms: NOW_MS + 10_000 + 60_000
            }
        );
        assert!(rpc.broadcasts().is_empty());

        // Past the timeout (+ margin) → the broadcast is the byte-exact TimeoutResolve
        // paying the FUNDER back everything the contract still holds.
        lock.timeout_ms = NOW_MS - 100_000;
        let out = r.refund(lock.clone()).unwrap();
        let redeem = HtlcRedeem {
            contract,
            recipient: funder,
            value: 500_000,
            fee: 0,
            validity_start_height: 500,
            network_id: crate::NetworkId::Testnet.wire_id(),
        };
        let sig: [u8; 64] = key
            .sign_content(redeem.serialize_content())
            .try_into()
            .unwrap();
        let expected = redeem.serialize_wire(&timeout_resolve_proof(&pk, &sig));
        assert_eq!(rpc.broadcasts(), vec![bytes_to_hex(&expected)]);
        assert_eq!(
            out,
            FfiRefundOutcome::Refunded {
                tx_hash: bytes_to_hex(&redeem.tx_hash())
            }
        );

        // Once the contract is emptied, the same call reads AlreadyResolved — the caller's
        // signal to forget the lock (only chain truth releases it).
        rpc.set_account(
            &contract_nq,
            RpcAccount {
                balance: 0,
                account_type: "htlc".into(),
                address: Some(contract_nq.clone()),
            },
        );
        assert_eq!(r.refund(lock).unwrap(), FfiRefundOutcome::AlreadyResolved);
    }

    #[test]
    fn the_mainnet_refunder_stamps_the_mainnet_wire_id() {
        // The 20-NIM strand of 2026-07-15: a MAINNET lock refunded through a testnet-stamped
        // refunder builds an unrelayable tx. The network-aware seam must stamp the refund
        // with the refunder's OWN network id — byte-exact, like the testnet twin above.
        let key = InMemoryEnclaveKey::from_secret(&[0x51; 32]);
        let pk: [u8; 32] = EnclaveKey::public_key(&key).try_into().unwrap();
        let funder = Address::from_public_key(&pk);
        let contract = Address::from_bytes([0xCA; 20]);
        let contract_nq = contract.to_user_friendly();
        let mut lock = super::tests_lock_for(&contract_nq);
        lock.timeout_ms = NOW_MS - 100_000;

        let rpc = Arc::new(MockRpc::new(500));
        rpc.set_account(
            &contract_nq,
            RpcAccount {
                balance: 500_000,
                account_type: "htlc".into(),
                address: Some(contract_nq.clone()),
            },
        );
        let r = NimHtlcRefunder::from_parts_on(
            Arc::new(InMemoryEnclaveKey::from_secret(&[0x51; 32])),
            rpc.clone(),
            crate::NetworkId::Mainnet,
        )
        .with_clock(|| NOW_MS);
        let out = r.refund(lock).unwrap();

        let redeem = HtlcRedeem {
            contract,
            recipient: funder,
            value: 500_000,
            fee: 0,
            validity_start_height: 500,
            network_id: crate::NetworkId::Mainnet.wire_id(),
        };
        let sig: [u8; 64] = key
            .sign_content(redeem.serialize_content())
            .try_into()
            .unwrap();
        let expected = redeem.serialize_wire(&timeout_resolve_proof(&pk, &sig));
        assert_eq!(rpc.broadcasts(), vec![bytes_to_hex(&expected)]);
        assert_eq!(
            out,
            FfiRefundOutcome::Refunded {
                tx_hash: bytes_to_hex(&redeem.tx_hash())
            }
        );

        // And the two stamps genuinely differ — the testnet twin cannot satisfy this wire.
        assert_ne!(
            crate::NetworkId::Mainnet.wire_id(),
            crate::NetworkId::Testnet.wire_id()
        );
    }

    #[cfg(feature = "gateway-rpc")]
    #[test]
    fn the_mainnet_refunder_ctor_refuses_a_testnet_url() {
        let key = Arc::new(InMemoryEnclaveKey::from_secret(&[1; 32]));
        assert!(matches!(
            NimHtlcRefunder::new_mainnet(key, "https://rpc.testnet.nimiqwatch.com".into()),
            Err(crate::swap_live_ffi::LiveSwapFfiError::Refused { .. })
        ));
    }

    #[cfg(feature = "polygon-leg")]
    #[test]
    fn evm_address_derivation_matches_the_signer_and_validates() {
        let secret = vec![0x46; 32];
        let addr = evm_address_for_secret(secret.clone()).expect("derives");
        let key = crate::evm_signer::LocalEvmKey::from_secret(&[0x46; 32]).unwrap();
        assert_eq!(addr, format!("0x{}", bytes_to_hex(&key.address())));
        assert!(evm_address_for_secret(vec![1; 16]).is_err());
        assert!(evm_address_for_secret(vec![0; 32]).is_err());
    }
}

/// A lock fixture for a given contract (shared with the featured module).
#[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
fn tests_lock_for(contract_nq: &str) -> FfiNimLock {
    FfiNimLock {
        contract: contract_nq.to_string(),
        value: 500_000,
        timeout_ms: 0,
        funding_tx_hash: "aa".repeat(32),
    }
}

// --- MAINNET assembly (armed with an INJECTED HTLC address; the shipped const stays off) --------
// Extracted to a sibling when this file hit the 800-line ceiling. Declared here at the top level
// rather than inside `live` because a #[path] on a nested inline module resolves against
// `src/<mod name>/`; at this level the base is `src/`. It reuses `live`'s fixtures via `super::live`.
#[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
#[path = "swap_live_ffi_mainnet_tests.rs"]
mod mainnet;
