//! # btc_verifier — the mempool.space-backed BTC-leg [`FundingVerifier`] (#72 tail, the BTC leg)
//!
//! The real-chain funding gate for the **Bitcoin leg**: before this node advances a swap against
//! the counterparty's BTC HTLC, [`BtcHtlcVerifier`] confirms a P2WSH HTLC funding output with the
//! agreed script + amount is really on-chain at depth — the BTC sibling of
//! [`crate::nim_verifier::NimHtlcVerifier`] and [`crate::polygon_verifier::PolygonHtlcVerifier`],
//! against the same [`crate::swap_funding_verify::require_funded`] gate.
//!
//! ## How it locates + binds the funding (address-derived, no hint needed)
//!
//! A BTC HTLC lives at a **script-derived** P2WSH address
//! (`OP_0 SHA256(redeemScript)`, [`crate::btc::BtcHtlcParams::p2wsh_address`]) that BOTH sides
//! derive from the public terms (hashlock + both pubkeys + CLTV) — so the funding output is
//! located by that agreed address, and everything that matters is re-established from chain truth:
//!
//! 1. the watched **scriptPubKey is recomputed locally** from the agreed terms; a funding output
//!    is an output whose scriptPubKey is EXACTLY that P2WSH (never the indexer's address grouping —
//!    a P2WSH output at our address that is NOT our exact script reads `Mismatch`), holding at
//!    least the agreed amount;
//! 2. a later tx **spending** that outpoint means the HTLC is already resolved (claimed/refunded) —
//!    a resolved slot is not funding, so it reads `Absent` (mirroring the NIM/Polygon verifiers);
//! 3. depth = `tip − funding-block + 1`, feeding [`require_funded`]'s per-chain
//!    [`ConfirmationPolicy`](crate::swap_funding_verify::ConfirmationPolicy) floor — a reorg that
//!    re-buries it shallower is refused again on the next observation (the gate is stateless).
//!
//! **Fail-closed:** any transport / parse error, an unseen tx, or an unconfirmed funding reads as
//! shallow/`Absent` — the gate then refuses (`NotFundedYet`/`TooShallow`) and retries later. A
//! transport blip can delay a swap; it can never authorize one.
//!
//! ## M5 cross-read (ADR-0011)
//!
//! Given an optional independent second reads source ([`BtcHtlcVerifier::with_secondary`],
//! e.g. mempool.space + blockstream.info), a depth is trusted only when the secondary **agrees on
//! the funding tx's block height**, and its tip folds into a **conservative (min) depth** — so a
//! single lying/MITM'd indexer can no longer fake "funded + deep". Disagreement / error → `Absent`.
//!
//! ## Timeout semantics (mirrors `polygon_verifier`)
//!
//! `observe` reports the agreed CLTV as the raw on-chain timeout (Unix seconds); it does NOT apply
//! the ADR-0010 term mapping, so — exactly like [`crate::polygon_verifier::PolygonHtlcVerifier`] —
//! [`FundingVerifier::chain_backed`] stays the default `false` here. The BTC leg has no live-signer
//! wiring yet (it is #72-tail, gated); a live BTC swap would add the term-mapped variant. Nothing
//! constructs this on a live path until the Andjroo-gated guard-lift.

use crate::swap_funding_verify::{
    FundingObservation, FundingVerifier, HtlcExpectation, MismatchReason,
};
use crate::swap_wire::SwapLegId;

/// An error from the [`BitcoinReads`] seam. Every variant reads fail-closed (`Absent`) at the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtcReadsError {
    /// A base URL was not a recognized indexer host (the read-side guard).
    BadBase {
        /// The offending base.
        url: String,
    },
    /// A transport / IO error talking to the indexer.
    Transport {
        /// A short description.
        reason: String,
    },
    /// The response body could not be parsed as expected.
    Parse {
        /// A short description.
        reason: String,
    },
}

impl std::fmt::Display for BtcReadsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtcReadsError::BadBase { url } => write!(f, "unrecognized btc indexer base: {url}"),
            BtcReadsError::Transport { reason } => write!(f, "btc reads transport: {reason}"),
            BtcReadsError::Parse { reason } => write!(f, "btc reads parse: {reason}"),
        }
    }
}

