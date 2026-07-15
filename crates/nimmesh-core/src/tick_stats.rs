//! # tick_stats — wedge-proof worker heartbeat diagnostics (the `ble ▸` playbook, for ticks)
//!
//! The 2026-07-15 field stall: the responder's funding re-verify froze at `attempt 1` for
//! 98+ s even though THREE independent drivers (the ~15 s keepalive `BeaconTick`, the ~3 s
//! `SwapFastTick`, and the proof-arrival path) all funnel into it — and every one of them is
//! sim-proven. Whatever fails on the device is invisible in the shipped build, because every
//! existing surface (the swap mirror, the verify note) is written BY the worker thread: a
//! wedged worker freezes the diagnostics along with the work, and dead timers look identical
//! to a dead session gate.
//!
//! This module makes the worker's job loop observable from OUTSIDE the worker: the loop
//! stamps lock-free atomics on [`WorkerCtx`](crate::engine) (via `ctx.citizen.ticks`) as each
//! job starts and finishes, and the FFI reads them directly — never through the worker. The
//! three failure modes now render differently on the swap sheet's `tick ▸` line:
//!
//! - **timers dead** (iOS suspension / lifecycle bug): `jobs_started` stops advancing — the
//!   last-started age grows with no wedge;
//! - **worker wedged** (a chain read hanging past its nominal timeout, a deadlock):
//!   `jobs_started == jobs_finished + 1` and `last_kind` names the stuck job while the
//!   started-age grows;
//! - **ticks healthy, session gate wrong**: both counters advance in lockstep while the
//!   verify note stays frozen — the bug is inside the session, not the plumbing.
//!
//! Diagnostics only: nothing reads these counters on any money path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::node::{Job, MeshNode};

/// Job-kind codes for `last_kind` (stable, webui-decoded; 0 = other/unmapped).
pub(crate) const KIND_INBOUND: u64 = 1;
pub(crate) const KIND_LOCAL_TX: u64 = 2;
pub(crate) const KIND_REQUEST_SYNC: u64 = 3;
pub(crate) const KIND_SYNC_TICK: u64 = 4;
pub(crate) const KIND_BEACON_TICK: u64 = 5;
pub(crate) const KIND_SWAP_FAST_TICK: u64 = 6;
pub(crate) const KIND_BALANCE_QUERY: u64 = 7;
pub(crate) const KIND_HISTORY_QUERY: u64 = 8;
pub(crate) const KIND_CHAT: u64 = 9;
pub(crate) const KIND_STOP_ADVERTISING: u64 = 10;

