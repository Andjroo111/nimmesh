//! # rpc — the blocking Albatross JSON-RPC client seam (G8, gateway broadcast)
//!
//! The one online hop. A [`GatewayRpc`] is the minimal slice of the Nimiq Albatross
//! JSON-RPC a [`crate::gateway::RpcGateway`] needs to take a signed-tx blob off the mesh
//! and put it on chain: read the head height (to check the validity window), broadcast a
//! raw transaction, and poll for inclusion. It mirrors the proven fleet clients
//! (`sendhome/src/chain/nimiq-rpc.ts`, `nimiq.sale/src/payments/nimiq-rpc-client.ts`):
//! plain JSON-RPC 2.0 over HTTP POST, result unwrapped from `{ result: { data } }`,
//! **never** the `@nimiq/core` consensus light-client (which never establishes consensus
//! under Bun/Node and is for offline crypto only).
//!
//! Two impls:
//! - [`MockRpc`] — a deterministic, in-memory fake (always compiled). Every gateway +
//!   wiring test runs against it, so **`cargo test` is hermetic and offline** — no network
//!   ever touches the unit-test path.
//! - [`HttpGatewayRpc`] — the real blocking HTTP client (`ureq`), gated behind the
//!   **`gateway-rpc`** cargo feature so the pure-protocol core stays dependency-light and
//!   WASM-friendly. It is what the live-broadcast example binary drives.
//!
//! ## Testnet-only guard (money-path safety)
//!
//! Every constructor runs [`guard_testnet`]: the [`crate::NetworkId`] **must** be
//! [`crate::NetworkId::Testnet`] (`networkId = 5`) and the URL must not look like a known
//! **mainnet** RPC host. `rpc.nimiqwatch.com` is MAINNET and is refused; the loop only
//! ever talks to testnet (`rpc.testnet.nimiqwatch.com`). There is no path here that can be
//! pointed at mainnet without tripping the guard (LOOP.md / RISKS.md core value #7).

use std::fmt;

use crate::NetworkId;

/// The default public Albatross **testnet** JSON-RPC endpoint.
pub const DEFAULT_TESTNET_RPC_URL: &str = "https://rpc.testnet.nimiqwatch.com";

/// Known **mainnet** RPC host fragments the [`guard_testnet`] refuses outright. These are
/// real-money endpoints — the autonomous loop must never reach them (core value #7).
pub const MAINNET_RPC_HOSTS: &[&str] = &["rpc.nimiqwatch.com"];

/// An Albatross account as the gateway reads it (balance + type), mirroring the fleet's
/// `RpcAccount`. Only public state — never key material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcAccount {
    /// Balance in luna (1 NIM = 100_000 luna).
    pub balance: u64,
    /// Account type as the node reports it (`"basic"`, `"vesting"`, …).
    pub account_type: String,
    /// The address the node echoed back, if any.
    pub address: Option<String>,
}

/// A transaction as the gateway reads it back when polling for inclusion. `block_number`
/// is `Some` once the tx has been included in a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcTransaction {
    /// The transaction hash (lowercase hex).
    pub hash: String,
    /// The block the tx was included in, or `None` while still pending in the mempool.
    pub block_number: Option<u32>,
    /// Confirmation depth as the node reports it, if known.
    pub confirmations: Option<u32>,
}

/// A failure talking to the RPC node. The transient/terminal split mirrors the fleet:
/// a [`RpcError::Transport`] (network down, 5xx, 429) is worth retrying; a
/// [`RpcError::Rpc`] (the node rejected the request — bad tx, unknown method) is terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The constructor's testnet guard tripped (non-testnet network or a mainnet URL).
    NotTestnet {
        /// Why the URL/network was refused.
        reason: String,
    },
    /// A non-2xx HTTP status (transient if `>= 500` or `429`).
    Http {
        /// The HTTP status code.
        status: u16,
        /// The JSON-RPC method that produced it.
        method: String,
    },
    /// A transport-level failure (DNS, connection, TLS, body decode) — transient.
    Transport(String),
    /// The node returned a JSON-RPC `error` object — terminal (the request was rejected).
    Rpc {
        /// The JSON-RPC method that was rejected.
        method: String,
        /// The node's error message.
        message: String,
    },
    /// The node's response was missing or the wrong shape for the method.
    BadResponse {
        /// The JSON-RPC method whose response could not be parsed.
        method: String,
    },
}

