//! # swap_engine — the cross-chain swap orchestrator (mesh swap, `bitcoin-leg` feature)
//!
//! One node's whole NIM⇄BTC atomic swap behind a single API. It folds what the
//! `live_cross_chain_swap` example did by hand into a first-class, pure (no-network) component the
//! UI and the mesh both drive: it owns the [`crate::swap::Swap`] state machine, holds this node's
//! [`crate::swap_builder::NimiqLeg`] + [`crate::swap_btc_leg::BtcSwapLeg`], and turns each
//! [`crate::swap::SwapAction`] into a concrete [`SwapEffect`] — signed tx bytes to broadcast, or a
//! BTC address to fund. The caller does the I/O (broadcast / watch) and feeds observations back.
//!
//! ## Units (the one cross-chain bridge)
//!
//! The engine works in Unix-**milliseconds** for the ladder + the NIM HTLC timeout (the NIM
//! validator compares the timeout to the block *time* in ms; the head-beacon's block time is the
//! mesh clock). The BTC leg's CLTV is a Unix-**seconds** timestamp = `T_B_ms / 1000`, baked into the
//! [`BtcSwapLeg`] by the caller at construction. So `SwapTerms` + [`crate::swap::LadderParams`] +
//! `head` are all in ms; the BTC leg already carries its seconds CLTV. The engine does no conversion
//! — it just wires the proven pieces together.
//!
//! ## Atomicity stays real
//!
//! The responder does not get told the secret — it **reads `S` off the initiator's on-chain BTC
//! claim** ([`crate::btc::extract_preimage`]) and verifies `SHA-256(S) == H` before claiming the NIM
//! leg. Every funded phase keeps a timeout [`refund`](SwapEngine::refund) exit. Testnet only; mainnet
//! gated.

use crate::btc::{extract_preimage, BtcError, FundedHtlc, HASH_LEN};
use crate::swap::{LadderParams, Swap, SwapAction, SwapError, SwapPhase, SwapRole, SwapTerms};
use crate::swap_btc_leg::BtcSwapLeg;
use crate::swap_builder::{FundingTerms, LegBuildError, LegBuilder, NimiqLeg};
use crate::swap_funding_verify::{
    require_funded, FundingObservation, FundingRejected, HtlcExpectation,
};
use crate::swap_leg::sha256;
use crate::swap_wire::SwapLegId;

/// The non-role configuration both sides share to build an engine.
pub struct SwapConfig {
    /// `T_A` (NIM) / `T_B` (counterparty) timeouts in Unix-**ms**, for the ladder gate.
    pub terms: SwapTerms,
    /// This node's NIM leg (its own NIM key behind the `EnclaveKey` seam).
    pub nim: NimiqLeg,
    /// This node's BTC leg (its own BTC key + both pubkeys; CLTV = `T_B_ms / 1000`).
    pub btc: BtcSwapLeg,
    /// Luna locked on the NIM leg (the initiator funds this).
    pub nim_amount: u64,
    /// Sat locked on the BTC leg (the responder funds this).
    pub btc_amount_sat: u64,
    /// The 20-byte raw NIM address that claims the NIM leg (the **responder's** NIM address). Used by
    /// the initiator when it builds the NIM HTLC funding; unused by the responder.
    pub counterparty_nim_address: Vec<u8>,
}

/// What the caller must do on-chain after a successful engine transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapEffect {
    /// Broadcast these signed tx bytes on the named leg's chain.
    Broadcast {
        /// Which chain to broadcast on.
        leg: SwapLegId,
        /// The signed, broadcast-safe tx bytes.
        tx: Vec<u8>,
    },
    /// Pay `amount_sat` to this BTC P2WSH HTLC address from the wallet (the responder's BTC funding —
    /// not a leg-built tx, since it spends the wallet's own UTXOs).
    FundBtcAddress {
        /// The P2WSH HTLC address to pay.
        address: String,
        /// The amount to lock, in satoshis.
        amount_sat: u64,
    },
}

