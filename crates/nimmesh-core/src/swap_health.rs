//! # swap_health — discovery health derivation (G55 logic, G57 lifted to non-test)
//!
//! A small read-only health summary derived purely from the G42 [`crate::swap_node::IntentMetrics`]
//! counters, so a node/dev can see at a glance whether discovery is working or being abused. No
//! behaviour depends on it. The demo surfaces it via `/api/health` ([`crate::demo_http`]); a live node
//! would expose it once the native bridge lands (OG-6). Pure + total, no allocation.

/// Which gate rejected the most intents — a diagnostic hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DominantDrop {
    None,
    Rate,
    Expiry,
    Signature,
    Throttle,
}

impl DominantDrop {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DominantDrop::None => "none",
            DominantDrop::Rate => "rate",
            DominantDrop::Expiry => "expiry",
            DominantDrop::Signature => "signature",
            DominantDrop::Throttle => "throttle",
        }
    }
}

/// A coarse classification of the discovery layer's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoveryStatus {
    /// No intents seen yet.
    Idle,
    /// Intents arrive but none match — normal when no compatible counterparty is near.
    NoCounterpartiesYet,
    /// Rejections dominated by forged signatures or throttled floods — a sign of abuse.
    PossiblyUnderAttack,
    /// At least one swap has been discovered.
    Healthy,
}

impl DiscoveryStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DiscoveryStatus::Idle => "Idle",
            DiscoveryStatus::NoCounterpartiesYet => "No counterparties yet",
            DiscoveryStatus::PossiblyUnderAttack => "Possibly under attack",
            DiscoveryStatus::Healthy => "Healthy",
        }
    }
}

/// A read-only health summary derived from the discovery counters (G55).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiscoveryHealth {
    pub(crate) total_dropped: usize,
    pub(crate) match_rate_pct: usize,
    pub(crate) dominant_drop: DominantDrop,
    pub(crate) status: DiscoveryStatus,
}

/// Derive a [`DiscoveryHealth`] from the raw G42 discovery counters. Pure and total.
pub(crate) fn discovery_health(
    seen: usize,
    matched: usize,
    dropped_rate: usize,
    dropped_expiry: usize,
    dropped_throttle: usize,
    dropped_signature: usize,
) -> DiscoveryHealth {
    let total_dropped = dropped_rate + dropped_expiry + dropped_throttle + dropped_signature;
    // Match rate over the RESOLVED intents (matched + dropped); buffered-but-unresolved count toward
    // neither. `max(1, …)` keeps it well-defined at zero.
    let match_rate_pct = matched * 100 / (matched + total_dropped).max(1);

    // The biggest single drop reason. Abuse reasons go LAST so `max_by_key` (which returns the last of
    // equal maxima) breaks ties toward flagging abuse.
    let dominant_drop = [
        (dropped_expiry, DominantDrop::Expiry),
        (dropped_rate, DominantDrop::Rate),
        (dropped_throttle, DominantDrop::Throttle),
        (dropped_signature, DominantDrop::Signature),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .max_by_key(|(n, _)| *n)
    .map(|(_, d)| d)
    .unwrap_or(DominantDrop::None);

    let status = if seen == 0 {
        DiscoveryStatus::Idle
    } else if matched > 0 {
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
