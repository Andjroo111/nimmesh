//! # polygon_gateway — the Polygon (EVM) JSON-RPC client (P8, behind `polygon-gateway`)
//!
//! The Polygon analog of [`crate::rpc`]/[`crate::gateway`]: the one online hop that takes a P4
//! signed USDC-leg transaction and puts it on **Polygon Amoy testnet**, plus the read queries the
//! leg needs (nonce, gas price, receipt, `eth_call` for HTLC/USDC state). It mirrors `gateway-rpc`
//! and `bitcoin-gateway` EXACTLY: the request/response **codec is a set of pure functions, unit-tested
//! OFFLINE against hardcoded JSON fixtures**, and the HTTP transport ([`HttpPolygonRpc`]) is layered on
//! top — so `cargo test` never touches the network.
//!
//! ## Money-path gating
//!
//! Every constructor runs [`guard_amoy`]: the URL must not be a known Polygon **mainnet** host. The
//! signed txs already carry the Amoy chain id ([`crate::evm_rlp::POLYGON_AMOY_CHAIN_ID`] = 80002). The
//! live `eth_sendRawTransaction` broadcast + real funds + mainnet are **owner-gated** (a manual/example
//! call); the loop never performs a live broadcast in its gate.

use std::fmt;

/// The default public Polygon **Amoy testnet** JSON-RPC endpoint.
pub const DEFAULT_AMOY_RPC_URL: &str = "https://rpc-amoy.polygon.technology";

/// Known Polygon **mainnet** RPC host fragments [`guard_amoy`] refuses outright — real-money
/// endpoints the autonomous loop must never reach.
pub const MAINNET_RPC_HOSTS: &[&str] = &["polygon-rpc.com", "polygon-mainnet", "polygon.llamarpc"];

/// A failure talking to the EVM RPC node. The transient/terminal split mirrors [`crate::rpc::RpcError`]:
/// a [`EvmRpcError::Transport`] / 5xx / 429 is worth retrying; a node `error` object is terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvmRpcError {
    /// The Amoy guard tripped (the URL looks like a Polygon mainnet host).
    NotAmoy {
        /// Why the URL was refused.
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
    /// The response was missing `result` or the wrong shape for the method.
    BadResponse {
        /// The JSON-RPC method whose response could not be parsed.
        method: String,
    },
}

impl EvmRpcError {
    /// Whether retrying might succeed (network blip / overload) vs a terminal rejection.
    pub fn is_transient(&self) -> bool {
        match self {
            EvmRpcError::Transport(_) => true,
            EvmRpcError::Http { status, .. } => *status >= 500 || *status == 429,
            _ => false,
        }
    }
}

impl fmt::Display for EvmRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvmRpcError::NotAmoy { reason } => write!(f, "refused non-Amoy RPC: {reason}"),
            EvmRpcError::Http { status, method } => write!(f, "evm rpc http {status} ({method})"),
            EvmRpcError::Transport(e) => write!(f, "evm rpc transport error: {e}"),
            EvmRpcError::Rpc { method, message } => write!(f, "{method}: {message}"),
            EvmRpcError::BadResponse { method } => write!(f, "{method}: malformed response"),
        }
    }
}

impl std::error::Error for EvmRpcError {}

/// Refuse any RPC target that looks like Polygon **mainnet** — the single choke point that keeps the
/// money path on Amoy testnet. (The Amoy chain id `80002` is bound into the signed tx itself, P4.)
pub fn guard_amoy(url: &str) -> Result<(), EvmRpcError> {
    let lower = url.to_ascii_lowercase();
    for host in MAINNET_RPC_HOSTS {
        if lower.contains(host) {
            return Err(EvmRpcError::NotAmoy {
                reason: format!("URL '{url}' matches a known mainnet RPC host '{host}'"),
            });
        }
    }
    Ok(())
}

// --- the pure JSON-RPC codec (offline-tested) ----------------------------------------------------

/// Build a JSON-RPC 2.0 request envelope for `method`/`params`/`id`.
pub fn json_rpc_request(method: &str, params: serde_json::Value, id: u64) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": id })
}