/// A failure driving the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// The state machine rejected the transition (illegal phase / unsafe ladder / early refund).
    Swap(SwapError),
    /// The NIM leg failed to build a tx.
    NimLeg(LegBuildError),
    /// The BTC leg failed to build a tx.
    BtcLeg(BtcError),
    /// An observation needed for this step was never recorded.
    MissingInput(&'static str),
    /// A claimed preimage did not hash to the swap's hashlock — never trust it.
    BadPreimage,
    /// The counterparty's on-chain HTLC funding could not be verified against the agreed terms, so we
    /// refuse to fund our own leg (or reveal `S`). Carries the reason (S1 / #72).
    FundingUnverified(FundingRejected),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Swap(e) => write!(f, "swap state machine: {e}"),
            EngineError::NimLeg(e) => write!(f, "nim leg: {e}"),
            EngineError::BtcLeg(e) => write!(f, "btc leg: {e}"),
            EngineError::MissingInput(what) => write!(f, "missing input: {what}"),
            EngineError::BadPreimage => write!(f, "revealed preimage does not match the hashlock"),
            EngineError::FundingUnverified(r) => {
                write!(f, "counterparty funding unverified: {r:?}")
            }
        }
    }
}

impl std::error::Error for EngineError {}

/// One node's cross-chain swap. Construct with [`new_initiator`](SwapEngine::new_initiator) or
/// [`new_responder`](SwapEngine::new_responder), then drive it through accept → fund → claim/refund.
pub struct SwapEngine {
    swap: Swap,
    nim: NimiqLeg,
    btc: BtcSwapLeg,
    nim_amount: u64,
    btc_amount_sat: u64,
    counterparty_nim_address: Vec<u8>,
    /// The secret: the initiator knows it up front; the responder learns it from the BTC claim.
    secret: Option<[u8; HASH_LEN]>,
    /// The NIM HTLC funding tx — the initiator's own (built on fund), the responder's observed.
    nim_funding_wire: Option<Vec<u8>>,
    /// The BTC HTLC funding output — the responder's own (it pays it), the initiator's observed.
    btc_funding: Option<FundedHtlc>,
}

impl SwapEngine {
    /// The **initiator** (gives NIM, wants BTC, knows the secret). Funds first.
    pub fn new_initiator(config: SwapConfig, secret: [u8; HASH_LEN]) -> Self {
        Self::from_config(config, SwapRole::Initiator, Some(secret))
    }

    /// The **responder** (gives BTC, wants NIM, learns the secret from the BTC claim). Funds only
    /// after observing the initiator's funding.
    pub fn new_responder(config: SwapConfig) -> Self {
        Self::from_config(config, SwapRole::Responder, None)
    }

    fn from_config(config: SwapConfig, role: SwapRole, secret: Option<[u8; HASH_LEN]>) -> Self {
        SwapEngine {
            swap: Swap::new(role, config.terms),
            nim: config.nim,
            btc: config.btc,
            nim_amount: config.nim_amount,
            btc_amount_sat: config.btc_amount_sat,
            counterparty_nim_address: config.counterparty_nim_address,
            secret,
            nim_funding_wire: None,
            btc_funding: None,
        }
    }

    /// The current lifecycle phase.
    pub fn phase(&self) -> SwapPhase {
        self.swap.phase
    }

    /// Which side this node plays.
    pub fn role(&self) -> SwapRole {
        self.swap.role
    }

    /// The shared hashlock `H` locking both legs.
    pub fn hashlock(&self) -> [u8; HASH_LEN] {
        self.btc.params().hash_root
    }

    /// The BTC P2WSH HTLC address (to fund / watch) — both sides derive the same one.
    pub fn btc_htlc_address(&self) -> String {
        self.btc.htlc_address().to_string()
    }

    /// Move `Proposed → Accepted`, refusing if the timelock ladder is unsafe at `head` (ms).
    pub fn accept(&mut self, head: u64, params: &LadderParams) -> Result<(), EngineError> {
        self.swap.accept(head, params).map_err(EngineError::Swap)
    }

