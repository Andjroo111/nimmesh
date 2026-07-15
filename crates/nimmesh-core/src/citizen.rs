//! # citizen — G20 good-citizen + battery-aware relay
//!
//! The mesh *is* its users: a payment only reaches the internet because someone nearby
//! relayed it. G20 turns that into two things the user can see and the device can respect:
//!
//! - **Good-citizen stats** — "you helped N payments reach the network." A small, honest
//!   participation counter ([`CitizenState::payments_relayed`]) bumped each time this node
//!   actually rebroadcasts a `nimiqTx` for someone else. It is the seed of the future
//!   incentive layer (G21), but here it is purely informational.
//! - **Battery-aware relay** — being a good citizen must **never cost the user**. The
//!   native shim reports the device battery ([`MeshNode::set_battery`]); the relay then
//!   adopts a [`RelayPosture`] that scales how much it carries for others: relay everything
//!   while charging / high battery, damp the dense-mesh fanout when mid, carry only the
//!   payment-critical traffic when low, and stop relaying entirely when critical (the user's
//!   own send/receive still work — those are not relays).
//!
//! All of this is **non-money-path**: it reads only the device battery and the packet's
//! public type, decides *whether/how much* to rebroadcast already-opaque bytes, and counts
//! what it carried. No keys, no signing, no payload inspection.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use crate::engine::{WorkerCtx, WorkerState};
use crate::packet::MessageType;

/// At/above this battery % (on battery power) the node relays at **full** participation.
/// Below 50 % the node starts protecting the user's charge.
pub const BATTERY_FULL_PCT: u8 = 50;
/// At/above this battery % (and below [`BATTERY_FULL_PCT`], on battery) the node relays at
/// reduced fanout; below it, only payment-critical traffic.
pub const BATTERY_LOW_PCT: u8 = 20;
/// Below this battery % (on battery) the node stops relaying for others entirely.
pub const BATTERY_CRITICAL_PCT: u8 = 10;

/// How much this node will carry for others right now, derived from its battery. Ordered
/// from most to least generous. FFI-visible so a UI can explain "relaying fully" vs
/// "saving battery."
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RelayPosture {
    /// Charging or high battery: relay every packet type at full fanout (best citizen).
    Full,
    /// Mid battery on battery power: relay every type, but damp the dense-mesh fanout.
    Reduced,
    /// Low battery on battery power: relay only payment-critical traffic (tx + receipt);
    /// drop opportunistic chatter (beacons, balance, memos, fragments, sync).
    Frugal,
    /// Critical battery: relay nothing for others. The user's own send/receive still work.
    Off,
}

impl RelayPosture {
    /// Whether this posture relays the given packet type at all. `Frugal` carries only the
    /// money path; `Off` carries nothing; the rest carry everything.
    pub fn relays_type(self, msg_type: MessageType) -> bool {
        match self {
            RelayPosture::Full | RelayPosture::Reduced => true,
            RelayPosture::Frugal => {
                matches!(msg_type, MessageType::NimiqTx | MessageType::NimiqTxReceipt)
            }
            RelayPosture::Off => false,
        }
    }

    /// The multiplier applied to the degree-adaptive relay probability. `Full`/`Frugal`
    /// carry their allowed types fully (`1.0`); `Reduced` halves the fanout; `Off` is `0.0`
    /// (and never consulted, since it relays no type).
    pub fn probability_factor(self) -> f64 {
        match self {
            RelayPosture::Full | RelayPosture::Frugal => 1.0,
            RelayPosture::Reduced => 0.5,
            RelayPosture::Off => 0.0,
        }
    }
}

/// Map a battery reading to a [`RelayPosture`]. **Charging always means `Full`** — a
/// plugged-in device should be the best citizen it can be. On battery power the posture
/// steps down with the charge: `Full` ≥ 50 %, `Reduced` ≥ 20 %, `Frugal` ≥ 10 %, else `Off`.
pub fn relay_posture(level_pct: u8, charging: bool) -> RelayPosture {
    if charging || level_pct >= BATTERY_FULL_PCT {
        RelayPosture::Full
    } else if level_pct >= BATTERY_LOW_PCT {
        RelayPosture::Reduced
    } else if level_pct >= BATTERY_CRITICAL_PCT {
        RelayPosture::Frugal
    } else {
        RelayPosture::Off
    }
}