/// Format a `u64` as a minimal EVM hex-quantity (`9` → `"0x9"`, `0` → `"0x0"`).
pub fn quantity_hex(v: u64) -> String {
    format!("0x{v:x}")
}

/// Parse an EVM hex-quantity (`"0x9"`) to `u64`; `None` on a missing prefix / non-hex / overflow.
pub fn parse_quantity(s: &str) -> Option<u64> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u64::from_str_radix(h, 16).ok()
}

/// Pull the `result` out of a JSON-RPC response, mapping a non-null `error` object to a terminal
/// [`EvmRpcError::Rpc`] and a missing `result` to [`EvmRpcError::BadResponse`]. Panic-free on any JSON.
fn parse_result(method: &str, resp: &serde_json::Value) -> Result<serde_json::Value, EvmRpcError> {
    if let Some(err) = resp.get("error") {
        if !err.is_null() {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| err.to_string());
            return Err(EvmRpcError::Rpc {
                method: method.to_string(),
                message,
            });
        }
    }
    resp.get("result")
        .cloned()
        .ok_or_else(|| EvmRpcError::BadResponse {
            method: method.to_string(),
        })
}

/// An EVM transaction receipt as the leg reads it back when polling for inclusion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmReceipt {
    /// The transaction hash (lowercase `0x…`).
    pub tx_hash: String,
    /// `true` if the tx succeeded (receipt `status == 0x1`), `false` if it reverted (`0x0`).
    pub success: bool,
    /// The block the tx was included in.
    pub block_number: u64,
}

/// `eth_getTransactionCount(address, "pending")` — the next nonce. Request builder + parser.
pub fn get_transaction_count_request(address: &str, id: u64) -> serde_json::Value {
    json_rpc_request(
        "eth_getTransactionCount",
        serde_json::json!([address, "pending"]),
        id,
    )
}

/// Parse an `eth_getTransactionCount` response → the nonce.
pub fn parse_transaction_count(resp: &serde_json::Value) -> Result<u64, EvmRpcError> {
    let r = parse_result("eth_getTransactionCount", resp)?;
    r.as_str()
        .and_then(parse_quantity)
        .ok_or(EvmRpcError::BadResponse {
            method: "eth_getTransactionCount".to_string(),
        })
}

/// `eth_gasPrice()` — the current gas price in wei. Request builder + parser.
pub fn gas_price_request(id: u64) -> serde_json::Value {
    json_rpc_request("eth_gasPrice", serde_json::json!([]), id)
}

/// Parse an `eth_gasPrice` response → wei.
pub fn parse_gas_price(resp: &serde_json::Value) -> Result<u64, EvmRpcError> {
    let r = parse_result("eth_gasPrice", resp)?;
    r.as_str()
        .and_then(parse_quantity)
        .ok_or(EvmRpcError::BadResponse {
            method: "eth_gasPrice".to_string(),
        })
}

/// `eth_sendRawTransaction(rawHex)` — broadcast a P4 signed tx. Request builder + parser. `raw_hex`
/// must be `0x`-prefixed. **The LIVE call is owner-gated**; this only builds/parses bytes.
pub fn send_raw_transaction_request(raw_hex: &str, id: u64) -> serde_json::Value {
    json_rpc_request("eth_sendRawTransaction", serde_json::json!([raw_hex]), id)
}

/// Parse an `eth_sendRawTransaction` response → the tx hash.
pub fn parse_send_raw_transaction(resp: &serde_json::Value) -> Result<String, EvmRpcError> {
    let r = parse_result("eth_sendRawTransaction", resp)?;
    r.as_str()
        .map(|s| s.to_string())
        .ok_or(EvmRpcError::BadResponse {
            method: "eth_sendRawTransaction".to_string(),
        })
}

/// `eth_getTransactionReceipt(hash)`. Request builder + parser. The result is `null` while pending.
pub fn get_transaction_receipt_request(tx_hash: &str, id: u64) -> serde_json::Value {
    json_rpc_request(
        "eth_getTransactionReceipt",
        serde_json::json!([tx_hash]),
        id,
    )
}