    /// (Responder) record the observed initiator NIM-HTLC funding tx → `InitiatorFunded`, but ONLY
    /// after verifying it on-chain (S1 / #72): `observed` is what the caller's gateway saw for the NIM
    /// HTLC (amount / timeout / depth); it must lock at least `nim_amount`, keep at least the agreed
    /// `T_A`, and be buried `min_confirmations` deep — otherwise we refuse to advance and so never fund
    /// BTC against an initiator that only *claims* to have funded. The wire is kept to build the claim.
    pub fn observe_initiator_funded(
        &mut self,
        nim_funding_wire: Vec<u8>,
        observed: FundingObservation,
        min_confirmations: u32,
    ) -> Result<(), EngineError> {
        let expect = HtlcExpectation {
            leg: SwapLegId::Nim,
            hashlock: self.hashlock(),
            min_amount: self.nim_amount,
            min_timeout: self.swap.terms.nim_timeout,
            recipient: self.counterparty_nim_address.clone(),
        };
        require_funded(&observed, &expect, min_confirmations)
            .map_err(EngineError::FundingUnverified)?;
        self.swap
            .observe_initiator_funded()
            .map_err(EngineError::Swap)?;
        self.nim_funding_wire = Some(nim_funding_wire);
        Ok(())
    }

    /// Fund this node's own leg, re-checking the ladder. `vsh` anchors the NIM funding tx (ignored
    /// for BTC). Initiator → a NIM HTLC funding tx to broadcast; responder → the BTC P2WSH to pay.
    pub fn fund(
        &mut self,
        head: u64,
        params: &LadderParams,
        vsh: u32,
    ) -> Result<SwapEffect, EngineError> {
        let action = self.swap.fund(head, params).map_err(EngineError::Swap)?;
        let SwapAction::FundLeg { leg, timeout } = action else {
            unreachable!("Swap::fund always yields FundLeg");
        };
        match leg {
            SwapLegId::Nim => {
                let terms = FundingTerms {
                    hashlock: self.hashlock(),
                    amount: self.nim_amount,
                    timeout, // T_A in ms — the NIM validator wants a ms timestamp
                    recipient: self.counterparty_nim_address.clone(),
                    validity_start_height: vsh,
                };
                let wire = self
                    .nim
                    .build_funding(&terms)
                    .map_err(EngineError::NimLeg)?;
                self.nim_funding_wire = Some(wire.clone());
                Ok(SwapEffect::Broadcast {
                    leg: SwapLegId::Nim,
                    tx: wire,
                })
            }
            SwapLegId::Counterparty => Ok(SwapEffect::FundBtcAddress {
                address: self.btc.htlc_address().to_string(),
                amount_sat: self.btc_amount_sat,
            }),
        }
    }

    /// (Initiator) record the observed BTC HTLC funding output → `BothFunded`, but ONLY after verifying
    /// it on-chain (S1 / #72): `observed` is the caller's gateway report; it must lock at least
    /// `btc_amount_sat` and be `min_confirmations` deep — otherwise we refuse to advance and so never
    /// reveal `S` against a BTC HTLC that is not really there. (The P2WSH address encodes the hashlock +
    /// CLTV, so locating funds at it already fixes those; here we enforce amount + depth.)
    pub fn observe_btc_funded(
        &mut self,
        funded: FundedHtlc,
        observed: FundingObservation,
        min_confirmations: u32,
    ) -> Result<(), EngineError> {
        let expect = HtlcExpectation {
            leg: SwapLegId::Counterparty,
            hashlock: self.hashlock(),
            min_amount: self.btc_amount_sat,
            min_timeout: self.swap.terms.counterparty_timeout,
            recipient: Vec::new(),
        };
        require_funded(&observed, &expect, min_confirmations)
            .map_err(EngineError::FundingUnverified)?;
        self.swap
            .observe_counterparty_funded()
            .map_err(EngineError::Swap)?;
        self.btc_funding = Some(funded);
        Ok(())
    }

