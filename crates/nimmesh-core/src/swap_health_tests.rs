//! # swap_health_tests — discovery health self-check (G55, cfg(test))
//!
//! A small read-only health summary derived purely from the G42 [`IntentMetrics`] counters, so a node
//! or dev can see at a glance whether discovery is working or being abused. No behaviour depends on it.
//! It lives with the tests because the snapshot it reads ([`IntentMetricsSnapshot`]) is itself
//! `cfg(test)` — if discovery health is ever surfaced in production, it lifts to a non-test accessor
//! over the live `IntentMetrics` unchanged.
//!
//! [`IntentMetrics`]: crate::swap_node::IntentMetrics

use crate::swap_node::IntentMetricsSnapshot;

/// Which gate rejected the most intents — a diagnostic hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DominantDrop {
    None,
    Rate,
    Expiry,
    Signature,
    Throttle,
}

/// A coarse classification of the discovery layer's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoveryStatus {
    /// No intents seen yet.
    Idle,
    /// Intents arrive but none match — normal when no compatible counterparty is near (drops are
    /// mostly wrong-rate / expired, not abuse).
    NoCounterpartiesYet,
    /// Rejections are dominated by forged signatures or throttled floods — a sign of abuse.
    PossiblyUnderAttack,
    /// At least one swap has been discovered.
    Healthy,
}

/// A read-only health summary derived from the discovery counters (G55).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiscoveryHealth {
    pub(crate) total_dropped: usize,
    pub(crate) match_rate_pct: usize,
    pub(crate) dominant_drop: DominantDrop,
    pub(crate) status: DiscoveryStatus,
}

impl IntentMetricsSnapshot {
    /// Derive a [`DiscoveryHealth`] from the counters (G55). Pure, total, no allocation.
    pub(crate) fn health(&self) -> DiscoveryHealth {
        let total_dropped = self.dropped_rate
            + self.dropped_expiry
            + self.dropped_throttle
            + self.dropped_signature;
        // Match rate over the RESOLVED intents (matched + dropped); a buffered-but-unresolved intent
        // counts toward neither. `max(1, …)` keeps it well-defined at zero.
        let match_rate_pct = self.matched * 100 / (self.matched + total_dropped).max(1);

        // The biggest single drop reason. The candidate order puts the abuse reasons LAST so that
        // `max_by_key` (which returns the last of equal maxima) breaks ties toward flagging abuse.
        let dominant_drop = [
            (self.dropped_expiry, DominantDrop::Expiry),
            (self.dropped_rate, DominantDrop::Rate),
            (self.dropped_throttle, DominantDrop::Throttle),
            (self.dropped_signature, DominantDrop::Signature),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .max_by_key(|(n, _)| *n)
        .map(|(_, d)| d)
        .unwrap_or(DominantDrop::None);

        let status = if self.seen == 0 {
            DiscoveryStatus::Idle
        } else if self.matched > 0 {
            DiscoveryStatus::Healthy
        } else if matches!(
            dominant_drop,
            DominantDrop::Signature | DominantDrop::Throttle
        ) {
            DiscoveryStatus::PossiblyUnderAttack
        } else {
            DiscoveryStatus::NoCounterpartiesYet
        };

        DiscoveryHealth {
            total_dropped,
            match_rate_pct,
            dominant_drop,
            status,
        }
    }
}

/// Build a snapshot with the given counters (readvertised is irrelevant to health).
fn snap(
    seen: usize,
    matched: usize,
    rate: usize,
    expiry: usize,
    throttle: usize,
    sig: usize,
) -> IntentMetricsSnapshot {
    IntentMetricsSnapshot {
        seen,
        matched,
        dropped_rate: rate,
        dropped_expiry: expiry,
        dropped_throttle: throttle,
        dropped_signature: sig,
        readvertised: 0,
    }
}

#[test]
fn health_classifies_each_discovery_state() {
    // Idle — nothing seen yet.
    let h = snap(0, 0, 0, 0, 0, 0).health();
    assert_eq!(h.status, DiscoveryStatus::Idle);
    assert_eq!(h.total_dropped, 0);
    assert_eq!(h.match_rate_pct, 0);
    assert_eq!(h.dominant_drop, DominantDrop::None);

    // Seen but no match, drops mostly wrong-rate/expired → just no counterparty, not abuse.
    let h = snap(5, 0, 3, 2, 0, 0).health();
    assert_eq!(h.status, DiscoveryStatus::NoCounterpartiesYet);
    assert_eq!(h.dominant_drop, DominantDrop::Rate); // 3 > 2
    assert_eq!(h.total_dropped, 5);
    assert_eq!(h.match_rate_pct, 0);

    // Rejections dominated by forged signatures → possibly under attack.
    let h = snap(10, 0, 1, 0, 2, 5).health();
    assert_eq!(h.status, DiscoveryStatus::PossiblyUnderAttack);
    assert_eq!(h.dominant_drop, DominantDrop::Signature); // 5 is the max

    // A match makes it healthy, and the match rate is over resolved intents.
    let h = snap(8, 3, 1, 0, 0, 0).health();
    assert_eq!(h.status, DiscoveryStatus::Healthy);
    assert_eq!(h.total_dropped, 1);
    assert_eq!(h.match_rate_pct, 75); // 3 / (3 + 1)
    assert_eq!(h.dominant_drop, DominantDrop::Rate);
}

#[test]
fn health_match_rate_and_drop_ties_are_deterministic() {
    assert_eq!(snap(2, 1, 3, 0, 0, 0).health().match_rate_pct, 25); // 1 / (1+3)
    assert_eq!(snap(2, 1, 0, 0, 0, 0).health().match_rate_pct, 100); // no drops

    // A tie between two non-abuse reasons breaks toward the later candidate (rate over expiry).
    assert_eq!(
        snap(4, 0, 2, 2, 0, 0).health().dominant_drop,
        DominantDrop::Rate
    );
    // A tie that includes an abuse reason breaks toward flagging abuse.
    let h = snap(4, 0, 2, 0, 0, 2).health();
    assert_eq!(h.dominant_drop, DominantDrop::Signature);
    assert_eq!(h.status, DiscoveryStatus::PossiblyUnderAttack);
}

#[test]
fn health_reads_a_forged_flood_off_a_real_node_as_an_attack() {
    // End to end: a node fed only forged intents reports PossiblyUnderAttack via the live counters.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_discovery_tests::{btc_giver_intent, intent_for, intent_frame, FRESH};
    use crate::swap_intent::Asset;
    use crate::test_support::{participant_fixtures, wait_until, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    // Three forged (unsigned) but rate-crossing intents from distinct spoofed origins.
    for i in 0..3u8 {
        let forged = btc_giver_intent(0x90 + i); // never signed → verify_authentic fails
        alice.on_packet_received_from(
            "evil".to_string(),
            intent_frame(&forged, [0xD0 + i; 8], 1 + i as u64),
        );
    }
    assert!(
        wait_until(|| alice.intent_metrics().dropped_signature >= 3, SETTLE),
        "the forged flood should register as signature drops"
    );

    let health = alice.intent_metrics().health();
    assert_eq!(health.status, DiscoveryStatus::PossiblyUnderAttack);
    assert_eq!(health.dominant_drop, DominantDrop::Signature);
    assert_eq!(health.match_rate_pct, 0);

    h.shutdown();
}
