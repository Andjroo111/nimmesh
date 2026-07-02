//! # swap_sim — an in-memory sim chain + dual-engine stepper (demo, `bitcoin-leg` feature)
//!
//! Drives a full NIM⇄BTC atomic swap between a real **initiator** and **responder**
//! [`SwapEngine`], one engine action per [`SwapSim::step`], against a trivial in-memory simulated
//! chain (funding is instantly "confirmed"; the responder learns the secret by reading it off the
//! initiator's BTC claim — exactly as on a real chain). The engines build the **real** HTLC tx bytes
//! (the ones validated vs `@nimiq/core` + `bitcoinjs-lib` and live-confirmed); only the network is
//! simulated. This is what the browser demo server drives so the UI runs on the real engine.
//!
//! **Sim / testnet only. No real funds, no faucets, no mainnet.** Presentation/demo path.

use std::sync::Arc;

use bitcoin::{Address as BtcAddress, CompressedPublicKey, Network, Txid};

use crate::btc::{BtcEnclaveKey, FundedHtlc, InMemoryBtcEnclaveKey};
use crate::nimiq::address::Address as NimAddress;
use crate::nimiq::hex::bytes_to_hex;
use crate::nimiq::signer::{EnclaveKey, InMemoryEnclaveKey};
use crate::swap::{LadderParams, SwapPhase, SwapTerms};
use crate::swap_btc_leg::BtcSwapLeg;
use crate::swap_builder::NimiqLeg;
use crate::swap_engine::{EngineError, SwapConfig, SwapEffect, SwapEngine};
use crate::swap_funding_verify::FundingObservation;
use crate::swap_leg::sha256;
use crate::swap_wire::SwapLegId;

const HEAD_MS: u64 = 1_000_000_000_000;
const T_B_MS: u64 = HEAD_MS + 3_600_000; // BTC leg: head + 1 h
const T_A_MS: u64 = HEAD_MS + 7_200_000; // NIM leg: head + 2 h (longer)
const NIM_AMOUNT: u64 = 280_000_000; // 2 800 NIM (luna)
const BTC_AMOUNT: u64 = 120_000; // sat (~0.0012 BTC)
const BTC_FEE: u64 = 500;
const VSH: u32 = 100;
const TOTAL_STEPS: u8 = 5;

/// A snapshot of the swap, for the demo server / UI. All public, sim-only data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapSnapshot {
    /// The current step, `0` (not started) … `5` (settled).
    pub step: u8,
    /// Total steps (`5`).
    pub total: u8,
    /// Human label for the current step (e.g. `"Locking up NIM"`).
    pub label: String,
    /// The initiator engine's lifecycle phase (`Debug` of [`SwapPhase`]).
    pub initiator_phase: String,
    /// The responder engine's lifecycle phase.
    pub responder_phase: String,
    /// Whether the NIM HTLC is locked on the sim chain.
    pub nim_locked: bool,
    /// Whether the BTC HTLC is claimed/locked on the sim chain.
    pub btc_locked: bool,
    /// Whether the secret has been revealed (read off the BTC claim).
    pub secret_revealed: bool,
    /// Whether the swap stalled and both legs were reclaimed via the timeout refund (funds safe).
    pub refunded: bool,
    /// A short display id of the last broadcast tx, if any.
    pub last_tx_id: Option<String>,
    /// The BTC P2WSH HTLC address both sides derive.
    pub btc_htlc_address: String,
    /// Whether the swap has settled (terminal success).
    pub done: bool,
}

/// One node's keys are seeded deterministically so the demo is reproducible.
fn btc_key(seed: u8) -> Arc<dyn BtcEnclaveKey> {
    Arc::new(InMemoryBtcEnclaveKey::from_secret(&[seed; 32]).unwrap())
}
fn btc_pubkey(seed: u8) -> [u8; 33] {
    InMemoryBtcEnclaveKey::from_secret(&[seed; 32])
        .unwrap()
        .public_key()
        .try_into()
        .unwrap()
}
fn p2wpkh_spk(pk: &[u8; 33], net: Network) -> bitcoin::ScriptBuf {
    BtcAddress::p2wpkh(&CompressedPublicKey::from_slice(pk).unwrap(), net).script_pubkey()
}
fn nim_addr(seed: u8) -> NimAddress {
    let pk: [u8; 32] = InMemoryEnclaveKey::from_secret(&[seed; 32])
        .public_key()
        .try_into()
        .unwrap();
    NimAddress::from_public_key(&pk)
}