impl std::error::Error for BtcReadsError {}

/// A prevout an address tx spends (its vin's `(txid, vout)`) — used to detect a claim/refund.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtcOutpoint {
    /// The prevout transaction id (display hex).
    pub txid: String,
    /// The prevout index.
    pub vout: u32,
}

/// One output of an address tx: its raw scriptPubKey bytes + value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtcTxOut {
    /// The scriptPubKey bytes (for a P2WSH HTLC: `0x0020 || SHA256(redeem)`, 34 bytes).
    pub scriptpubkey: Vec<u8>,
    /// The output value in satoshis.
    pub value_sat: u64,
}

/// One transaction related to a queried address (mempool.space/esplora `/address/:a/txs`): its id,
/// the outpoints it spends (its vins), and its outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtcAddressTx {
    /// The transaction id (display hex).
    pub txid: String,
    /// The outpoints this tx spends (its vins' prevouts).
    pub spends: Vec<BtcOutpoint>,
    /// This tx's outputs.
    pub outputs: Vec<BtcTxOut>,
}

/// A transaction's confirmation status (esplora `/tx/:txid/status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtcTxStatus {
    /// Whether the tx is in a block.
    pub confirmed: bool,
    /// The confirmation block height, if confirmed.
    pub block_height: Option<u64>,
}

/// The chain reads the BTC verifier needs — a seam so the logic tests OFFLINE against a fake. The
/// live implementation is [`HttpBitcoinReads`] (behind `bitcoin-gateway`). The three endpoints map
/// to esplora's `/address/:a/txs`, `/tx/:txid/status`, and `/blocks/tip/height`.
pub trait BitcoinReads: Send + Sync {
    /// Every tx paying/spending `address` (confirmed + mempool).
    fn address_txs(&self, address: &str) -> Result<Vec<BtcAddressTx>, BtcReadsError>;
    /// The confirmation status of `txid`.
    fn tx_status(&self, txid: &str) -> Result<BtcTxStatus, BtcReadsError>;
    /// The current best-chain tip height.
    fn tip_height(&self) -> Result<u64, BtcReadsError>;
}

/// Whether `spk` is a P2WSH scriptPubKey (`OP_0 <32-byte program>`): 34 bytes, `0x00 0x20` prefix.
/// Used to tell a *wrong-terms* HTLC-shaped output (→ `Mismatch`) from ordinary change outputs.
fn is_p2wsh(spk: &[u8]) -> bool {
    spk.len() == 34 && spk[0] == 0x00 && spk[1] == 0x20
}

/// The gateway-backed BTC-leg funding verifier. Construct with the chain reads (live:
/// [`HttpBitcoinReads`]) and the agreed HTLC's watched scriptPubKey + address + hashlock + CLTV —
/// or, with the `bitcoin-leg` feature, derive them from [`crate::btc::BtcHtlcParams`] via
/// [`BtcHtlcVerifier::from_params`].
pub struct BtcHtlcVerifier<R: BitcoinReads> {
    reads: R,
    /// M5: an OPTIONAL independent second reads source. When set, a reported depth is only trusted
    /// when this endpoint AGREES on the funding tx's block height, and its tip folds into a
    /// conservative (min) depth. When `None`, today's single-source trust assumption holds
    /// (documented in ADR-0011).
    secondary: Option<R>,
    /// The watched P2WSH scriptPubKey, recomputed from the agreed terms (bind: an on-chain output
    /// must equal these bytes EXACTLY to count as funding).
    expected_spk: Vec<u8>,
    /// The watched P2WSH address (what the reads seam is queried by).
    expected_addr: String,
    /// The agreed hashlock — a consistency guard: an expectation for a different hashlock is not
    /// this verifier's swap.
    hashlock: [u8; 32],
    /// The agreed CLTV locktime (Unix seconds) — the on-chain timeout the P2WSH commits to.
    cltv_locktime: u64,
}