/// The good-citizen + battery state a node carries (lives in [`WorkerCtx`]). Lock-free
/// atomics so the native shim can update the battery from any thread while the worker reads
/// it on the relay hot path. **Defaults to full participation** (100 %, charging) so a node
/// that never reports a battery — e.g. the simulator — behaves exactly as before G20.
pub struct CitizenState {
    /// Count of `nimiqTx` packets this node has relayed onward for others ("payments helped").
    payments_relayed: AtomicU64,
    /// Last reported battery level, 0..=100 %.
    battery_level: AtomicU8,
    /// Whether the device is currently charging.
    battery_charging: AtomicBool,
    /// Wedge-proof worker-loop heartbeats (`tick ▸` diagnostics) — parked here because this
    /// is the ctx's node-health slot and `engine.rs` sits at the 800-line cap.
    pub(crate) ticks: crate::tick_stats::TickStats,
}

impl Default for CitizenState {
    fn default() -> Self {
        Self::new()
    }
}

impl CitizenState {
    /// A fresh state: full battery + charging → [`RelayPosture::Full`] (no behaviour change
    /// until a real battery is reported).
    pub fn new() -> Self {
        CitizenState {
            payments_relayed: AtomicU64::new(0),
            battery_level: AtomicU8::new(100),
            battery_charging: AtomicBool::new(true),
            ticks: crate::tick_stats::TickStats::default(),
        }
    }

    /// Update the reported battery (clamped to 0..=100). Called by the native shim.
    pub fn set_battery(&self, level_pct: u8, charging: bool) {
        self.battery_level
            .store(level_pct.min(100), Ordering::Relaxed);
        self.battery_charging.store(charging, Ordering::Relaxed);
    }

    /// The current battery reading.
    pub fn battery(&self) -> BatteryState {
        BatteryState {
            level_pct: self.battery_level.load(Ordering::Relaxed),
            charging: self.battery_charging.load(Ordering::Relaxed),
        }
    }

    /// The relay posture for the current battery.
    pub fn posture(&self) -> RelayPosture {
        let b = self.battery();
        relay_posture(b.level_pct, b.charging)
    }

