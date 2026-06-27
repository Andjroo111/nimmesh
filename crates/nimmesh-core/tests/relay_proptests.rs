//! Property / fuzz tests for the G6 relay engine (PROTOCOL.md "TTL / hop cap & relay",
//! "Dedup", "Fragmentation"). Three invariants the mesh must never violate:
//!
//! 1. **Loop-freedom** — a packet never relays forever: the TTL hop cap strictly
//!    terminates from *any* starting TTL, in a bounded number of hops.
//! 2. **Dedup correctness** — a node acts on (delivers/relays) a packet at most once: the
//!    LRU returns "fresh" for a given key at most once while it is remembered.
//! 3. **Fragment round-trip** — any payload fragmented and reassembled (in any order)
//!    comes back byte-identical with its original type.
//!
//! Plus a topology property: **adaptive relay still reaches the gateway** across a random
//! connected sparse mesh (where degree-adaptive flooding always relays).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nimmesh_core::dedup::DedupCache;
use nimmesh_core::default_network;
use nimmesh_core::fragment::{fragment_message, parse_fragment, Reassembler};
use nimmesh_core::gateway::MockGateway;
use nimmesh_core::mock_radio::MeshHarness;
use nimmesh_core::relay::relayed_ttl;
use nimmesh_core::settlement::PaymentStatus;
use proptest::prelude::*;

/// Poll `f` until true or `timeout` elapses (the mesh is async / threaded).
fn wait_until<F: Fn() -> bool>(f: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    f()
}

proptest! {
    /// Loop-freedom: relaying strictly shrinks the TTL and terminates within the hop cap,
    /// from any starting value — a packet can never circulate forever.
    #[test]
    fn ttl_relay_is_loop_free(start in any::<u8>()) {
        let mut ttl = start;
        let mut hops = 0u32;
        while let Some(next) = relayed_ttl(ttl) {
            prop_assert!(next < ttl, "TTL must strictly decrease on relay");
            ttl = next;
            hops += 1;
            prop_assert!(hops <= 7, "relay must terminate within the 7-hop cap");
        }
    }

    /// Dedup correctness: over an arbitrary stream of keys (within capacity, so nothing is
    /// evicted), the cache reports a key "fresh" at most once — i.e. a node would act on a
    /// packet at most once.
    #[test]
    fn dedup_reports_each_key_fresh_at_most_once(
        keys in proptest::collection::vec(any::<u64>(), 0..512)
    ) {
        // Capacity comfortably exceeds the key count, so no age-eviction confounds this.
        let mut cache: DedupCache<u64> = DedupCache::new(4096);
        let mut seen_fresh: HashSet<u64> = HashSet::new();
        for k in &keys {
            let fresh = cache.insert(*k);
            if fresh {
                prop_assert!(seen_fresh.insert(*k), "key reported fresh twice");
            } else {
                prop_assert!(seen_fresh.contains(k), "duplicate before any fresh sight");
            }
        }
    }

    /// Fragment round-trip: any payload split at any chunk size reassembles byte-for-byte
    /// (even delivered in reverse order), carrying its original message type.
    #[test]
    fn fragment_reassembly_round_trips(
        payload in proptest::collection::vec(any::<u8>(), 0..3000),
        chunk in 1usize..600,
        ty in any::<u8>(),
        reverse in any::<bool>(),
    ) {
        let mut frags = fragment_message(ty, &payload, chunk, [0xAB; 8]);
        if reverse {
            frags.reverse();
        }
        let mut r = Reassembler::new();
        let mut out = None;
        for (i, fp) in frags.iter().enumerate() {
            let f = parse_fragment(fp).expect("our own fragment parses");
            if let Some(done) = r.accept(f, 1000 + i as u64) {
                out = Some(done);
            }
        }
        prop_assert_eq!(out, Some((ty, payload)));
    }
}

proptest! {
    // Spinning up real worker-threaded meshes is heavier than a pure proptest, so keep the
    // case count modest — the topologies are still randomized and connected.
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Adaptive relay reaches the gateway across a random **connected sparse** mesh: a
    /// random tree of `n` nodes (each node links to an earlier one) keeps every degree
    /// below the high-degree threshold, so degree-adaptive flooding always relays. A
    /// payment submitted from the deepest node must reach the gateway (node 0) and settle.
    #[test]
    fn adaptive_relay_reaches_gateway_in_sparse_tree(
        // parents[i] is the earlier node that node (i+1) attaches to.
        parents in proptest::collection::vec(any::<usize>(), 1..6)
    ) {
        let n = parents.len() + 1; // nodes 0..n; node 0 is the gateway.
        let mut h = MeshHarness::new();
        let gw = Arc::new(MockGateway::new(default_network()));
        h.add_gateway("n0", &[0], gw.clone());
        for i in 1..n {
            h.add_node(&format!("n{i}"), &[i as u8]);
        }
        // Wire the tree: node i attaches to parents[i-1] mod i (always an earlier node →
        // connected + acyclic). Max degree stays small for n < 6.
        for i in 1..n {
            let parent = parents[i - 1] % i;
            h.connect(&format!("n{i}"), &format!("n{parent}"));
        }

        // Originate from the last-added node (typically the farthest from the gateway).
        let origin = h.node(&format!("n{}", n - 1));
        let tx_id = origin.submit_local_tx(b"sparse-mesh-reachability".to_vec());

        let settled = wait_until(
            || origin.payment_status(tx_id.clone()) == PaymentStatus::Settled,
            Duration::from_secs(3),
        );
        prop_assert!(settled, "payment did not reach the gateway across the sparse tree");
        prop_assert_eq!(gw.submission_count(), 1);
        h.shutdown();
    }
}