impl<R: BitcoinReads> BtcHtlcVerifier<R> {
    /// A verifier over `reads` watching the P2WSH `expected_addr` whose scriptPubKey is
    /// `expected_spk`, for the HTLC committing to `hashlock` with CLTV `cltv_locktime`. Pure —
    /// unit-testable without the `bitcoin` crate (the derivation lives in [`Self::from_params`]).
    pub fn new(
        reads: R,
        expected_spk: Vec<u8>,
        expected_addr: String,
        hashlock: [u8; 32],
        cltv_locktime: u64,
    ) -> Self {
        BtcHtlcVerifier {
            reads,
            secondary: None,
            expected_spk,
            expected_addr,
            hashlock,
            cltv_locktime,
        }
    }

    /// M5: add an INDEPENDENT secondary reads source (a second indexer, e.g. blockstream.info). A
    /// reported depth is then only trusted when this endpoint agrees on the funding tx's block
    /// height; disagreement / error reads `Absent` (fail-closed).
    pub fn with_secondary(mut self, secondary: R) -> Self {
        self.secondary = Some(secondary);
        self
    }

    fn observe_btc(&self, expect: &HtlcExpectation) -> FundingObservation {
        // This verifier watches ONE HTLC; an expectation for a different hashlock is not its swap.
        if self.hashlock != expect.hashlock {
            return FundingObservation::Absent;
        }
        let txs = match self.reads.address_txs(&self.expected_addr) {
            Ok(t) => t,
            Err(_) => return FundingObservation::Absent, // fail-closed on transport
        };

        // Find the funding output: an output whose scriptPubKey is EXACTLY our recomputed P2WSH
        // (never trust the indexer's address grouping). Its amount is reported as-is and judged by
        // `require_funded` (an underfunded HTLC is `Underfunded`, not silently invisible — the
        // polygon_verifier discipline). A P2WSH output at our address that is NOT our exact script
        // is a wrong-terms HTLC → Mismatch; ordinary change outputs (P2WPKH/P2TR) are ignored.
        let mut funding: Option<(String, u32, u64)> = None; // (txid, vout, value)
        let mut wrong_script = false;
        for tx in &txs {
            for (idx, out) in tx.outputs.iter().enumerate() {
                if out.scriptpubkey == self.expected_spk {
                    if funding.is_none() {
                        funding = Some((tx.txid.clone(), idx as u32, out.value_sat));
                    }
                } else if is_p2wsh(&out.scriptpubkey) {
                    wrong_script = true;
                }
            }
        }
        let (txid, vout, value) = match funding {
            Some(f) => f,
            // No funding to our exact script: a wrong-terms P2WSH under our address is a Mismatch;
            // otherwise nothing is on-chain for us yet.
            None if wrong_script => return FundingObservation::Mismatch(MismatchReason::Recipient),
            None => return FundingObservation::Absent,
        };

        // Resolved ≠ funding: a later tx spending our funding outpoint (a claim or refund) means the
        // HTLC is already emptied — reads `Absent`, mirroring the NIM/Polygon verifiers.
        let spent = txs
            .iter()
            .any(|tx| tx.spends.iter().any(|o| o.txid == txid && o.vout == vout));
        if spent {
            return FundingObservation::Absent;
        }

        // Depth from the funding tx's confirmation + the tip. An unconfirmed funding (still in the
        // mempool) reads 0 confirmations — the gate then refuses it as too shallow.
        let block = match self.reads.tx_status(&txid) {
            Ok(BtcTxStatus {
                confirmed: true,
                block_height: Some(h),
            }) => h,
            Ok(_) => {
                return FundingObservation::Found {
                    amount: value,
                    timeout: self.cltv_locktime,
                    confirmations: 0,
                }
            }
            Err(_) => return FundingObservation::Absent,
        };
        let mut tip = match self.reads.tip_height() {
            Ok(t) => t,
            Err(_) => return FundingObservation::Absent,
        };

        // M5 cross-read: an independent source must agree on the funding block AND its tip folds
        // into the conservative (min) depth — a single endpoint can neither fake inclusion nor
        // inflate the tip to fake depth. Disagreement / error → fail-closed.
        if let Some(sec) = &self.secondary {
            match sec.tx_status(&txid) {
                Ok(BtcTxStatus {
                    confirmed: true,
                    block_height: Some(h),
                }) if h == block => {}
                _ => return FundingObservation::Absent,
            }
            match sec.tip_height() {
                Ok(st) => tip = tip.min(st),
                Err(_) => return FundingObservation::Absent,
            }
        }

        let depth = tip.saturating_sub(block).saturating_add(1);
        FundingObservation::Found {
            amount: value,
            timeout: self.cltv_locktime,
            confirmations: u32::try_from(depth).unwrap_or(u32::MAX),
        }
    }
}