impl RpcError {
    /// Whether retrying might succeed (network blip / overload), as opposed to a terminal
    /// rejection of the request itself.
    pub fn is_transient(&self) -> bool {
        match self {
            RpcError::Transport(_) => true,
            RpcError::Http { status, .. } => *status >= 500 || *status == 429,
            _ => false,
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::NotTestnet { reason } => write!(f, "refused non-testnet RPC: {reason}"),
            RpcError::Http { status, method } => write!(f, "rpc http {status} ({method})"),
            RpcError::Transport(e) => write!(f, "rpc transport error: {e}"),
            RpcError::Rpc { method, message } => write!(f, "{method}: {message}"),
            RpcError::BadResponse { method } => write!(f, "{method}: malformed response"),
        }
    }
}

impl std::error::Error for RpcError {}

/// Refuse any RPC target that is not Nimiq **testnet**. The network must be
/// [`NetworkId::Testnet`] (`networkId = 5`) and the URL must not contain a known mainnet
/// host fragment. This is the single choke point that keeps the money path on testnet.
pub fn guard_testnet(url: &str, network: NetworkId) -> Result<(), RpcError> {
    if network != NetworkId::Testnet {
        return Err(RpcError::NotTestnet {
            reason: format!(
                "network {network:?} (wire id {}) is not testnet",
                network.wire_id()
            ),
        });
    }
    let lower = url.to_ascii_lowercase();
    for host in MAINNET_RPC_HOSTS {
        if lower.contains(host) {
            return Err(RpcError::NotTestnet {
                reason: format!("URL '{url}' matches a known mainnet RPC host '{host}'"),
            });
        }
    }
    Ok(())
}

/// The blocking JSON-RPC slice the gateway broadcast path needs. Kept as a trait so the
/// engine/gateway depend only on this seam: tests inject [`MockRpc`] (offline,
/// deterministic) and the live example injects [`HttpGatewayRpc`].
pub trait GatewayRpc: Send + Sync {
    /// Current head block height — the input to the validity-window check (RISKS.md #1).
    fn block_number(&self) -> Result<u32, RpcError>;
    /// Public account state for an address; `None` if the account does not exist yet.
    fn get_account(&self, address: &str) -> Result<Option<RpcAccount>, RpcError>;
    /// Broadcast a client-signed raw transaction (lowercase hex) and return its hash.
    fn send_raw_transaction(&self, raw_hex: &str) -> Result<String, RpcError>;
    /// Look a transaction up by hash; `None` until the node knows it.
    fn get_transaction(&self, hash: &str) -> Result<Option<RpcTransaction>, RpcError>;
}

// --- MockRpc: the deterministic, always-compiled, offline fake -----------------------

use std::collections::HashMap;
use std::sync::Mutex;

use crate::nimiq::hex::bytes_to_hex;
use crate::transport::mock_tx_id;

/// A deterministic in-memory [`GatewayRpc`] for tests + dev. **No network** — every unit
/// test that exercises the gateway broadcast path runs against this, keeping `cargo test`
/// hermetic. Configurable head height + accept/reject so the validity-window and
/// failure paths are all reachable offline.
pub struct MockRpc {
    head: Mutex<u32>,
    /// When `Some`, `send_raw_transaction` returns this terminal RPC rejection.
    reject: Mutex<Option<String>>,
    /// When set, every call returns this transient transport error (models a flaky node).
    transient: Mutex<Option<String>>,
    /// Raw-hex blobs broadcast so far (assert the gateway broadcast exactly the bytes).
    sent: Mutex<Vec<String>>,
    /// Known transactions by hash (inclusion poll).
    txs: Mutex<HashMap<String, RpcTransaction>>,
}

impl MockRpc {
    /// A mock node at `head` height that accepts every broadcast.
    pub fn new(head: u32) -> Self {
        MockRpc {
            head: Mutex::new(head),
            reject: Mutex::new(None),
            transient: Mutex::new(None),
            sent: Mutex::new(Vec::new()),
            txs: Mutex::new(HashMap::new()),
        }
    }

    /// Set the head height the node reports (drives the validity-window check).
    pub fn set_head(&self, head: u32) {
        *self.head.lock().unwrap() = head;
    }

    /// Make future broadcasts fail with a terminal RPC rejection (e.g. "Invalid transaction").
    pub fn reject_with(&self, message: impl Into<String>) {
        *self.reject.lock().unwrap() = Some(message.into());
    }

    /// Make every call fail with a transient transport error (models a flaky/offline node).
    pub fn fail_transient(&self, message: impl Into<String>) {
        *self.transient.lock().unwrap() = Some(message.into());
    }

    /// Clear an injected transient failure.
    pub fn recover(&self) {
        *self.transient.lock().unwrap() = None;
    }