    /// (Responder) confirm both legs are funded → `BothFunded`, recording this node's **own** BTC
    /// funding output (the wallet learns the outpoint after it pays the P2WSH). The outpoint is kept
    /// so the responder can build a timeout refund of its BTC if the swap stalls.
    pub fn observe_nim_funded(&mut self, own_btc_funding: FundedHtlc) -> Result<(), EngineError> {
        self.swap
            .observe_counterparty_funded()
            .map_err(EngineError::Swap)?;
        self.btc_funding = Some(own_btc_funding);
        Ok(())
    }

    /// (Initiator) claim the BTC leg by **revealing the secret** → a broadcastable BTC claim tx whose
    /// witness puts `S` on-chain (that is how the responder learns it). → `Revealed`. Gated on the
    /// reveal deadline at `head` (ms) via the ladder `params` (G4 / #75): if the counterparty leg's
    /// `T_B` is within the claim window it refuses and does NOT reveal `S`, leaving the refund path.
    pub fn reveal_and_claim_btc(
        &mut self,
        head: u64,
        params: &LadderParams,
    ) -> Result<SwapEffect, EngineError> {
        self.swap
            .reveal_and_claim(head, params)
            .map_err(EngineError::Swap)?;
        let secret = self
            .secret
            .ok_or(EngineError::MissingInput("initiator has no secret"))?;
        let funded = self
            .btc_funding
            .ok_or(EngineError::MissingInput("BTC funding not observed"))?;
        let tx = self
            .btc
            .build_claim(&funded, &secret)
            .map_err(EngineError::BtcLeg)?;
        Ok(SwapEffect::Broadcast {
            leg: SwapLegId::Counterparty,
            tx,
        })
    }

    /// (Responder) learn `S` from the initiator's broadcast BTC claim and claim the NIM leg with it.
    /// Verifies `SHA-256(S) == H` first. `vsh` anchors the NIM claim tx. → `Revealed`.
    pub fn claim_nim_from_btc_claim(
        &mut self,
        btc_claim_tx: &[u8],
        vsh: u32,
    ) -> Result<SwapEffect, EngineError> {
        let secret = extract_preimage(btc_claim_tx)
            .ok_or(EngineError::MissingInput("no preimage in the BTC claim"))?;
        if sha256(&secret) != self.hashlock() {
            return Err(EngineError::BadPreimage);
        }
        self.swap.observe_secret().map_err(EngineError::Swap)?;
        self.secret = Some(secret);
        let funding = self
            .nim_funding_wire
            .as_ref()
            .ok_or(EngineError::MissingInput("NIM funding wire not observed"))?;
        let tx = self
            .nim
            .build_claim(funding, secret, vsh)
            .map_err(EngineError::NimLeg)?;
        Ok(SwapEffect::Broadcast {
            leg: SwapLegId::Nim,
            tx,
        })
    }

    /// Record that this node's claim settled on-chain → `Settled` (terminal success).
    pub fn observe_settled(&mut self) -> Result<(), EngineError> {
        self.swap.observe_settled().map_err(EngineError::Swap)
    }

    /// Cancel before this node has funded → `Aborted` (illegal once funds are locked).
    pub fn abort(&mut self) -> Result<(), EngineError> {
        self.swap.abort().map_err(EngineError::Swap)
    }