impl<R: BitcoinReads> FundingVerifier for BtcHtlcVerifier<R> {
    fn observe(&self, expect: &HtlcExpectation) -> FundingObservation {
        // This verifier speaks only the counterparty (BTC) leg; the NIM leg has its own gateway.
        if expect.leg != SwapLegId::Counterparty {
            return FundingObservation::Absent;
        }
        self.observe_btc(expect)
    }

    // C1 note: `chain_backed` stays the DEFAULT `false` — the reported `timeout` is the RAW on-chain
    // CLTV seconds (no ADR-0010 term mapping), exactly like `polygon_verifier`, so it is not yet
    // live-signer-eligible; a live BTC swap would add the mapped variant. Testnet-inert.
}

// --- the bitcoin-leg derivation door (needs the `bitcoin` crate for the P2WSH derivation) --------

#[cfg(feature = "bitcoin-leg")]
impl<R: BitcoinReads> BtcHtlcVerifier<R> {
    /// Derive the watched scriptPubKey + address from the agreed [`crate::btc::BtcHtlcParams`] on
    /// `network` (the same P2WSH both sides derive from the public terms). The `bitcoin`-dependent
    /// half; the verification logic ([`Self::observe_btc`]) stays pure + offline-testable.
    pub fn from_params(
        reads: R,
        params: &crate::btc::BtcHtlcParams,
        network: bitcoin::Network,
    ) -> Self {
        let spk = params.script_pubkey(network).as_bytes().to_vec();
        let addr = params.p2wsh_address(network).to_string();
        BtcHtlcVerifier::new(
            reads,
            spk,
            addr,
            params.hash_root,
            params.cltv_locktime.max(0) as u64,
        )
    }
}

// --- the live HTTP reads seam (behind `bitcoin-gateway`; mempool.space / blockstream.info) --------

#[cfg(feature = "bitcoin-gateway")]
pub use http_reads::HttpBitcoinReads;

#[cfg(feature = "bitcoin-gateway")]
mod http_reads {
    use super::{BitcoinReads, BtcAddressTx, BtcOutpoint, BtcReadsError, BtcTxOut, BtcTxStatus};

    /// The recognized esplora indexer hosts (the read-side allowlist, mirroring
    /// [`crate::btc_gateway::BtcSignetGateway`]'s base guard). Both provide the identical esplora
    /// JSON schema and serve BTC mainnet + testnet/signet — the two INDEPENDENT sources the M5
    /// cross-read pairs. Reads are value-neutral (this verifier moves nothing), so — unlike the
    /// broadcast gateway — a mainnet base is allowed; the verifier itself stays testnet-inert until
    /// the Andjroo-gated guard-lift wires it onto a live path.
    const ALLOWED_HOSTS: [&str; 2] = ["mempool.space", "blockstream.info"];

    /// A blocking esplora reads client over a base like `https://mempool.space/testnet/api` or
    /// `https://blockstream.info/api`. Guards that the base is a recognized esplora host.
    #[derive(Debug, Clone)]
    pub struct HttpBitcoinReads {
        base: String,
    }

