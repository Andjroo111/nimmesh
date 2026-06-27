//! # btc_gateway — broadcast + confirmation for the Bitcoin leg (B2, `bitcoin-gateway` feature)
//!
//! The one online hop for the BTC leg: a thin client over a public **signet** indexer
//! (`mempool.space/signet/api`) — broadcast a raw tx, find an HTLC's funding output, and poll for
//! inclusion. The analog of the Nimiq [`crate::rpc`] gateway (blocking `ureq`, no async, no node).
//! A gateway node runs this; the on-device wallet only *builds* txs (`crate::btc`) and floods them
//! over the mesh. **Signet/testnet only — a mainnet indexer base is refused** (the no-mainnet
//! invariant, mirroring `rpc::guard_testnet`).

use std::fmt;

/// The default public signet indexer base (mempool.space).
pub const DEFAULT_SIGNET_API: &str = "https://mempool.space/signet/api";

/// An unspent output paying an address (e.g. an HTLC's P2WSH), as the indexer reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utxo {
    /// The funding transaction id (display/big-endian hex).
    pub txid: String,
    /// The output index.
    pub vout: u32,
    /// The value in satoshis.
    pub value: u64,
    /// Whether the funding tx is confirmed.
    pub confirmed: bool,
    /// The confirmation block height, if confirmed.
    pub block_height: Option<u32>,
}

/// A BTC gateway failure.
#[derive(Debug, Clone)]
pub enum BtcGatewayError {
    /// The base URL was not a signet/testnet indexer (mainnet refused).
    NotSignet {
        /// The offending base URL.
        url: String,
    },
    /// An HTTP error status.
    Http {
        /// The HTTP status code.
        status: u16,
        /// The endpoint that returned it.
        what: &'static str,
    },
    /// A transport / IO error.
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

impl fmt::Display for BtcGatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BtcGatewayError::NotSignet { url } => write!(f, "refused non-signet indexer: {url}"),
            BtcGatewayError::Http { status, what } => {
                write!(f, "btc indexer http {status} ({what})")
            }
            BtcGatewayError::Transport { reason } => write!(f, "btc indexer transport: {reason}"),
            BtcGatewayError::Parse { reason } => write!(f, "btc indexer parse: {reason}"),
        }
    }
}

impl std::error::Error for BtcGatewayError {}

/// A blocking client over a signet indexer. Guards that the base is **not** a mainnet endpoint.
#[derive(Debug, Clone)]
pub struct BtcSignetGateway {
    base: String,
}

impl BtcSignetGateway {
    /// Construct over `base` (default [`DEFAULT_SIGNET_API`]). Refuses a base that is not clearly a
    /// signet/testnet indexer — there is no path to a mainnet broadcast.
    pub fn new(base: &str) -> Result<Self, BtcGatewayError> {
        let b = base.trim_end_matches('/');
        let low = b.to_ascii_lowercase();
        if !(low.contains("signet") || low.contains("testnet")) {
            return Err(BtcGatewayError::NotSignet { url: b.to_string() });
        }
        Ok(BtcSignetGateway {
            base: b.to_string(),
        })
    }

    /// The default mempool.space signet gateway.
    pub fn signet() -> Self {
        BtcSignetGateway {
            base: DEFAULT_SIGNET_API.to_string(),
        }
    }

    fn get(&self, path: &str, what: &'static str) -> Result<String, BtcGatewayError> {
        let url = format!("{}{path}", self.base);
        match ureq::get(&url).call() {
            Ok(resp) => resp.into_string().map_err(|e| BtcGatewayError::Transport {
                reason: e.to_string(),
            }),
            Err(ureq::Error::Status(status, _)) => Err(BtcGatewayError::Http { status, what }),
            Err(e) => Err(BtcGatewayError::Transport {
                reason: e.to_string(),
            }),
        }
    }

    /// The current chain tip height.
    pub fn tip_height(&self) -> Result<u32, BtcGatewayError> {
        self.get("/blocks/tip/height", "tip")?
            .trim()
            .parse()
            .map_err(|_| BtcGatewayError::Parse {
                reason: "tip height not a number".into(),
            })
    }

    /// Broadcast a raw (hex) transaction. Returns the txid the indexer echoes.
    pub fn broadcast(&self, raw_hex: &str) -> Result<String, BtcGatewayError> {
        let url = format!("{}/tx", self.base);
        match ureq::post(&url).send_string(raw_hex) {
            Ok(resp) => Ok(resp
                .into_string()
                .map_err(|e| BtcGatewayError::Transport {
                    reason: e.to_string(),
                })?
                .trim()
                .to_string()),
            Err(ureq::Error::Status(status, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(BtcGatewayError::Parse {
                    reason: format!("broadcast rejected (http {status}): {}", body.trim()),
                })
            }
            Err(e) => Err(BtcGatewayError::Transport {
                reason: e.to_string(),
            }),
        }
    }

    /// The UTXOs paying `address` (e.g. an HTLC P2WSH) — used to find the funding outpoint + value.
    pub fn address_utxos(&self, address: &str) -> Result<Vec<Utxo>, BtcGatewayError> {
        let body = self.get(&format!("/address/{address}/utxo"), "utxo")?;
        parse_utxos(&body)
    }

    /// The confirmation height of `txid`, or `None` if still unconfirmed / unknown.
    pub fn tx_block_height(&self, txid: &str) -> Result<Option<u32>, BtcGatewayError> {
        let body = match self.get(&format!("/tx/{txid}"), "tx") {
            Ok(b) => b,
            Err(BtcGatewayError::Http { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| BtcGatewayError::Parse {
                reason: e.to_string(),
            })?;
        if v["status"]["confirmed"].as_bool() == Some(true) {
            Ok(v["status"]["block_height"].as_u64().map(|h| h as u32))
        } else {
            Ok(None)
        }
    }
}

/// Parse a mempool.space `address/:a/utxo` JSON array into [`Utxo`]s.
fn parse_utxos(body: &str) -> Result<Vec<Utxo>, BtcGatewayError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| BtcGatewayError::Parse {
        reason: e.to_string(),
    })?;
    let arr = v.as_array().ok_or(BtcGatewayError::Parse {
        reason: "utxo response is not an array".into(),
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for u in arr {
        out.push(Utxo {
            txid: u["txid"].as_str().unwrap_or_default().to_string(),
            vout: u["vout"].as_u64().unwrap_or_default() as u32,
            value: u["value"].as_u64().unwrap_or_default(),
            confirmed: u["status"]["confirmed"].as_bool().unwrap_or(false),
            block_height: u["status"]["block_height"].as_u64().map(|h| h as u32),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_mainnet_base() {
        assert!(matches!(
            BtcSignetGateway::new("https://mempool.space/api"),
            Err(BtcGatewayError::NotSignet { .. })
        ));
        assert!(BtcSignetGateway::new("https://mempool.space/signet/api").is_ok());
    }

    #[test]
    fn parses_a_utxo_response() {
        let body = r#"[
            {"txid":"aa11","vout":0,"value":100000,"status":{"confirmed":true,"block_height":310689}},
            {"txid":"bb22","vout":1,"value":5000,"status":{"confirmed":false}}
        ]"#;
        let utxos = parse_utxos(body).unwrap();
        assert_eq!(utxos.len(), 2);
        assert_eq!(utxos[0].txid, "aa11");
        assert_eq!(utxos[0].value, 100000);
        assert_eq!(utxos[0].block_height, Some(310689));
        assert!(!utxos[1].confirmed);
        assert_eq!(utxos[1].block_height, None);
    }
}