/// A short display id for a tx (sim only): `sha256(tx)[..4]` hex.
fn tx_id(tx: &[u8]) -> String {
    bytes_to_hex(&sha256(tx)[..4])
}

/// A full NIM⇄BTC atomic swap between two real engines, advanced step by step against a sim chain.
pub struct SwapSim {
    initiator: SwapEngine,
    responder: SwapEngine,
    ladder: LadderParams,
    btc_htlc_address: String,
    step: u8,
    // sim-chain state, filled as the swap progresses
    nim_funding: Option<Vec<u8>>,
    btc_funding: Option<FundedHtlc>,
    btc_claim: Option<Vec<u8>>,
    last_tx_id: Option<String>,
    secret_revealed: bool,
    refunded: bool,
}

impl SwapSim {
    /// A fresh swap at step 0. Keys/amounts are fixed demo values (sim/testnet only).
    pub fn new() -> Self {
        let net = Network::Testnet;
        let hashlock = sha256(&[0x42u8; 32]);
        let cltv = (T_B_MS / 1000) as i64;
        let claimant = btc_pubkey(0x11); // initiator claims BTC
        let funder = btc_pubkey(0x22); // responder funds/refunds BTC

        let btc_leg = |node_seed: u8| {
            BtcSwapLeg::new(
                hashlock,
                claimant,
                funder,
                cltv,
                net,
                btc_key(node_seed),
                p2wpkh_spk(&btc_pubkey(node_seed), net),
                BTC_FEE,
            )
        };
        let terms = SwapTerms {
            nim_timeout: T_A_MS,
            counterparty_timeout: T_B_MS,
        };

        let initiator = SwapEngine::new_initiator(
            SwapConfig {
                terms,
                nim: NimiqLeg::new(Arc::new(InMemoryEnclaveKey::from_secret(&[3u8; 32]))),
                btc: btc_leg(0x11),
                nim_amount: NIM_AMOUNT,
                btc_amount_sat: BTC_AMOUNT,
                counterparty_nim_address: nim_addr(4).as_bytes().to_vec(),
            },
            [0x42u8; 32],
        );
        let responder = SwapEngine::new_responder(SwapConfig {
            terms,
            nim: NimiqLeg::new(Arc::new(InMemoryEnclaveKey::from_secret(&[4u8; 32]))),
            btc: btc_leg(0x22),
            nim_amount: NIM_AMOUNT,
            btc_amount_sat: BTC_AMOUNT,
            counterparty_nim_address: Vec::new(),
        });
        let btc_htlc_address = initiator.btc_htlc_address();

        SwapSim {
            initiator,
            responder,
            ladder: LadderParams {
                delta_safe_blocks: 1_800_000,
                min_claim_window_blocks: 1_800_000,
            },
            btc_htlc_address,
            step: 0,
            nim_funding: None,
            btc_funding: None,
            btc_claim: None,
            last_tx_id: None,
            secret_revealed: false,
            refunded: false,
        }
    }