    impl HttpBitcoinReads {
        /// Construct over `base` (e.g. `https://mempool.space/testnet/api`). Refuses a base whose
        /// host is not a recognized esplora provider (the read-side guard).
        pub fn new(base: &str) -> Result<Self, BtcReadsError> {
            let b = base.trim_end_matches('/');
            let low = b.to_ascii_lowercase();
            let ok = low.starts_with("https://") && ALLOWED_HOSTS.iter().any(|h| low.contains(h));
            if !ok {
                return Err(BtcReadsError::BadBase { url: b.to_string() });
            }
            Ok(HttpBitcoinReads {
                base: b.to_string(),
            })
        }

        /// mempool.space for `network` ("mainnet" → `/api`, "testnet" → `/testnet/api`,
        /// "signet" → `/signet/api`).
        pub fn mempool_space(network: &str) -> Result<Self, BtcReadsError> {
            let suffix = match network {
                "mainnet" => "",
                "testnet" => "/testnet",
                "signet" => "/signet",
                other => return Err(BtcReadsError::BadBase { url: other.into() }),
            };
            Self::new(&format!("https://mempool.space{suffix}/api"))
        }

        /// blockstream.info for `network` (mainnet `/api`, testnet `/testnet/api` — no signet).
        pub fn blockstream(network: &str) -> Result<Self, BtcReadsError> {
            let suffix = match network {
                "mainnet" => "",
                "testnet" => "/testnet",
                other => return Err(BtcReadsError::BadBase { url: other.into() }),
            };
            Self::new(&format!("https://blockstream.info{suffix}/api"))
        }

        fn get(&self, path: &str) -> Result<String, BtcReadsError> {
            match ureq::get(&format!("{}{path}", self.base)).call() {
                Ok(resp) => resp.into_string().map_err(|e| BtcReadsError::Transport {
                    reason: e.to_string(),
                }),
                Err(ureq::Error::Status(status, _)) => Err(BtcReadsError::Transport {
                    reason: format!("http {status}"),
                }),
                Err(e) => Err(BtcReadsError::Transport {
                    reason: e.to_string(),
                }),
            }
        }
    }

    fn parse_status(v: &serde_json::Value) -> BtcTxStatus {
        BtcTxStatus {
            confirmed: v["confirmed"].as_bool().unwrap_or(false),
            block_height: v["block_height"].as_u64(),
        }
    }

    impl BitcoinReads for HttpBitcoinReads {
        fn address_txs(&self, address: &str) -> Result<Vec<BtcAddressTx>, BtcReadsError> {
            let body = self.get(&format!("/address/{address}/txs"))?;
            let v: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| BtcReadsError::Parse {
                    reason: e.to_string(),
                })?;
            let arr = v.as_array().ok_or(BtcReadsError::Parse {
                reason: "address txs not an array".into(),
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for tx in arr {
                let spends = tx["vin"]
                    .as_array()
                    .map(|vins| {
                        vins.iter()
                            .filter_map(|vin| {
                                Some(BtcOutpoint {
                                    txid: vin["txid"].as_str()?.to_string(),
                                    vout: vin["vout"].as_u64()? as u32,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let outputs = tx["vout"]
                    .as_array()
                    .map(|vouts| {
                        vouts
                            .iter()
                            .filter_map(|o| {
                                Some(BtcTxOut {
                                    scriptpubkey: crate::nimiq::hex::hex_to_bytes(
                                        o["scriptpubkey"].as_str()?,
                                    )
                                    .ok()?,
                                    value_sat: o["value"].as_u64()?,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(BtcAddressTx {
                    txid: tx["txid"].as_str().unwrap_or_default().to_string(),
                    spends,
                    outputs,
                });
            }
            Ok(out)
        }

        fn tx_status(&self, txid: &str) -> Result<BtcTxStatus, BtcReadsError> {
            let body = self.get(&format!("/tx/{txid}/status"))?;
            let v: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| BtcReadsError::Parse {
                    reason: e.to_string(),
                })?;
            Ok(parse_status(&v))
        }

        fn tip_height(&self) -> Result<u64, BtcReadsError> {
            self.get("/blocks/tip/height")?
                .trim()
                .parse()
                .map_err(|_| BtcReadsError::Parse {
                    reason: "tip height not a number".into(),
                })
        }
    }
}

#[cfg(test)]
#[path = "btc_verifier_tests.rs"]
mod tests;