    /// Refund this node's own funded leg via the timeout path — the always-available safety exit
    /// once past the leg's timeout (`head` in ms). `vsh` anchors a NIM refund (ignored for BTC).
    pub fn refund(&mut self, head: u64, vsh: u32) -> Result<SwapEffect, EngineError> {
        let action = self
            .swap
            .refund_after_timeout(head)
            .map_err(EngineError::Swap)?;
        let SwapAction::RefundLeg { leg } = action else {
            unreachable!("Swap::refund_after_timeout always yields RefundLeg");
        };
        match leg {
            SwapLegId::Nim => {
                let funding = self
                    .nim_funding_wire
                    .as_ref()
                    .ok_or(EngineError::MissingInput("no NIM funding to refund"))?;
                let tx = self
                    .nim
                    .build_refund(funding, vsh)
                    .map_err(EngineError::NimLeg)?;
                Ok(SwapEffect::Broadcast {
                    leg: SwapLegId::Nim,
                    tx,
                })
            }
            SwapLegId::Counterparty => {
                let funded = self
                    .btc_funding
                    .ok_or(EngineError::MissingInput("no BTC funding to refund"))?;
                let tx = self
                    .btc
                    .build_refund(&funded)
                    .map_err(EngineError::BtcLeg)?;
                Ok(SwapEffect::Broadcast {
                    leg: SwapLegId::Counterparty,
                    tx,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btc::{BtcEnclaveKey, InMemoryBtcEnclaveKey};
    use crate::nimiq::address::Address as NimAddress;
    use crate::nimiq::signer::{EnclaveKey, InMemoryEnclaveKey};
    use crate::swap::{LadderVerdict, SwapTerms};
    use bitcoin::{Address as BtcAddress, CompressedPublicKey, Network, ScriptBuf, Txid};
    use std::str::FromStr;
    use std::sync::Arc;

    const HEAD_MS: u64 = 1_000_000_000_000; // a Unix-ms head
    const T_B_MS: u64 = HEAD_MS + 3_600_000; // responder leg: head + 1 h
    const T_A_MS: u64 = HEAD_MS + 7_200_000; // initiator leg: head + 2 h (longer)
    const NIM_AMOUNT: u64 = 200_000;
    const BTC_AMOUNT: u64 = 100_000;
    const BTC_FEE: u64 = 500;
    const VSH: u32 = 100;

    /// A healthy on-chain observation (locks the exact agreed amount, ample timeout, deeply buried) —
    /// the honest happy path for the S1 funding-verification gate.
    fn observed(amount: u64) -> FundingObservation {
        FundingObservation::Found {
            amount,
            timeout: u64::MAX,
            confirmations: 100,
        }
    }

    fn ladder() -> LadderParams {
        // Margin T_A−T_B = 3_600_000 ms; window T_B−head = 3_600_000 ms. Require 30 min of each (ms).
        LadderParams {
            delta_safe_blocks: 1_800_000,
            min_claim_window_blocks: 1_800_000,
        }
    }

    fn terms() -> SwapTerms {
        SwapTerms {
            nim_timeout: T_A_MS,
            counterparty_timeout: T_B_MS,
        }
    }

    fn btc_pk(seed: u8) -> [u8; 33] {
        InMemoryBtcEnclaveKey::from_secret(&[seed; 32])
            .unwrap()
            .public_key()
            .try_into()
            .unwrap()
    }

    fn p2wpkh_spk(pk33: &[u8; 33], net: Network) -> ScriptBuf {
        BtcAddress::p2wpkh(&CompressedPublicKey::from_slice(pk33).unwrap(), net).script_pubkey()
    }

    fn nim_addr(seed: u8) -> NimAddress {
        let pk: [u8; 32] = InMemoryEnclaveKey::from_secret(&[seed; 32])
            .public_key()
            .try_into()
            .unwrap();
        NimAddress::from_public_key(&pk)
    }

    // Keys: initiator NIM = sender(seed 3), responder NIM = recipient(seed 4); initiator BTC =
    // claimant(seed 0x11), responder BTC = funder(seed 0x22).
    fn btc_leg(node_seed: u8, payout_seed: u8) -> BtcSwapLeg {
        let net = Network::Signet;
        BtcSwapLeg::new(
            sha256(&[0x42u8; 32]),  // H = SHA-256(secret)
            btc_pk(0x11),           // claimant pubkey (initiator)
            btc_pk(0x22),           // funder pubkey (responder)
            (T_B_MS / 1000) as i64, // CLTV = T_B in seconds
            net,
            Arc::new(InMemoryBtcEnclaveKey::from_secret(&[node_seed; 32]).unwrap()),
            p2wpkh_spk(&btc_pk(payout_seed), net),
            BTC_FEE,
        )
    }

    fn initiator() -> SwapEngine {
        SwapEngine::new_initiator(
            SwapConfig {
                terms: terms(),
                nim: NimiqLeg::new(Arc::new(InMemoryEnclaveKey::from_secret(&[3u8; 32]))),
                btc: btc_leg(0x11, 0x11), // initiator signs claims; payout to itself
                nim_amount: NIM_AMOUNT,
                btc_amount_sat: BTC_AMOUNT,
                counterparty_nim_address: nim_addr(4).as_bytes().to_vec(), // responder claims NIM
            },
            [0x42u8; 32],
        )
    }

    fn responder() -> SwapEngine {
        SwapEngine::new_responder(SwapConfig {
            terms: terms(),
            nim: NimiqLeg::new(Arc::new(InMemoryEnclaveKey::from_secret(&[4u8; 32]))),
            btc: btc_leg(0x22, 0x22), // responder signs refunds; payout to itself
            nim_amount: NIM_AMOUNT,
            btc_amount_sat: BTC_AMOUNT,
            counterparty_nim_address: vec![], // unused by the responder
        })
    }

    fn synth_btc_funding(addr_matches: &str) -> FundedHtlc {
        // The responder pays the P2WSH from its wallet; here we synthesize the resulting outpoint.
        assert!(addr_matches.starts_with("tb1q"));
        FundedHtlc {
            txid: Txid::from_str(&"ab".repeat(32)).unwrap(),
            vout: 0,
            value_sat: BTC_AMOUNT,
        }
    }

    #[test]
    fn a_responder_engine_refuses_to_fund_an_unverified_nim_htlc() {
        // S1 / #72 at the engine layer: the responder will not advance (and so never funds BTC) unless
        // the observed NIM funding meets the agreed amount + required depth.
        let p = ladder();
        let mut resp = responder();
        resp.accept(HEAD_MS, &p).unwrap();

        // Underfunded (1 luna vs the agreed NIM_AMOUNT) → refused, stays at Accepted.
        let underfunded = FundingObservation::Found {
            amount: 1,
            timeout: u64::MAX,
            confirmations: 100,
        };
        assert!(matches!(
            resp.observe_initiator_funded(vec![0u8; 248], underfunded, 1),
            Err(EngineError::FundingUnverified(_))
        ));
        assert_eq!(resp.phase(), SwapPhase::Accepted);

        // Correct amount but too shallow → refused (reorg safety).
        let shallow = FundingObservation::Found {
            amount: NIM_AMOUNT,
            timeout: u64::MAX,
            confirmations: 1,
        };
        assert!(matches!(
            resp.observe_initiator_funded(vec![0u8; 248], shallow, 6),
            Err(EngineError::FundingUnverified(_))
        ));
        assert_eq!(resp.phase(), SwapPhase::Accepted);

        // Healthy + deep → advances (and only now would it fund BTC).
        resp.observe_initiator_funded(vec![0u8; 248], observed(NIM_AMOUNT), 1)
            .unwrap();
        assert_eq!(resp.phase(), SwapPhase::InitiatorFunded);
    }

    #[test]
    fn full_cross_chain_swap_drives_both_engines_to_settled() {
        let p = ladder();
        let mut init = initiator();
        let mut resp = responder();

        // Both accept (ladder safe at head).
        init.accept(HEAD_MS, &p).unwrap();
        resp.accept(HEAD_MS, &p).unwrap();

        // 1) Initiator funds the NIM HTLC.
        let nim_funding = match init.fund(HEAD_MS, &p, VSH).unwrap() {
            SwapEffect::Broadcast {
                leg: SwapLegId::Nim,
                tx,
            } => {
                assert_eq!(tx.len(), 248); // the signed HTLC funding wire @nimiq/core accepts
                tx
            }
            other => panic!("expected NIM funding broadcast, got {other:?}"),
        };

        // 2) Responder observes it, then funds the BTC HTLC by paying the P2WSH.
        resp.observe_initiator_funded(nim_funding, observed(NIM_AMOUNT), 1)
            .unwrap();
        let btc_addr = match resp.fund(HEAD_MS, &p, VSH).unwrap() {
            SwapEffect::FundBtcAddress {
                address,
                amount_sat,
            } => {
                assert_eq!(amount_sat, BTC_AMOUNT);
                // Both sides derive the SAME P2WSH from the shared public params.
                assert_eq!(address, init.btc_htlc_address());
                address
            }
            other => panic!("expected a BTC funding address, got {other:?}"),
        };
        let funded = synth_btc_funding(&btc_addr);

        // 3) Both observe the second leg funded (same single BTC outpoint).
        init.observe_btc_funded(funded, observed(BTC_AMOUNT), 1)
            .unwrap();
        resp.observe_nim_funded(funded).unwrap();
        assert_eq!(init.phase(), SwapPhase::BothFunded);
        assert_eq!(resp.phase(), SwapPhase::BothFunded);

        // 4) Initiator claims BTC, revealing S on-chain (well within the reveal deadline).
        let btc_claim = match init.reveal_and_claim_btc(HEAD_MS, &p).unwrap() {
            SwapEffect::Broadcast {
                leg: SwapLegId::Counterparty,
                tx,
            } => tx,
            other => panic!("expected a BTC claim broadcast, got {other:?}"),
        };
        assert_eq!(init.phase(), SwapPhase::Revealed);
        // The secret is now public in the BTC claim witness — exactly what the responder reads.
        assert_eq!(extract_preimage(&btc_claim), Some([0x42u8; 32]));

        // 5) Responder reads S off the BTC claim and claims the NIM leg with it.
        let nim_claim = match resp.claim_nim_from_btc_claim(&btc_claim, VSH).unwrap() {
            SwapEffect::Broadcast {
                leg: SwapLegId::Nim,
                tx,
            } => tx,
            other => panic!("expected a NIM claim broadcast, got {other:?}"),
        };
        assert_eq!(resp.phase(), SwapPhase::Revealed);
        assert_eq!(nim_claim[0], 0x01); // Extended format
        assert!(nim_claim.windows(3).any(|w| w == [0xA6, 0x01, 0x00])); // RegularTransfer proof frame

        // 6) Both settle.
        init.observe_settled().unwrap();
        resp.observe_settled().unwrap();
        assert_eq!(init.phase(), SwapPhase::Settled);
        assert_eq!(resp.phase(), SwapPhase::Settled);
    }

    #[test]
    fn a_forged_preimage_is_rejected_at_the_nim_claim() {
        let p = ladder();
        let mut init = initiator();
        let mut resp = responder();
        init.accept(HEAD_MS, &p).unwrap();
        resp.accept(HEAD_MS, &p).unwrap();
        let nim_funding = match init.fund(HEAD_MS, &p, VSH).unwrap() {
            SwapEffect::Broadcast { tx, .. } => tx,
            other => panic!("got {other:?}"),
        };
        resp.observe_initiator_funded(nim_funding, observed(NIM_AMOUNT), 1)
            .unwrap();
        let addr = match resp.fund(HEAD_MS, &p, VSH).unwrap() {
            SwapEffect::FundBtcAddress { address, .. } => address,
            other => panic!("got {other:?}"),
        };
        resp.observe_nim_funded(synth_btc_funding(&addr)).unwrap();
        // A BTC "claim" carrying the wrong preimage (a different valid HTLC claim) must not advance
        // the responder: extract works but SHA-256(S') != H.
        let bogus_leg = BtcSwapLeg::new(
            sha256(&[0x99u8; 32]), // a DIFFERENT hashlock
            btc_pk(0x11),
            btc_pk(0x22),
            (T_B_MS / 1000) as i64,
            Network::Signet,
            Arc::new(InMemoryBtcEnclaveKey::from_secret(&[0x11u8; 32]).unwrap()),
            p2wpkh_spk(&btc_pk(0x11), Network::Signet),
            BTC_FEE,
        );
        let bogus_claim = bogus_leg
            .build_claim(
                &FundedHtlc {
                    txid: Txid::from_str(&"cd".repeat(32)).unwrap(),
                    vout: 0,
                    value_sat: BTC_AMOUNT,
                },
                &[0x99u8; 32],
            )
            .unwrap();
        assert_eq!(
            resp.claim_nim_from_btc_claim(&bogus_claim, VSH),
            Err(EngineError::BadPreimage)
        );
        // Phase unchanged — no NIM claim built on a secret that doesn't open our hashlock.
        assert_eq!(resp.phase(), SwapPhase::BothFunded);
    }

    #[test]
    fn both_legs_keep_a_timeout_refund_exit() {
        let p = ladder();
        let mut init = initiator();
        let mut resp = responder();
        init.accept(HEAD_MS, &p).unwrap();
        resp.accept(HEAD_MS, &p).unwrap();
        let nim_funding = match init.fund(HEAD_MS, &p, VSH).unwrap() {
            SwapEffect::Broadcast { tx, .. } => tx,
            other => panic!("got {other:?}"),
        };
        resp.observe_initiator_funded(nim_funding, observed(NIM_AMOUNT), 1)
            .unwrap();
        let addr = match resp.fund(HEAD_MS, &p, VSH).unwrap() {
            SwapEffect::FundBtcAddress { address, .. } => address,
            other => panic!("got {other:?}"),
        };
        let funded = synth_btc_funding(&addr);
        init.observe_btc_funded(funded, observed(BTC_AMOUNT), 1)
            .unwrap();
        resp.observe_nim_funded(funded).unwrap();

        // Initiator refunds its NIM leg after T_A; the tx is a TimeoutResolve.
        let nim_refund = match init.refund(T_A_MS + 1, VSH).unwrap() {
            SwapEffect::Broadcast {
                leg: SwapLegId::Nim,
                tx,
            } => tx,
            other => panic!("got {other:?}"),
        };
        assert_eq!(nim_refund[nim_refund.len() - 99], 0x02); // TimeoutResolve before the 98-B sig proof
        assert_eq!(init.phase(), SwapPhase::Refunded);

        // Responder refunds its BTC leg after T_B; it reveals no preimage.
        let btc_refund = match resp.refund(T_B_MS + 1, VSH).unwrap() {
            SwapEffect::Broadcast {
                leg: SwapLegId::Counterparty,
                tx,
            } => tx,
            other => panic!("got {other:?}"),
        };
        assert_eq!(extract_preimage(&btc_refund), None);
        assert_eq!(resp.phase(), SwapPhase::Refunded);
    }

    #[test]
    fn accept_refuses_an_unsafe_ladder() {
        // Inverted ladder: T_A <= T_B.
        let mut bad = SwapEngine::new_initiator(
            SwapConfig {
                terms: SwapTerms {
                    nim_timeout: T_B_MS,
                    counterparty_timeout: T_A_MS,
                },
                nim: NimiqLeg::new(Arc::new(InMemoryEnclaveKey::from_secret(&[3u8; 32]))),
                btc: btc_leg(0x11, 0x11),
                nim_amount: NIM_AMOUNT,
                btc_amount_sat: BTC_AMOUNT,
                counterparty_nim_address: nim_addr(4).as_bytes().to_vec(),
            },
            [0x42u8; 32],
        );
        assert_eq!(
            bad.accept(HEAD_MS, &ladder()),
            Err(EngineError::Swap(SwapError::UnsafeLadder(
                LadderVerdict::Inverted
            )))
        );
        assert_eq!(bad.phase(), SwapPhase::Proposed);
    }
}