/// The job kind code for a queued [`Job`] (test-only variants read as 0/other).
pub(crate) fn kind_of(job: &Job) -> u64 {
    match job {
        Job::Inbound { .. } => KIND_INBOUND,
        Job::LocalTx(_) => KIND_LOCAL_TX,
        Job::RequestSync => KIND_REQUEST_SYNC,
        Job::SyncTick => KIND_SYNC_TICK,
        Job::BeaconTick => KIND_BEACON_TICK,
        Job::SwapFastTick => KIND_SWAP_FAST_TICK,
        Job::BalanceQuery(_) => KIND_BALANCE_QUERY,
        Job::HistoryQuery(_) => KIND_HISTORY_QUERY,
        Job::Chat(_) => KIND_CHAT,
        Job::StopAdvertising => KIND_STOP_ADVERTISING,
        _ => 0,
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lock-free worker-loop heartbeats, stamped by the worker and read by the FFI thread.
/// Lives on `WorkerCtx` (inside [`crate::citizen::CitizenState`] — the ctx's extensible
/// node-health slot; `engine.rs` sits at the 800-line cap and gains no lines this way).
#[derive(Debug, Default)]
pub struct TickStats {
    jobs_started: AtomicU64,
    jobs_finished: AtomicU64,
    last_kind: AtomicU64,
    last_started_ms: AtomicU64,
    last_finished_ms: AtomicU64,
}

impl TickStats {
    /// The worker is about to run `job` — stamp BEFORE the dispatch so a job that never
    /// returns (the wedge case) is visible as `started == finished + 1` with its kind.
    pub(crate) fn job_started(&self, job: &Job) {
        self.last_kind.store(kind_of(job), Ordering::Relaxed);
        self.last_started_ms.store(epoch_ms(), Ordering::Relaxed);
        self.jobs_started.fetch_add(1, Ordering::Relaxed);
    }

    /// The job's dispatch returned (its `catch_unwind` swallowed any panic).
    pub(crate) fn job_finished(&self) {
        self.last_finished_ms.store(epoch_ms(), Ordering::Relaxed);
        self.jobs_finished.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> FfiTickStats {
        FfiTickStats {
            jobs_started: self.jobs_started.load(Ordering::Relaxed),
            jobs_finished: self.jobs_finished.load(Ordering::Relaxed),
            last_kind: self.last_kind.load(Ordering::Relaxed),
            last_started_ms: self.last_started_ms.load(Ordering::Relaxed),
            last_finished_ms: self.last_finished_ms.load(Ordering::Relaxed),
            now_ms: epoch_ms(),
        }
    }
}

/// The worker heartbeat snapshot the app renders on the swap sheet's `tick ▸` line.
/// `now_ms` is stamped at READ time by the same clock as the stamps, so the UI computes
/// honest ages without trusting the phone's JS clock to match the process clock.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiTickStats {
    /// Jobs the worker loop has picked up since node construction.
    pub jobs_started: u64,
    /// Jobs whose dispatch returned. `started == finished + 1` ⇒ one is IN FLIGHT (or wedged).
    pub jobs_finished: u64,
    /// The kind code of the most recently started job (see the `KIND_*` consts; 0 = other).
    pub last_kind: u64,
    /// Epoch ms when the most recent job started; 0 = no job yet.
    pub last_started_ms: u64,
    /// Epoch ms when the most recent job finished; 0 = none yet.
    pub last_finished_ms: u64,
    /// Epoch ms at snapshot time (the reference the UI subtracts ages from).
    pub now_ms: u64,
}

#[uniffi::export]
impl MeshNode {
    /// The worker's live job-loop heartbeats — readable even when the worker itself is
    /// stuck, because the stamps are lock-free atomics on the shared ctx and this call
    /// never enqueues or blocks. Diagnostics only (the swap sheet's `tick ▸` line).
    pub fn tick_stats(&self) -> FfiTickStats {
        self.ctx.citizen.ticks.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wedged_job_reads_started_ahead_of_finished_with_its_kind() {
        let t = TickStats::default();
        t.job_started(&Job::BeaconTick);
        t.job_finished();
        t.job_started(&Job::SwapFastTick); // never finishes — the wedge shape
        let s = t.snapshot();
        assert_eq!(s.jobs_started, 2);
        assert_eq!(s.jobs_finished, 1);
        assert_eq!(s.last_kind, KIND_SWAP_FAST_TICK);
        assert!(s.last_started_ms > 0 && s.last_finished_ms > 0 && s.now_ms > 0);
    }

    #[test]
    fn kind_codes_cover_the_shipping_jobs() {
        assert_eq!(
            kind_of(&Job::Inbound {
                src: None,
                bytes: vec![]
            }),
            KIND_INBOUND
        );
        assert_eq!(kind_of(&Job::BeaconTick), KIND_BEACON_TICK);
        assert_eq!(kind_of(&Job::SwapFastTick), KIND_SWAP_FAST_TICK);
        assert_eq!(kind_of(&Job::RequestSync), KIND_REQUEST_SYNC);
        assert_eq!(kind_of(&Job::SyncTick), KIND_SYNC_TICK);
        assert_eq!(kind_of(&Job::StopAdvertising), KIND_STOP_ADVERTISING);
    }

    #[test]
    fn healthy_lockstep_reads_equal_counters() {
        let t = TickStats::default();
        for _ in 0..5 {
            t.job_started(&Job::BeaconTick);
            t.job_finished();
        }
        let s = t.snapshot();
        assert_eq!(s.jobs_started, 5);
        assert_eq!(s.jobs_finished, 5);
    }
}