    /// Record that this node relayed `msg_type` onward; bumps the good-citizen "payments
    /// helped" counter only for an actual `nimiqTx` (the money the user carried for others).
    pub fn note_relay(&self, msg_type: MessageType) {
        if msg_type == MessageType::NimiqTx {
            self.payments_relayed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How many payments this node has helped reach the network.
    pub fn payments_relayed(&self) -> u64 {
        self.payments_relayed.load(Ordering::Relaxed)
    }
}

/// The device's last-reported battery (FFI-visible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct BatteryState {
    /// Battery level, 0..=100 %.
    pub level_pct: u8,
    /// Whether the device is charging.
    pub charging: bool,
}

/// Good-citizen relay stats for the UI ("you helped N payments reach the network").
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct RelayStats {
    /// `nimiqTx` packets relayed onward for others — "payments helped."
    pub payments_relayed: u64,
    /// Total packets (any type) relayed onward.
    pub packets_relayed: u64,
    /// The node's current relay posture (so the UI can show "relaying fully" / "saving battery").
    pub posture: RelayPosture,
}

/// The G20 relay gate, applied by [`crate::engine::relay_onward`] before a rebroadcast:
/// the battery posture must (1) carry this packet *type* and (2) win the (battery-damped)
/// degree-adaptive probability roll. Returns `false` (no relay) on a critical battery.
pub(crate) fn relay_allowed(ctx: &WorkerCtx, msg_type: MessageType, st: &mut WorkerState) -> bool {
    let posture = ctx.citizen.posture();
    if !posture.relays_type(msg_type) {
        return false;
    }
    st.policy
        .should_relay_throttled(ctx.peer_degree(), posture.probability_factor())
}

/// Build the FFI [`RelayStats`] snapshot from a node's context.
pub(crate) fn relay_stats(ctx: &WorkerCtx) -> RelayStats {
    RelayStats {
        payments_relayed: ctx.citizen.payments_relayed(),
        packets_relayed: ctx.forwarded.load(Ordering::Relaxed) as u64,
        posture: ctx.citizen.posture(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charging_is_always_full() {
        // Even at 1 % — plugged in means be the best citizen.
        assert_eq!(relay_posture(1, true), RelayPosture::Full);
        assert_eq!(relay_posture(100, true), RelayPosture::Full);
    }

    #[test]
    fn on_battery_posture_steps_down_with_charge() {
        assert_eq!(relay_posture(80, false), RelayPosture::Full);
        assert_eq!(relay_posture(50, false), RelayPosture::Full);
        assert_eq!(relay_posture(49, false), RelayPosture::Reduced);
        assert_eq!(relay_posture(20, false), RelayPosture::Reduced);
        assert_eq!(relay_posture(19, false), RelayPosture::Frugal);
        assert_eq!(relay_posture(10, false), RelayPosture::Frugal);
        assert_eq!(relay_posture(9, false), RelayPosture::Off);
        assert_eq!(relay_posture(0, false), RelayPosture::Off);
    }

    #[test]
    fn frugal_carries_only_the_money_path() {
        let p = RelayPosture::Frugal;
        assert!(p.relays_type(MessageType::NimiqTx));
        assert!(p.relays_type(MessageType::NimiqTxReceipt));
        // Opportunistic chatter is dropped to save the user's battery.
        assert!(!p.relays_type(MessageType::NimiqHeadBeacon));
        assert!(!p.relays_type(MessageType::NimiqBalanceResponse));
        assert!(!p.relays_type(MessageType::NoiseEncrypted));
    }

    #[test]
    fn off_carries_nothing_full_carries_everything() {
        for t in [
            MessageType::NimiqTx,
            MessageType::NimiqTxReceipt,
            MessageType::NimiqHeadBeacon,
            MessageType::NoiseEncrypted,
        ] {
            assert!(RelayPosture::Full.relays_type(t));
            assert!(!RelayPosture::Off.relays_type(t));
        }
    }

    #[test]
    fn probability_factors_match_the_posture() {
        assert_eq!(RelayPosture::Full.probability_factor(), 1.0);
        assert_eq!(RelayPosture::Frugal.probability_factor(), 1.0);
        assert_eq!(RelayPosture::Reduced.probability_factor(), 0.5);
        assert_eq!(RelayPosture::Off.probability_factor(), 0.0);
    }

    #[test]
    fn default_is_full_participation() {
        // The simulator / a node that never reports a battery behaves exactly as pre-G20.
        let c = CitizenState::new();
        assert_eq!(c.posture(), RelayPosture::Full);
        assert_eq!(
            c.battery(),
            BatteryState {
                level_pct: 100,
                charging: true
            }
        );
    }

    #[test]
    fn set_battery_changes_the_posture_and_clamps() {
        let c = CitizenState::new();
        c.set_battery(15, false);
        assert_eq!(c.posture(), RelayPosture::Frugal);
        c.set_battery(200, false); // clamped to 100
        assert_eq!(c.battery().level_pct, 100);
        assert_eq!(c.posture(), RelayPosture::Full);
    }

    #[test]
    fn only_a_nimiq_tx_counts_as_a_helped_payment() {
        let c = CitizenState::new();
        c.note_relay(MessageType::NimiqTx);
        c.note_relay(MessageType::NimiqTx);
        c.note_relay(MessageType::NimiqTxReceipt); // not a payment we originated help for
        c.note_relay(MessageType::NimiqHeadBeacon);
        assert_eq!(c.payments_relayed(), 2);
    }
}