    /// Advance the swap one real engine action. No-op once settled.
    pub fn step(&mut self) -> Result<SwapSnapshot, EngineError> {
        match self.step {
            0 => {
                // Both accept (the Δ_safe ladder is checked here).
                self.initiator.accept(HEAD_MS, &self.ladder)?;
                self.responder.accept(HEAD_MS, &self.ladder)?;
            }
            1 => {
                // Initiator funds the NIM HTLC; the sim confirms it.
                let eff = self.initiator.fund(HEAD_MS, &self.ladder, VSH)?;
                let wire = expect_broadcast(eff, SwapLegId::Nim)?;
                self.last_tx_id = Some(tx_id(&wire));
                self.nim_funding = Some(wire);
            }
            2 => {
                // Responder observes the NIM funding, funds the BTC HTLC (pays the P2WSH); the sim
                // confirms it and both sides observe both legs funded.
                let wire = self.nim_funding.clone().ok_or(missing("nim funding"))?;
                // The sim chain confirms funding instantly and exactly as agreed, so the S1 gate sees a
                // healthy on-chain observation (agreed amount, agreed timeout, one confirmation).
                let nim_obs = FundingObservation::Found {
                    amount: NIM_AMOUNT,
                    timeout: T_A_MS,
                    confirmations: 1,
                };
                self.responder.observe_initiator_funded(wire, nim_obs, 1)?;
                let _addr = self.responder.fund(HEAD_MS, &self.ladder, VSH)?; // FundBtcAddress
                let funded = FundedHtlc {
                    txid: sim_txid(),
                    vout: 0,
                    value_sat: BTC_AMOUNT,
                };
                let btc_obs = FundingObservation::Found {
                    amount: BTC_AMOUNT,
                    timeout: T_B_MS,
                    confirmations: 1,
                };
                self.initiator.observe_btc_funded(funded, btc_obs, 1)?;
                self.responder.observe_nim_funded(funded)?;
                self.btc_funding = Some(funded);
            }
            3 => {
                // Initiator claims the BTC, revealing the secret on the sim chain.
                let eff = self.initiator.reveal_and_claim_btc()?;
                let claim = expect_broadcast(eff, SwapLegId::Counterparty)?;
                self.last_tx_id = Some(tx_id(&claim));
                self.secret_revealed = true;
                self.btc_claim = Some(claim);
            }
            4 => {
                // Responder reads the secret off the BTC claim and claims the NIM leg; both settle.
                let claim = self.btc_claim.clone().ok_or(missing("btc claim"))?;
                let eff = self.responder.claim_nim_from_btc_claim(&claim, VSH)?;
                let nim_claim = expect_broadcast(eff, SwapLegId::Nim)?;
                self.last_tx_id = Some(tx_id(&nim_claim));
                self.initiator.observe_settled()?;
                self.responder.observe_settled()?;
            }
            _ => return Ok(self.snapshot()), // already settled
        }
        self.step += 1;
        Ok(self.snapshot())
    }

    /// The safety net: from `BothFunded` (after step 3) the swap stalls, and **both sides reclaim
    /// their own funds via the timeout refund path** — the initiator its NIM (past `T_A`), the
    /// responder its BTC (past `T_B`). Worst case is a refund, never a loss. Errors if called before
    /// both legs are funded.
    pub fn stall_and_refund(&mut self) -> Result<SwapSnapshot, EngineError> {
        let nim_refund = self.initiator.refund(T_A_MS + 1, VSH)?;
        let _ = expect_broadcast(nim_refund, SwapLegId::Nim)?;
        let btc_refund = self.responder.refund(T_B_MS + 1, VSH)?;
        let tx = expect_broadcast(btc_refund, SwapLegId::Counterparty)?;
        self.last_tx_id = Some(tx_id(&tx));
        self.refunded = true;
        Ok(self.snapshot())
    }

    /// The current state without advancing.
    pub fn snapshot(&self) -> SwapSnapshot {
        let label = if self.refunded {
            "Refunded — funds safe"
        } else {
            match self.step {
                0 => "Ready",
                1 => "Proposing swap",
                2 => "Locking up NIM",
                3 => "Waiting for Bitcoin",
                4 => "Claiming Bitcoin",
                _ => "Settling",
            }
        };
        SwapSnapshot {
            step: self.step,
            total: TOTAL_STEPS,
            label: label.to_string(),
            initiator_phase: format!("{:?}", self.initiator.phase()),
            responder_phase: format!("{:?}", self.responder.phase()),
            nim_locked: self.step >= 3,
            btc_locked: self.step >= 5,
            secret_revealed: self.secret_revealed,
            refunded: self.refunded,
            last_tx_id: self.last_tx_id.clone(),
            btc_htlc_address: self.btc_htlc_address.clone(),
            done: self.initiator.phase() == SwapPhase::Settled
                && self.responder.phase() == SwapPhase::Settled,
        }
    }