/// Parse an `eth_getTransactionReceipt` response → `Some(receipt)` once mined, `None` while pending.
pub fn parse_transaction_receipt(
    resp: &serde_json::Value,
) -> Result<Option<EvmReceipt>, EvmRpcError> {
    let r = parse_result("eth_getTransactionReceipt", resp)?;
    if r.is_null() {
        return Ok(None); // still pending — the node knows no receipt yet
    }
    let bad = || EvmRpcError::BadResponse {
        method: "eth_getTransactionReceipt".to_string(),
    };
    let tx_hash = r
        .get("transactionHash")
        .and_then(|h| h.as_str())
        .ok_or_else(bad)?
        .to_string();
    let success = r
        .get("status")
        .and_then(|s| s.as_str())
        .and_then(parse_quantity)
        .ok_or_else(bad)?
        == 1;
    let block_number = r
        .get("blockNumber")
        .and_then(|b| b.as_str())
        .and_then(parse_quantity)
        .ok_or_else(bad)?;
    Ok(Some(EvmReceipt {
        tx_hash,
        success,
        block_number,
    }))
}

/// `eth_call({to, data}, "latest")` — a read-only contract call (HTLC state / USDC `balanceOf`).
/// Request builder + parser. `to` is a `0x…` address, `data` is the `0x…` calldata (P3 `evm_abi`).
pub fn eth_call_request(to: &str, data: &str, id: u64) -> serde_json::Value {
    json_rpc_request(
        "eth_call",
        serde_json::json!([{ "to": to, "data": data }, "latest"]),
        id,
    )
}

/// Parse an `eth_call` response → the returned `0x…` data string.
pub fn parse_eth_call(resp: &serde_json::Value) -> Result<String, EvmRpcError> {
    let r = parse_result("eth_call", resp)?;
    r.as_str()
        .map(|s| s.to_string())
        .ok_or(EvmRpcError::BadResponse {
            method: "eth_call".to_string(),
        })
}

/// `eth_blockNumber()` — the current head height. Request builder + parser.
pub fn block_number_request(id: u64) -> serde_json::Value {
    json_rpc_request("eth_blockNumber", serde_json::json!([]), id)
}

/// Parse an `eth_blockNumber` response → the head height.
pub fn parse_block_number(resp: &serde_json::Value) -> Result<u64, EvmRpcError> {
    let r = parse_result("eth_blockNumber", resp)?;
    r.as_str()
        .and_then(parse_quantity)
        .ok_or(EvmRpcError::BadResponse {
            method: "eth_blockNumber".to_string(),
        })
}

/// `eth_getLogs` filtered to one contract + an event signature (topic 0) + an optional indexed
/// topic 3 — how the Polygon funding verifier (#72 tail) finds `NewSwap` escrows paying OUR
/// recipient. Request builder + parser.
pub fn get_logs_request(
    address: &str,
    topic0: &str,
    topic3: Option<&str>,
    from_block: u64,
    id: u64,
) -> serde_json::Value {
    json_rpc_request(
        "eth_getLogs",
        serde_json::json!([{
            "address": address,
            "fromBlock": quantity_hex(from_block),
            "toBlock": "latest",
            "topics": [topic0, serde_json::Value::Null, serde_json::Value::Null, topic3],
        }]),
        id,
    )
}

/// One log entry, decoded to the slice the verifier needs: the first indexed argument
/// (topic 1 — the swap id for `NewSwap`), the raw data words, and the inclusion block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmLog {
    /// The event's first indexed argument (topic 1).
    pub topic1: [u8; 32],
    /// The non-indexed data words, decoded from hex.
    pub data: Vec<u8>,
    /// The block the event was emitted in.
    pub block_number: u64,
}

