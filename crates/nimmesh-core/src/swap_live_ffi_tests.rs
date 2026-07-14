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
            intent_seed: vec![7; 32],
            evm_claim_address: vec![0xA5; 20],
            evm_gas_secret: vec![0x46; 32],
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
}

// --- with the live features: the full door -------------------------------------------------------

#[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
mod live {
    use std::sync::Arc;

    use super::live_impl::{
        assemble_initiator_mainnet, assemble_responder_mainnet, build_live_intent,
        mainnet_money_path, LockRecordingSigner, OneShotSigner,
    };
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
    use crate::swap_funding_verify::{ConfirmationPolicy, SwapCaps};
    use crate::swap_intent::Asset;
    use crate::swap_signer::SwapSigner;
    use crate::swap_wire::{SwapLegId, NIM_ADDRESS_LEN, SWAP_ID_LEN};

    const HTLC_HEX: &str = "0xb3B3703E07AC897B7E3e864C113a2Fa547D76736";
    const USDC_HEX: &str = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582";

    fn radio(id: &str) -> Arc<dyn BleRadio> {
        MockRadio::new(id, MockEther::new())
    }

    fn wallet_key() -> Arc<dyn EnclaveKey> {
        Arc::new(InMemoryEnclaveKey::from_secret(&[0x51; 32]))
    }

    fn init_cfg() -> FfiLiveInitiatorConfig {
        FfiLiveInitiatorConfig {
            nim_luna: 500_000,
            usdc_micro: 1_000_000,
            expiry_height: 1_000_000,
            intent_seed: vec![7; 32],
            evm_claim_address: vec![0xA5; 20],
            evm_gas_secret: vec![0x46; 32],
            nim_rpc_url: "https://rpc.testnet.nimiqwatch.com".into(),
            amoy_rpc_url: "https://rpc-amoy.polygon.technology".into(),
            htlc_address: HTLC_HEX.into(),
            delta_safe_blocks: 0,
            min_claim_window_blocks: 0,
        }
    }