    /// The raw-hex blobs broadcast so far, in order.
    pub fn broadcasts(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    /// Mark a previously-broadcast tx as included at `block` (so `get_transaction` returns it).
    pub fn confirm(&self, hash: &str, block: u32) {
        let mut txs = self.txs.lock().unwrap();
        txs.insert(
            hash.to_string(),
            RpcTransaction {
                hash: hash.to_string(),
                block_number: Some(block),
                confirmations: Some(1),
            },
        );
    }

    /// The deterministic hash this mock assigns a broadcast blob (a stand-in for the real
    /// Blake2b tx hash, derived from the raw bytes so it is stable across calls).
    pub fn hash_of(raw_hex: &str) -> String {
        bytes_to_hex(&mock_tx_id(raw_hex.as_bytes()).0)
    }

    fn transient_guard(&self) -> Result<(), RpcError> {
        if let Some(msg) = self.transient.lock().unwrap().clone() {
            return Err(RpcError::Transport(msg));
        }
        Ok(())
    }
}

impl GatewayRpc for MockRpc {
    fn block_number(&self) -> Result<u32, RpcError> {
        self.transient_guard()?;
        Ok(*self.head.lock().unwrap())
    }

    fn get_account(&self, address: &str) -> Result<Option<RpcAccount>, RpcError> {
        self.transient_guard()?;
        Ok(Some(RpcAccount {
            balance: 0,
            account_type: "basic".to_string(),
            address: Some(address.to_string()),
        }))
    }

    fn send_raw_transaction(&self, raw_hex: &str) -> Result<String, RpcError> {
        self.transient_guard()?;
        if let Some(msg) = self.reject.lock().unwrap().clone() {
            return Err(RpcError::Rpc {
                method: "sendRawTransaction".to_string(),
                message: msg,
            });
        }
        self.sent.lock().unwrap().push(raw_hex.to_string());
        let hash = Self::hash_of(raw_hex);
        // Record it as known-but-pending so a subsequent get_transaction sees it.
        self.txs
            .lock()
            .unwrap()
            .entry(hash.clone())
            .or_insert_with(|| RpcTransaction {
                hash: hash.clone(),
                block_number: None,
                confirmations: Some(0),
            });
        Ok(hash)
    }

    fn get_transaction(&self, hash: &str) -> Result<Option<RpcTransaction>, RpcError> {
        self.transient_guard()?;
        Ok(self.txs.lock().unwrap().get(hash).cloned())
    }
}

// --- HttpGatewayRpc: the real blocking HTTP client (feature `gateway-rpc`) ------------

#[cfg(feature = "gateway-rpc")]
mod http {
    use std::time::Duration;

    use super::{
        guard_testnet, GatewayRpc, RpcAccount, RpcError, RpcTransaction, DEFAULT_TESTNET_RPC_URL,
    };
    use crate::NetworkId;

    /// The live blocking Albatross JSON-RPC client over `ureq` (no tokio, no async). Gated
    /// behind `gateway-rpc` so it never bloats the pure-protocol core. **Testnet-guarded
    /// at construction** — there is no way to build one pointed at mainnet.
    pub struct HttpGatewayRpc {
        url: String,
        agent: ureq::Agent,
    }

    impl HttpGatewayRpc {
        /// Build a client for `url` on `network`. Errors via [`guard_testnet`] unless the
        /// network is testnet and the URL is not a known mainnet host.
        pub fn new(url: impl Into<String>, network: NetworkId) -> Result<Self, RpcError> {
            let url = url.into();
            guard_testnet(&url, network)?;
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build();
            Ok(HttpGatewayRpc { url, agent })
        }

        /// A client against the default public testnet endpoint ([`DEFAULT_TESTNET_RPC_URL`]).
        pub fn testnet_default() -> Self {
            // The default URL + Testnet always pass the guard.
            Self::new(DEFAULT_TESTNET_RPC_URL, NetworkId::Testnet)
                .expect("default testnet endpoint passes the testnet guard")
        }

        /// The endpoint this client talks to.
        pub fn url(&self) -> &str {
            &self.url
        }

        /// One JSON-RPC 2.0 call. Unwraps the Albatross `{ result: { data } }` envelope and
        /// maps the HTTP/transport/RPC-error split onto [`RpcError`].
        fn call(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, RpcError> {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
                "id": 1,
            });
            let resp = self.agent.post(&self.url).send_json(body);
            let value: serde_json::Value = match resp {
                Ok(r) => r
                    .into_json()
                    .map_err(|e| RpcError::Transport(e.to_string()))?,
                Err(ureq::Error::Status(status, _)) => {
                    return Err(RpcError::Http {
                        status,
                        method: method.to_string(),
                    })
                }
                Err(ureq::Error::Transport(t)) => return Err(RpcError::Transport(t.to_string())),
            };
            if let Some(err) = value.get("error") {
                if !err.is_null() {
                    return Err(RpcError::Rpc {
                        method: method.to_string(),
                        message: err.to_string(),
                    });
                }
            }
            // Albatross wraps the payload under result.data; tolerate a bare result too.
            let result = value.get("result");
            Ok(result
                .and_then(|r| r.get("data"))
                .or(result)
                .cloned()
                .unwrap_or(serde_json::Value::Null))
        }
    }