    /// Restart the swap from step 0.
    pub fn reset(&mut self) {
        *self = SwapSim::new();
    }
}

impl Default for SwapSim {
    fn default() -> Self {
        SwapSim::new()
    }
}

fn missing(what: &'static str) -> EngineError {
    EngineError::MissingInput(what)
}

/// Unwrap a `Broadcast` effect on the expected leg into its tx bytes.
fn expect_broadcast(eff: SwapEffect, leg: SwapLegId) -> Result<Vec<u8>, EngineError> {
    match eff {
        SwapEffect::Broadcast { leg: l, tx } if l == leg => Ok(tx),
        _ => Err(EngineError::MissingInput("unexpected effect for this step")),
    }
}

/// A deterministic fake funding txid for the sim chain.
fn sim_txid() -> Txid {
    use bitcoin::hashes::Hash;
    Txid::from_byte_array([0xab; 32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_swap_drives_to_settled_through_real_engines() {
        let mut sim = SwapSim::new();
        assert_eq!(sim.snapshot().step, 0);
        assert!(sim.snapshot().btc_htlc_address.starts_with("tb1q"));

        // 1) Proposing swap → both Accepted
        let s = sim.step().unwrap();
        assert_eq!((s.step, s.label.as_str()), (1, "Proposing swap"));
        assert_eq!(s.initiator_phase, "Accepted");

        // 2) Locking up NIM → the initiator broadcast a real 248-byte NIM HTLC funding tx
        let s = sim.step().unwrap();
        assert_eq!(s.label, "Locking up NIM");
        assert!(s.last_tx_id.is_some());
        assert_eq!(s.initiator_phase, "SelfFunded");

        // 3) Waiting for Bitcoin → both legs funded
        let s = sim.step().unwrap();
        assert_eq!(s.label, "Waiting for Bitcoin");
        assert!(s.nim_locked);
        assert_eq!(s.initiator_phase, "BothFunded");

        // 4) Claiming Bitcoin → secret revealed on the BTC claim
        let s = sim.step().unwrap();
        assert_eq!(s.label, "Claiming Bitcoin");
        assert!(s.secret_revealed);
        assert_eq!(s.initiator_phase, "Revealed");

        // 5) Settling → both Settled
        let s = sim.step().unwrap();
        assert!(s.done);
        assert!(s.btc_locked);
        assert_eq!(s.initiator_phase, "Settled");
        assert_eq!(s.responder_phase, "Settled");

        // idempotent once settled
        let again = sim.step().unwrap();
        assert!(again.done);
        assert_eq!(again.step, 5);
    }

    #[test]
    fn reset_restarts_the_swap() {
        let mut sim = SwapSim::new();
        for _ in 0..5 {
            sim.step().unwrap();
        }
        assert!(sim.snapshot().done);
        sim.reset();
        assert_eq!(sim.snapshot().step, 0);
        assert!(!sim.snapshot().done);
    }

    #[test]
    fn stall_refunds_both_sides_through_the_real_engines() {
        let mut sim = SwapSim::new();
        sim.step().unwrap(); // accept
        sim.step().unwrap(); // lock NIM
        let s = sim.step().unwrap(); // both funded
        assert_eq!(s.initiator_phase, "BothFunded");

        // The swap stalls; both sides reclaim their own funds via the timeout refund. No loss.
        let r = sim.stall_and_refund().unwrap();
        assert!(r.refunded);
        assert!(!r.done); // not a successful settle — but funds are safe
        assert_eq!(r.label, "Refunded — funds safe");
        assert_eq!(r.initiator_phase, "Refunded");
        assert_eq!(r.responder_phase, "Refunded");
    }

    #[test]
    fn cannot_refund_before_both_legs_are_funded() {
        let mut sim = SwapSim::new();
        sim.step().unwrap(); // accept only — nothing locked yet
        assert!(sim.stall_and_refund().is_err());
    }
}