/// Parse an `eth_getLogs` response → the decoded entries. Entries missing a topic 1, with
/// non-hex fields, or with a malformed block number are skipped (panic-free on any JSON).
pub fn parse_logs(resp: &serde_json::Value) -> Result<Vec<EvmLog>, EvmRpcError> {
    let r = parse_result("eth_getLogs", resp)?;
    let arr = r.as_array().ok_or(EvmRpcError::BadResponse {
        method: "eth_getLogs".to_string(),
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let Some(t1) = entry
            .get("topics")
            .and_then(|t| t.as_array())
            .and_then(|t| t.get(1))
            .and_then(|t| t.as_str())
            .and_then(|t| decode_hex(t.strip_prefix("0x").unwrap_or(t)))
        else {
            continue;
        };
        let Ok(topic1) = <[u8; 32]>::try_from(t1.as_slice()) else {
            continue;
        };
        let Some(data) = entry
            .get("data")
            .and_then(|d| d.as_str())
            .and_then(|d| decode_hex(d.strip_prefix("0x").unwrap_or(d)))
        else {
            continue;
        };
        let Some(block_number) = entry
            .get("blockNumber")
            .and_then(|b| b.as_str())
            .and_then(parse_quantity)
        else {
            continue;
        };
        out.push(EvmLog {
            topic1,
            data,
            block_number,
        });
    }
    Ok(out)
}

// --- HttpPolygonRpc: the real blocking HTTP client (live calls owner-gated) -----------------------

/// The live blocking Polygon JSON-RPC client over `ureq` (no tokio). **Amoy-guarded at construction**.
/// The `send`/read methods perform real network I/O and are owner-gated — they are NEVER exercised in
/// `cargo test`; the codec above is what the gate validates.
pub struct HttpPolygonRpc {
    url: String,
    agent: ureq::Agent,
    next_id: std::sync::atomic::AtomicU64,
}

impl HttpPolygonRpc {
    /// Build a client for `url`. Errors via [`guard_amoy`] if the URL looks like Polygon mainnet.
    pub fn new(url: impl Into<String>) -> Result<Self, EvmRpcError> {
        let url = url.into();
        guard_amoy(&url)?;
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build();
        Ok(HttpPolygonRpc {
            url,
            agent,
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// A client against the default public Amoy endpoint ([`DEFAULT_AMOY_RPC_URL`]).
    pub fn amoy_default() -> Self {
        Self::new(DEFAULT_AMOY_RPC_URL).expect("default Amoy endpoint passes the guard")
    }

    /// The endpoint this client talks to.
    pub fn url(&self) -> &str {
        &self.url
    }

    fn next_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// POST one JSON-RPC request `Value` and return the response `Value`. The single networking
    /// primitive; the typed methods build a request via the codec, call this, and parse the result.
    fn post(
        &self,
        request: serde_json::Value,
        method: &str,
    ) -> Result<serde_json::Value, EvmRpcError> {
        match self.agent.post(&self.url).send_json(request) {
            Ok(r) => r
                .into_json()
                .map_err(|e| EvmRpcError::Transport(e.to_string())),
            Err(ureq::Error::Status(status, _)) => Err(EvmRpcError::Http {
                status,
                method: method.to_string(),
            }),
            Err(ureq::Error::Transport(t)) => Err(EvmRpcError::Transport(t.to_string())),
        }
    }

    /// The next account nonce (LIVE — owner-gated).
    pub fn get_transaction_count(&self, address: &str) -> Result<u64, EvmRpcError> {
        let req = get_transaction_count_request(address, self.next_id());
        parse_transaction_count(&self.post(req, "eth_getTransactionCount")?)
    }

    /// The current gas price in wei (LIVE — owner-gated).
    pub fn gas_price(&self) -> Result<u64, EvmRpcError> {
        let req = gas_price_request(self.next_id());
        parse_gas_price(&self.post(req, "eth_gasPrice")?)
    }

    /// Broadcast a `0x`-prefixed signed raw tx, returning its hash (LIVE BROADCAST — owner-gated;
    /// real funds + mainnet = needs:owner).
    pub fn send_raw_transaction(&self, raw_hex: &str) -> Result<String, EvmRpcError> {
        let req = send_raw_transaction_request(raw_hex, self.next_id());
        parse_send_raw_transaction(&self.post(req, "eth_sendRawTransaction")?)
    }

    /// Poll a tx receipt; `None` until mined (LIVE — owner-gated).
    pub fn get_transaction_receipt(
        &self,
        tx_hash: &str,
    ) -> Result<Option<EvmReceipt>, EvmRpcError> {
        let req = get_transaction_receipt_request(tx_hash, self.next_id());
        parse_transaction_receipt(&self.post(req, "eth_getTransactionReceipt")?)
    }

    /// A read-only contract call (LIVE — owner-gated; read-only, no funds).
    pub fn eth_call(&self, to: &str, data: &str) -> Result<String, EvmRpcError> {
        let req = eth_call_request(to, data, self.next_id());
        parse_eth_call(&self.post(req, "eth_call")?)
    }

    /// The current head height (LIVE — read-only).
    pub fn block_number(&self) -> Result<u64, EvmRpcError> {
        let req = block_number_request(self.next_id());
        parse_block_number(&self.post(req, "eth_blockNumber")?)
    }

    /// Filtered event logs (LIVE — read-only). See [`get_logs_request`].
    pub fn get_logs(
        &self,
        address: &str,
        topic0: &str,
        topic3: Option<&str>,
        from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError> {
        let req = get_logs_request(address, topic0, topic3, from_block, self.next_id());
        parse_logs(&self.post(req, "eth_getLogs")?)
    }
}

// --- MockPolygonRpc: an in-memory fake for the SIM dry-run (no network) ---------------------------

/// A deterministic, in-memory stand-in for [`HttpPolygonRpc`] for the P9 sim dry-run + tests. It does
/// **no network**: it RECORDS the raw-hex broadcasts (so a test can assert exactly the signed bytes
/// were submitted) and answers each call by building the request through the real codec and parsing a
/// synthesized JSON response through the real parser — so the dry-run exercises the actual codec both
/// directions, just without HTTP. The `eth_sendRawTransaction` hash it returns is the REAL Ethereum tx
/// hash (`keccak256` of the signed bytes), so it lines up with what a node would assign.
pub struct MockPolygonRpc {
    sent: std::sync::Mutex<Vec<String>>,
    nonce: u64,
    gas_price: u64,
    head_block: u64,
}

impl Default for MockPolygonRpc {
    fn default() -> Self {
        MockPolygonRpc {
            sent: std::sync::Mutex::new(Vec::new()),
            nonce: 0,
            gas_price: 30_000_000_000, // 30 gwei
            head_block: 0x10,
        }
    }
}

impl MockPolygonRpc {
    /// A mock at nonce 0, 30 gwei, head block 16.
    pub fn new() -> Self {
        Self::default()
    }

    /// A mock reporting `nonce` for `get_transaction_count`.
    pub fn at_nonce(nonce: u64) -> Self {
        MockPolygonRpc {
            nonce,
            ..Self::default()
        }
    }

    /// The raw-hex blobs broadcast so far, in order.
    pub fn broadcasts(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    /// The deterministic tx hash this mock assigns a `0x`-prefixed raw tx — the REAL Ethereum hash,
    /// `keccak256` of the signed bytes (decoded from the hex), so it matches a node's assignment.
    fn fixture_hash(raw_hex: &str) -> String {
        let bytes = decode_hex(raw_hex.strip_prefix("0x").unwrap_or(raw_hex)).unwrap_or_default();
        format!(
            "0x{}",
            crate::nimiq::hex::bytes_to_hex(&crate::evm::keccak256(&bytes))
        )
    }

    /// Next nonce — builds the request + parses a synthesized response through the real codec.
    pub fn get_transaction_count(&self, address: &str) -> Result<u64, EvmRpcError> {
        let _req = get_transaction_count_request(address, 1);
        let resp =
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": quantity_hex(self.nonce) });
        parse_transaction_count(&resp)
    }

    /// Gas price — same codec round-trip, no network.
    pub fn gas_price(&self) -> Result<u64, EvmRpcError> {
        let _req = gas_price_request(1);
        let resp = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": quantity_hex(self.gas_price) });
        parse_gas_price(&resp)
    }

    /// Record the broadcast + return the (real) tx hash, parsed back through the codec.
    pub fn send_raw_transaction(&self, raw_hex: &str) -> Result<String, EvmRpcError> {
        let _req = send_raw_transaction_request(raw_hex, 1);
        self.sent.lock().unwrap().push(raw_hex.to_string());
        let hash = Self::fixture_hash(raw_hex);
        let resp = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": hash });
        parse_send_raw_transaction(&resp)
    }

    /// A successful receipt at the mock's head block, via the real receipt parser.
    pub fn get_transaction_receipt(
        &self,
        tx_hash: &str,
    ) -> Result<Option<EvmReceipt>, EvmRpcError> {
        let _req = get_transaction_receipt_request(tx_hash, 1);
        let resp = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "transactionHash": tx_hash, "status": "0x1", "blockNumber": quantity_hex(self.head_block) }
        });
        parse_transaction_receipt(&resp)
    }
}

/// Decode a hex string (no `0x`) to bytes; `None` on odd length / non-hex (used by the mock's hash).
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn guard_refuses_polygon_mainnet_hosts_accepts_amoy() {
        assert!(guard_amoy(DEFAULT_AMOY_RPC_URL).is_ok());
        assert!(guard_amoy("https://rpc.ankr.com/polygon_amoy").is_ok());
        for host in MAINNET_RPC_HOSTS {
            assert!(matches!(
                guard_amoy(&format!("https://{host}/")),
                Err(EvmRpcError::NotAmoy { .. })
            ));
        }
    }

    #[test]
    fn quantity_hex_round_trips() {
        for v in [0u64, 1, 9, 21_000, 80_002, u64::MAX] {
            assert_eq!(parse_quantity(&quantity_hex(v)), Some(v));
        }
        assert_eq!(quantity_hex(9), "0x9");
        assert_eq!(parse_quantity("0x5208"), Some(21_000));
        assert_eq!(parse_quantity("not-hex"), None); // panic-free on junk
        assert_eq!(parse_quantity("0xZZ"), None);
    }

    #[test]
    fn logs_and_block_number_codecs_round_trip() {
        assert_eq!(block_number_request(4)["method"], "eth_blockNumber");
        let head = json!({ "jsonrpc": "2.0", "id": 4, "result": "0x2753924" });
        assert_eq!(parse_block_number(&head).unwrap(), 0x2753924);

        let req = get_logs_request("0xdead", "0xt0", Some("0xt3"), 7, 5);
        assert_eq!(req["method"], "eth_getLogs");
        assert_eq!(req["params"][0]["fromBlock"], "0x7");
        assert_eq!(req["params"][0]["topics"][0], "0xt0");
        assert_eq!(req["params"][0]["topics"][3], "0xt3");
        assert!(req["params"][0]["topics"][1].is_null());

        // A realistic entry (the shape Amoy returned for the deployed HTLC's NewSwap), plus a
        // malformed sibling that must be skipped, not panicked on.
        let resp = json!({ "jsonrpc": "2.0", "id": 5, "result": [
            {
                "topics": ["0xaaaa", format!("0x{}", "11".repeat(32)), "0xbb", "0xcc"],
                "data": format!("0x{}", "22".repeat(96)),
                "blockNumber": "0x64"
            },
            { "topics": [], "data": 7, "blockNumber": null }
        ]});
        let logs = parse_logs(&resp).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].topic1, [0x11; 32]);
        assert_eq!(logs[0].data, vec![0x22; 96]);
        assert_eq!(logs[0].block_number, 100);
        // And a node error is terminal, not a panic.
        assert!(
            parse_logs(&json!({ "jsonrpc": "2.0", "id": 5, "error": { "message": "nope" } }))
                .is_err()
        );
    }

    #[test]
    fn request_envelopes_have_the_jsonrpc_shape() {
        let req = get_transaction_count_request("0xabc", 7);
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["method"], "eth_getTransactionCount");
        assert_eq!(req["params"], json!(["0xabc", "pending"]));
        assert_eq!(req["id"], 7);

        assert_eq!(gas_price_request(1)["method"], "eth_gasPrice");
        assert_eq!(
            send_raw_transaction_request("0xf86c09", 2)["params"],
            json!(["0xf86c09"])
        );
        let call = eth_call_request("0xcontract", "0x70a08231", 3);
        assert_eq!(call["method"], "eth_call");
        assert_eq!(call["params"][0]["to"], "0xcontract");
        assert_eq!(call["params"][0]["data"], "0x70a08231");
        assert_eq!(call["params"][1], "latest");
    }

    #[test]
    fn parses_transaction_count_and_gas_price_fixtures() {
        let resp = json!({ "jsonrpc": "2.0", "id": 1, "result": "0x9" });
        assert_eq!(parse_transaction_count(&resp).unwrap(), 9);
        let gas = json!({ "jsonrpc": "2.0", "id": 1, "result": "0x6fc23ac00" }); // 30 gwei
        assert_eq!(parse_gas_price(&gas).unwrap(), 30_000_000_000);
    }

    #[test]
    fn parses_send_raw_transaction_hash_fixture() {
        let resp = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
        });
        assert_eq!(
            parse_send_raw_transaction(&resp).unwrap(),
            "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
        );
    }

    #[test]
    fn parses_receipt_success_failure_and_pending_fixtures() {
        let mined = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "transactionHash": "0xdead", "status": "0x1", "blockNumber": "0x10" }
        });
        assert_eq!(
            parse_transaction_receipt(&mined).unwrap(),
            Some(EvmReceipt {
                tx_hash: "0xdead".to_string(),
                success: true,
                block_number: 16,
            })
        );
        let reverted = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "transactionHash": "0xbeef", "status": "0x0", "blockNumber": "0x11" }
        });
        assert!(
            !parse_transaction_receipt(&reverted)
                .unwrap()
                .unwrap()
                .success
        );
        // null result = still pending.
        let pending = json!({ "jsonrpc": "2.0", "id": 1, "result": null });
        assert_eq!(parse_transaction_receipt(&pending).unwrap(), None);
    }

    #[test]
    fn parses_eth_call_data_fixture() {
        // balanceOf → a 32-byte word (here 1_500_000 micro-USDC = 1.5 USDC).
        let resp = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": "0x000000000000000000000000000000000000000000000000000000000016e360"
        });
        let data = parse_eth_call(&resp).unwrap();
        assert_eq!(parse_quantity(&data), Some(1_500_000));
    }

    #[test]
    fn a_node_error_object_is_a_terminal_rpc_error() {
        let resp = json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32000, "message": "nonce too low" }
        });
        let err = parse_send_raw_transaction(&resp).unwrap_err();
        assert!(matches!(err, EvmRpcError::Rpc { ref message, .. } if message == "nonce too low"));
        assert!(!err.is_transient()); // a node rejection is terminal
    }

    #[test]
    fn malformed_responses_are_structured_errors_not_panics() {
        // Missing result → BadResponse.
        let no_result = json!({ "jsonrpc": "2.0", "id": 1 });
        assert!(matches!(
            parse_transaction_count(&no_result),
            Err(EvmRpcError::BadResponse { .. })
        ));
        // Result of the wrong type → BadResponse, no panic.
        let wrong_type = json!({ "jsonrpc": "2.0", "id": 1, "result": 9 });
        assert!(matches!(
            parse_gas_price(&wrong_type),
            Err(EvmRpcError::BadResponse { .. })
        ));
        // Arbitrary hostile JSON shapes never panic.
        for v in [json!(null), json!([]), json!("x"), json!({ "result": {} })] {
            let _ = parse_transaction_receipt(&v);
            let _ = parse_eth_call(&v);
        }
    }

    #[test]
    fn http_errors_transient_classification() {
        assert!(EvmRpcError::Http {
            status: 503,
            method: "x".into()
        }
        .is_transient());
        assert!(EvmRpcError::Http {
            status: 429,
            method: "x".into()
        }
        .is_transient());
        assert!(!EvmRpcError::Http {
            status: 400,
            method: "x".into()
        }
        .is_transient());
        assert!(EvmRpcError::Transport("dns".into()).is_transient());
    }
}
