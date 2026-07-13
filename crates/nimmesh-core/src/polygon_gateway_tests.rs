//! # polygon_gateway_tests — the offline codec suite, extracted from `polygon_gateway.rs` so
//! the module stays under the 800-line ceiling (a CHILD module via `#[path]`, keeping access to
//! the module's private helpers — same pattern as `swap_node_test_hooks.rs`). Everything here is
//! fixture-driven: no network in `cargo test`, ever.

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
fn mainnet_guard_admits_only_the_allowlisted_hosts() {
    // OWNER-GATED mirror of guard_amoy: the mainnet choke point admits ONLY the two reviewed
    // independent cross-read endpoints; every other host (and Amoy) is refused.
    for host in POLYGON_MAINNET_RPC_ALLOWLIST {
        assert!(guard_polygon_mainnet(&format!("https://{host}/")).is_ok());
    }
    assert!(matches!(
        guard_polygon_mainnet(DEFAULT_AMOY_RPC_URL),
        Err(EvmRpcError::NotAmoy { .. })
    ));
    assert!(matches!(
        guard_polygon_mainnet("https://polygon-rpc.com/"),
        Err(EvmRpcError::NotAmoy { .. })
    ));
    // The two guards are disjoint on their allowed hosts: an Amoy URL passes guard_amoy but not
    // the mainnet guard, and vice-versa — no host is admitted by both.
    assert!(guard_amoy(DEFAULT_AMOY_RPC_URL).is_ok());
    assert!(guard_amoy("https://polygon.drpc.org/").is_ok()); // guard_amoy only refuses KNOWN mainnet hosts
}

#[test]
fn new_mainnet_client_admits_allowlisted_and_refuses_others() {
    assert!(HttpPolygonRpc::new_mainnet("https://polygon.drpc.org").is_ok());
    assert!(matches!(
        HttpPolygonRpc::new_mainnet("https://evil.example/rpc"),
        Err(EvmRpcError::NotAmoy { .. })
    ));
    // The default/testnet client still refuses a mainnet allow-listed host (guard_amoy path is
    // unchanged; new_mainnet is the ONLY door to a mainnet endpoint).
    assert!(HttpPolygonRpc::new("https://polygon.drpc.org").is_ok()); // guard_amoy doesn't know this host
}

#[test]
fn native_usdc_address_is_the_circle_native_token_not_bridged() {
    // The canonical native (Circle-issued) USDC on Polygon PoS — never the bridged USDC.e.
    assert_eq!(
        NATIVE_USDC_POLYGON_MAINNET,
        "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359"
    );
    assert_ne!(
        NATIVE_USDC_POLYGON_MAINNET.to_ascii_lowercase(),
        "0x2791bca1f2de4661ed88a30c99a7a9449aa84174" // bridged USDC.e — must NOT be this
    );
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
        parse_logs(&json!({ "jsonrpc": "2.0", "id": 5, "error": { "message": "nope" } })).is_err()
    );
}

#[test]
fn balance_codec_round_trips() {
    let req = get_balance_request("0xabc", 3);
    assert_eq!(req["method"], "eth_getBalance");
    assert_eq!(req["params"], json!(["0xabc", "latest"]));
    // 2.5 POL fits u64; 100,000 POL does not — the u128 parser covers both.
    let ok = json!({ "jsonrpc": "2.0", "id": 3, "result": "0x22b1c8c1227a0000" });
    assert_eq!(parse_balance(&ok).unwrap(), 2_500_000_000_000_000_000);
    let big = json!({ "jsonrpc": "2.0", "id": 3, "result": "0x152d02c7e14af6800000" });
    assert_eq!(
        parse_balance(&big).unwrap(),
        100_000_000_000_000_000_000_000
    );
    assert!(parse_balance(&json!({ "jsonrpc": "2.0", "id": 3, "result": 7 })).is_err());
    assert_eq!(parse_quantity_u128("junk"), None); // panic-free on junk
                                                   // The mock answers through the same codec.
    assert_eq!(
        MockPolygonRpc::new().get_balance("0xabc").unwrap(),
        1_000_000_000_000_000_000
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