    impl GatewayRpc for HttpGatewayRpc {
        fn block_number(&self) -> Result<u32, RpcError> {
            let v = self.call("getBlockNumber", serde_json::json!([]))?;
            v.as_u64().map(|n| n as u32).ok_or(RpcError::BadResponse {
                method: "getBlockNumber".to_string(),
            })
        }

        fn get_account(&self, address: &str) -> Result<Option<RpcAccount>, RpcError> {
            let v = self.call("getAccountByAddress", serde_json::json!([address]))?;
            if v.is_null() {
                return Ok(None);
            }
            Ok(Some(RpcAccount {
                balance: v.get("balance").and_then(|b| b.as_u64()).unwrap_or(0),
                account_type: v
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("basic")
                    .to_string(),
                address: v
                    .get("address")
                    .and_then(|a| a.as_str())
                    .map(|s| s.to_string()),
            }))
        }

        fn send_raw_transaction(&self, raw_hex: &str) -> Result<String, RpcError> {
            let v = self.call("sendRawTransaction", serde_json::json!([raw_hex]))?;
            v.as_str()
                .map(|s| s.to_string())
                .ok_or(RpcError::BadResponse {
                    method: "sendRawTransaction".to_string(),
                })
        }

        fn get_transaction(&self, hash: &str) -> Result<Option<RpcTransaction>, RpcError> {
            let v = self.call("getTransactionByHash", serde_json::json!([hash]))?;
            if v.is_null() {
                return Ok(None);
            }
            Ok(Some(RpcTransaction {
                hash: v
                    .get("hash")
                    .and_then(|h| h.as_str())
                    .unwrap_or(hash)
                    .to_string(),
                block_number: v
                    .get("blockNumber")
                    .and_then(|b| b.as_u64())
                    .map(|n| n as u32),
                confirmations: v
                    .get("confirmations")
                    .and_then(|c| c.as_u64())
                    .map(|n| n as u32),
            }))
        }
    }
}

#[cfg(feature = "gateway-rpc")]
pub use http::HttpGatewayRpc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_accepts_testnet_default() {
        assert!(guard_testnet(DEFAULT_TESTNET_RPC_URL, NetworkId::Testnet).is_ok());
        assert!(guard_testnet("https://rpc.nimiq-testnet.com", NetworkId::Testnet).is_ok());
    }

    #[test]
    fn guard_refuses_mainnet_network() {
        let err = guard_testnet(DEFAULT_TESTNET_RPC_URL, NetworkId::Mainnet).unwrap_err();
        assert!(matches!(err, RpcError::NotTestnet { .. }));
    }

    #[test]
    fn guard_refuses_known_mainnet_url_even_on_testnet_enum() {
        // The mainnet host is refused outright, regardless of the requested network enum.
        let err = guard_testnet("https://rpc.nimiqwatch.com", NetworkId::Testnet).unwrap_err();
        assert!(matches!(err, RpcError::NotTestnet { .. }));
    }

    #[test]
    fn mock_broadcasts_and_polls_inclusion() {
        let rpc = MockRpc::new(1000);
        assert_eq!(rpc.block_number().unwrap(), 1000);
        let hash = rpc.send_raw_transaction("00ff").unwrap();
        assert_eq!(rpc.broadcasts(), vec!["00ff".to_string()]);
        // Pending until confirmed.
        assert_eq!(
            rpc.get_transaction(&hash).unwrap().unwrap().block_number,
            None
        );
        rpc.confirm(&hash, 1001);
        assert_eq!(
            rpc.get_transaction(&hash).unwrap().unwrap().block_number,
            Some(1001)
        );
    }

    #[test]
    fn mock_terminal_rejection_is_not_transient() {
        let rpc = MockRpc::new(10);
        rpc.reject_with("Invalid transaction");
        let err = rpc.send_raw_transaction("dead").unwrap_err();
        assert!(matches!(err, RpcError::Rpc { .. }));
        assert!(!err.is_transient());
        assert!(rpc.broadcasts().is_empty());
    }

    #[test]
    fn mock_transient_failure_is_retryable() {
        let rpc = MockRpc::new(10);
        rpc.fail_transient("connection refused");
        let err = rpc.block_number().unwrap_err();
        assert!(err.is_transient());
        rpc.recover();
        assert_eq!(rpc.block_number().unwrap(), 10);
    }
}