    fn resp_cfg() -> FfiLiveResponderConfig {
        FfiLiveResponderConfig {
            usdc_micro: 1_000_000,
            nim_luna: 500_000,
            expiry_height: 1_000_000,
            nim_claim_seed: vec![0x52; 32],
            evm_funding_secret: vec![0x46; 32],
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

    // --- MAINNET assembly (armed with an injected HTLC address; the shipped const stays off) ------

    /// A stand-in deployed HTLC for the ARMED-assembly tests: any valid 20-byte 0x-hex. This is
    /// injected straight into `assemble_*_mainnet`, so it NEVER touches the shipped
    /// `MAINNET_HTLC_ADDRESS` const (still empty) or `MAINNET_SWAP_ENABLED` (still false).
    const MAINNET_HTLC_HEX: &str = "0xA7bB819Ba03743643249dFCCa7508280eCE059b1";
    /// A Nimiq mainnet RPC (no "testnet" fragment → `HttpGatewayRpc::new_mainnet` admits it).
    const MAINNET_NIM_RPC: &str = "https://rpc.nimiqwatch.com";
    /// An allow-listed Polygon mainnet RPC (`HttpPolygonRpc::new_mainnet` admits only these).
    const MAINNET_POLY_RPC: &str = "https://polygon.drpc.org";

    fn mainnet_init_cfg() -> FfiLiveInitiatorConfig {
        let mut c = init_cfg();
        c.nim_rpc_url = MAINNET_NIM_RPC.into();
        c.amoy_rpc_url = MAINNET_POLY_RPC.into();
        c.htlc_address = String::new(); // ignored on the mainnet path (the const wins)
        c
    }

    fn mainnet_resp_cfg() -> FfiLiveResponderConfig {
        let mut c = resp_cfg();
        c.nim_rpc_url = MAINNET_NIM_RPC.into();
        c.amoy_rpc_url = MAINNET_POLY_RPC.into();
        c.htlc_address = String::new();
        c.usdc_address = String::new(); // ignored — the native mainnet USDC const wins
        c
    }

    #[test]
    fn mainnet_money_path_selects_every_mainnet_component() {
        let m = mainnet_money_path(MAINNET_HTLC_HEX).expect("valid htlc resolves");
        assert_eq!(m.evm_chain_id, crate::evm_rlp::POLYGON_MAINNET_CHAIN_ID);
        assert_eq!(m.evm_chain_id, 137, "Polygon mainnet chain id");
        assert_eq!(m.network, crate::NetworkId::Mainnet);
        assert_eq!(m.confirm_policy, ConfirmationPolicy::mainnet_defaults());
        assert_eq!(m.caps, SwapCaps::mainnet_first_swap());
        // The escrow contract is the injected address; the token is the NATIVE Circle USDC.
        assert!(format!("0x{}", bytes_to_hex(&m.htlc)).eq_ignore_ascii_case(MAINNET_HTLC_HEX));
        assert!(format!("0x{}", bytes_to_hex(&m.usdc))
            .eq_ignore_ascii_case(crate::polygon_gateway::NATIVE_USDC_POLYGON_MAINNET));
    }

    #[test]
    fn armed_initiator_assembly_is_mainnet_and_caps_refuse_over_cap() {
        let (session, _signer, gateway, money) = assemble_initiator_mainnet(
            wallet_key(),
            LiveLockBook::new(),
            mainnet_init_cfg(),
            None,
            MAINNET_HTLC_HEX,
        )
        .expect("armed initiator assembles");
        // The intent is stamped MAINNET and gives NIM (the initiator role).
        let intent = session
            .identity
            .standing_intent
            .as_ref()
            .expect("standing intent");
        assert_eq!(intent.network_id, crate::NetworkId::Mainnet.wire_id());
        assert_eq!(intent.gives, Asset::Nim);
        assert!(gateway.is_none(), "no gateway requested");
        // C1 live-safety holds: chain-backed verifier + non-sim secret + non-zero mainnet depths.
        assert!(session.live_safety().is_ok());
        // The hard mainnet caps are wired into the session and REFUSE over-cap.
        assert_eq!(money.caps, SwapCaps::mainnet_first_swap());
        assert!(
            session.within_caps(50 * 100_000, 5 * 1_000_000),
            "exactly at cap is admitted"
        );
        assert!(
            !session.within_caps(51 * 100_000, 5 * 1_000_000),
            "over the NIM cap is refused"
        );
        assert!(
            !session.within_caps(50 * 100_000, 6 * 1_000_000),
            "over the USDC cap is refused"
        );
    }

    #[test]
    fn armed_responder_assembly_uses_native_usdc_and_mainnet() {
        let (session, _signer, gateway, money) =
            assemble_responder_mainnet(mainnet_resp_cfg(), None, MAINNET_HTLC_HEX)
                .expect("armed responder assembles");
        let intent = session
            .identity
            .standing_intent
            .as_ref()
            .expect("standing intent");
        assert_eq!(intent.network_id, crate::NetworkId::Mainnet.wire_id());
        assert_eq!(intent.gives, Asset::Usdc);
        assert!(gateway.is_none());
        assert!(session.live_safety().is_ok());
        // The responder escrows the NATIVE Polygon-mainnet USDC (never `config.usdc_address`).
        assert!(format!("0x{}", bytes_to_hex(&money.usdc))
            .eq_ignore_ascii_case(crate::polygon_gateway::NATIVE_USDC_POLYGON_MAINNET));
        assert!(
            !session.within_caps(50 * 100_000, 6 * 1_000_000),
            "over the USDC cap is refused"
        );
    }

    #[test]
    fn the_mainnet_ctors_gate_on_the_arm_flag() {
        // State-agnostic (the mainnet_swap.rs pattern): the mainnet FFI ctors REFUSE while the path
        // is unarmed and PROCEED once Andjroo's arming release opens the flag + records the escrow.
        let init = MeshNode::new_live_swap_initiator_mainnet(
            b"m".to_vec(),
            radio("m"),
            wallet_key(),
            LiveLockBook::new(),
            mainnet_init_cfg(),
            None,
        );
        let resp = MeshNode::new_live_swap_responder_mainnet(
            b"m2".to_vec(),
            radio("m2"),
            mainnet_resp_cfg(),
            None,
        );
        if mainnet_swap_armed() {
            // ARMED: the arm gate is open, so the ctors build a real mainnet-network node from the
            // (valid) mainnet RPC fixtures — no longer a refusal. The armed-assembly invariants are
            // proven by the dedicated `assemble_*_mainnet` tests above; here just confirm they no
            // longer refuse on the arm gate.
            assert!(init.is_ok(), "armed mainnet initiator constructs");
            assert!(resp.is_ok(), "armed mainnet responder constructs");
        } else {
            // UNARMED (pre-arming build): both ctors REFUSE outright (flag off / empty HTLC) — inert.
            assert!(
                matches!(init, Err(LiveSwapFfiError::Refused { .. })),
                "the unarmed mainnet initiator refuses"
            );
            assert!(
                matches!(resp, Err(LiveSwapFfiError::Refused { .. })),
                "the unarmed mainnet responder refuses"
            );
        }
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
