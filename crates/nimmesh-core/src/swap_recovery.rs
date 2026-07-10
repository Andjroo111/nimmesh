//! # swap_recovery — crash recovery of in-flight swaps (G31)
//!
//! A [`crate::swap_session::SwapSession`] lives only in memory, so a node that crashes with funds
//! locked would lose its coordinator and, with it, the refund path. This module gives the session a
//! [`snapshot`](SwapSession::snapshot) / [`restore`](SwapSession::restore) pair: the node persists the
//! per-swap [`CoordinatorSnapshot`]s and rebuilds the session on restart, so the refund tick re-arms
//! and "worst case is a refund" holds across a crash, not just within one process.
//!
//! The snapshot carries the initiator secret (it must, to rebuild an initiator). The node is
//! responsible for storing it as securely as a key; `CoordinatorSnapshot` has no `Debug` so it is
//! never logged. The pending retransmit buffer is intentionally not captured: a restarted node
//! re-derives it as the swap advances, and the refund/GC tick needs only the coordinators.

use crate::swap::{LadderParams, SwapPhase, SwapRole, SwapTerms};
use crate::swap_coordinator::{CoordinatorSnapshot, SwapContext, SwapCoordinator};
use crate::swap_session::{NodeIdentity, SwapSession};

impl SwapSession {
    /// Capture every in-flight swap for crash recovery. The node persists these (securely — they
    /// carry the initiator secret) and feeds them back to [`restore`](Self::restore) on restart.
    pub fn snapshot(&self) -> Vec<CoordinatorSnapshot> {
        self.coordinators
            .values()
            .map(|c| c.to_snapshot())
            .collect()
    }

    /// Rebuild a session from `identity` + `ladder` + a persisted snapshot, so its refund/GC tick
    /// resumes after a restart. The rate/concurrency policies come from `identity`; the pending
    /// retransmit buffer starts empty (re-derived as swaps advance).
    pub fn restore(
        identity: NodeIdentity,
        ladder: LadderParams,
        snapshot: Vec<CoordinatorSnapshot>,
    ) -> Self {
        let mut session = SwapSession::new(identity, ladder);
        for snap in snapshot {
            let swap_id = snap.ctx.swap_id;
            // #189: a restored INITIATOR swap means this node already issued that swap's
            // secret — tombstone the id so a post-restart re-match can never reissue it.
            if snap.role == crate::swap::SwapRole::Initiator {
                session.initiated_ever.insert(swap_id);
            }
            session
                .coordinators
                .insert(swap_id, SwapCoordinator::from_snapshot(snap, ladder));
        }
        session
    }

    /// G32: the whole session's recovery state as **bytes** the node writes to disk (the persistent
    /// form of [`snapshot`](Self::snapshot)). Restore with [`restore_bytes`](Self::restore_bytes).
    ///
    /// H2 (#189): the blob ALSO carries every `initiated_ever` tombstone — a SETTLED initiator
    /// swap is reaped from the coordinator map before a snapshot, so without the trailer a
    /// settle → reap → restart would forget the id and the deterministic secret source could
    /// reissue its now-PUBLIC `S` to a re-derived swap. Tombstones are 16-byte ids only.
    pub fn encode_snapshot(&self) -> Vec<u8> {
        let mut out = encode_snapshot(&self.snapshot());
        let mut ids: Vec<[u8; 16]> = self.initiated_ever.iter().copied().collect();
        ids.sort_unstable(); // deterministic bytes
        out.extend_from_slice(&(ids.len() as u16).to_be_bytes());
        for id in &ids {
            out.extend_from_slice(id);
        }
        out
    }

    /// G32: rebuild a session from persisted bytes. `Err` (never a panic) on truncated/malformed
    /// input — a node with a corrupt recovery file starts clean rather than crashing.
    pub fn restore_bytes(
        identity: NodeIdentity,
        ladder: LadderParams,
        bytes: &[u8],
    ) -> Result<Self, SnapshotError> {
        let (snaps, tombstones) = decode_snapshot_with_tombstones(bytes)?;
        let mut session = SwapSession::restore(identity, ladder, snaps);
        // H2: re-arm every persisted tombstone, including those whose coordinator was
        // already reaped (the settle → reap → restart hole the review flagged).
        session.initiated_ever.extend(tombstones);
        Ok(session)
    }
}

/// A snapshot-decoding failure. Carries no payload (so a corrupt blob, secret and all, is never
/// surfaced in an error / log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotError {
    /// The bytes ended mid-record.
    Truncated,
    /// A field held a value outside its domain (bad role/phase tag).
    Malformed,
}

/// G32: encode a list of snapshots to bytes — `u16` count, then each record sequentially. The secret
/// is just opaque bytes in the blob (the node encrypts the file at rest; nothing here logs it).
pub fn encode_snapshot(snaps: &[CoordinatorSnapshot]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(snaps.len() as u16).to_be_bytes());
    for s in snaps {
        out.push(role_tag(s.role));
        out.push(phase_tag(s.phase));
        out.extend_from_slice(&s.ctx.swap_id);
        out.extend_from_slice(&s.ctx.terms.nim_timeout.to_be_bytes());
        out.extend_from_slice(&s.ctx.terms.counterparty_timeout.to_be_bytes());
        out.extend_from_slice(&s.ctx.hashlock);
        out.extend_from_slice(&s.ctx.nim_address);
        out.extend_from_slice(&s.ctx.btc_pubkey);
        out.extend_from_slice(&s.ctx.give_amount.to_be_bytes());
        out.extend_from_slice(&s.ctx.take_amount.to_be_bytes());
        out.extend_from_slice(&s.ctx.term_anchor.to_be_bytes());
        out.push(s.ctx.network_id);
        out.extend_from_slice(&(s.ctx.btc_address.len() as u16).to_be_bytes());
        out.extend_from_slice(&s.ctx.btc_address);
        match s.secret {
            Some(sec) => {
                out.push(1);
                out.extend_from_slice(&sec);
            }
            None => out.push(0),
        }
        match s.peer_btc_pubkey {
            Some(pk) => {
                out.push(1);
                out.extend_from_slice(&pk);
            }
            None => out.push(0),
        }
    }
    out
}

/// G32: decode snapshots from bytes. Panic-free on arbitrary input (every read is bounds-checked); a
/// huge length field can't allocate because the slice read fails first.
pub fn decode_snapshot(bytes: &[u8]) -> Result<Vec<CoordinatorSnapshot>, SnapshotError> {
    decode_snapshot_with_tombstones(bytes).map(|(snaps, _)| snaps)
}

/// H2: decode snapshots + the `initiated_ever` tombstone trailer. A blob written before the
/// trailer existed simply ends after the records — that reads as zero tombstones (backward
/// compatible); a PRESENT-but-truncated trailer is a hard `Truncated` (never silently drop a
/// tombstone).
pub fn decode_snapshot_with_tombstones(
    bytes: &[u8],
) -> Result<(Vec<CoordinatorSnapshot>, Vec<[u8; 16]>), SnapshotError> {
    let mut r = Reader { buf: bytes, pos: 0 };
    let count = r.u16().ok_or(SnapshotError::Truncated)?;
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(decode_one(&mut r)?);
    }
    let mut tombstones = Vec::new();
    if let Some(tcount) = r.u16() {
        for _ in 0..tcount {
            tombstones.push(r.arr::<16>().ok_or(SnapshotError::Truncated)?);
        }
    }
    Ok((out, tombstones))
}

fn decode_one(r: &mut Reader) -> Result<CoordinatorSnapshot, SnapshotError> {
    let role = role_from_tag(r.u8().ok_or(SnapshotError::Truncated)?)?;
    let phase = phase_from_tag(r.u8().ok_or(SnapshotError::Truncated)?)?;
    let t = SnapshotError::Truncated;
    let swap_id = r.arr::<16>().ok_or(t)?;
    let nim_timeout = r.u64().ok_or(t)?;
    let counterparty_timeout = r.u64().ok_or(t)?;
    let hashlock = r.arr::<32>().ok_or(t)?;
    let nim_address = r.arr::<20>().ok_or(t)?;
    let btc_pubkey = r.arr::<33>().ok_or(t)?;
    let give_amount = r.u64().ok_or(t)?;
    let take_amount = r.u64().ok_or(t)?;
    let term_anchor = r.u64().ok_or(t)?;
    let network_id = r.u8().ok_or(t)?;
    let addr_len = r.u16().ok_or(t)? as usize;
    let btc_address = r.take(addr_len).ok_or(t)?.to_vec();
    let secret = if r.u8().ok_or(t)? == 1 {
        Some(r.arr::<32>().ok_or(t)?)
    } else {
        None
    };
    let peer_btc_pubkey = if r.u8().ok_or(t)? == 1 {
        Some(r.arr::<33>().ok_or(t)?)
    } else {
        None
    };
    Ok(CoordinatorSnapshot {
        role,
        phase,
        ctx: SwapContext {
            swap_id,
            terms: SwapTerms {
                nim_timeout,
                counterparty_timeout,
            },
            hashlock,
            nim_address,
            btc_address,
            btc_pubkey,
            give_amount,
            take_amount,
            network_id,
            term_anchor,
        },
        secret,
        peer_btc_pubkey,
    })
}

/// A bounds-checked byte cursor — every read returns `None` past the end (so decode is panic-free).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|s| u64::from_be_bytes(s.try_into().unwrap()))
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N).map(|s| s.try_into().unwrap())
    }
}

fn role_tag(r: SwapRole) -> u8 {
    match r {
        SwapRole::Initiator => 0,
        SwapRole::Responder => 1,
    }
}

fn role_from_tag(t: u8) -> Result<SwapRole, SnapshotError> {
    match t {
        0 => Ok(SwapRole::Initiator),
        1 => Ok(SwapRole::Responder),
        _ => Err(SnapshotError::Malformed),
    }
}

fn phase_tag(p: SwapPhase) -> u8 {
    match p {
        SwapPhase::Proposed => 0,
        SwapPhase::Accepted => 1,
        SwapPhase::InitiatorFunded => 2,
        SwapPhase::SelfFunded => 3,
        SwapPhase::BothFunded => 4,
        SwapPhase::Revealed => 5,
        SwapPhase::Settled => 6,
        SwapPhase::Aborted => 7,
        SwapPhase::Refunded => 8,
    }
}

fn phase_from_tag(t: u8) -> Result<SwapPhase, SnapshotError> {
    Ok(match t {
        0 => SwapPhase::Proposed,
        1 => SwapPhase::Accepted,
        2 => SwapPhase::InitiatorFunded,
        3 => SwapPhase::SelfFunded,
        4 => SwapPhase::BothFunded,
        5 => SwapPhase::Revealed,
        6 => SwapPhase::Settled,
        7 => SwapPhase::Aborted,
        8 => SwapPhase::Refunded,
        _ => return Err(SnapshotError::Malformed),
    })
}

#[cfg(test)]
mod tests {
    use crate::swap::{LadderParams, SwapPhase, SwapTerms};
    use crate::swap_coordinator::{SwapContext, SwapCoordinator};
    use crate::swap_leg::sha256;
    use crate::swap_messages::SwapAcceptance;
    use crate::swap_session::{
        NodeIdentity, RatePolicy, SwapSession, DEFAULT_MAX_CONCURRENT_SWAPS,
    };

    fn identity() -> NodeIdentity {
        let mut pk = [0x22; 33];
        pk[0] = 0x02;
        NodeIdentity {
            nim_address: [0xB2; 20],
            btc_address: b"tb1qbob".to_vec(),
            btc_pubkey: pk,
            rate_policy: RatePolicy::accept_all(),
            max_concurrent_swaps: DEFAULT_MAX_CONCURRENT_SWAPS,
            standing_intent: None,
        }
    }

    /// A session holding one initiator swap driven to `SelfFunded` (the NIM leg is locked).
    fn funds_locked_session(swap_id: [u8; 16], secret: [u8; 32]) -> SwapSession {
        let ladder = LadderParams::default();
        let mut alice_pk = [0x11; 33];
        alice_pk[0] = 0x02;
        let ctx = SwapContext {
            swap_id,
            terms: SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            hashlock: sha256(&secret),
            nim_address: [0xA1; 20],
            btc_address: b"tb1qalice".to_vec(),
            btc_pubkey: alice_pk,
            give_amount: 100_000,
            take_amount: 50_000,
            network_id: 5,
            term_anchor: 0,
        };
        let (mut coord, _propose) = SwapCoordinator::new_initiator(ctx, secret, ladder);
        let accept = SwapAcceptance {
            swap_id,
            nim_address: [0xB2; 20],
            btc_address: b"tb1qbob".to_vec(),
            btc_pubkey: [0x03; 33],
        }
        .to_envelope();
        coord.recv_accept(&accept, 0).unwrap();
        coord.fund(0, vec![0x11; 248], [0xC1; 32]).unwrap();
        assert!(coord.phase().has_funds_locked());
        let mut s = SwapSession::new(identity(), ladder);
        s.add_initiator(swap_id, coord);
        s
    }

    #[test]
    fn a_funds_locked_swap_survives_a_snapshot_restore_and_still_refunds() {
        // G31: a node funds its NIM leg, then "crashes". Its session is snapshotted, dropped, and
        // restored — and the restored coordinator still refunds past its timeout. "Worst case is a
        // refund" survives the restart, not just one process.
        let swap_id = [0x5A; 16];
        let before = funds_locked_session(swap_id, [42u8; 32]);
        let snapshot = before.snapshot();
        assert_eq!(snapshot.len(), 1);
        drop(before); // the crash

        let mut after = SwapSession::restore(identity(), LadderParams::default(), snapshot);
        assert!(after
            .coordinator(&swap_id)
            .unwrap()
            .phase()
            .has_funds_locked());

        assert_eq!(
            after
                .coordinator(&swap_id)
                .unwrap()
                .refund_after_timeout(10_001),
            Ok(())
        );
        assert_eq!(
            after.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::Refunded
        );
        after.tick(10_001);
        assert!(after.coordinator(&swap_id).is_none());
    }

    #[test]
    fn the_worker_gc_tick_refunds_a_restored_funds_locked_swap() {
        // The same, but let the GC/refund tick drive it — exactly what a restarted node's tick does.
        let swap_id = [0x5B; 16];
        let before = funds_locked_session(swap_id, [7u8; 32]);
        let mut after =
            SwapSession::restore(identity(), LadderParams::default(), before.snapshot());

        // Kept before the timeout (no premature refund); refunded + reaped past it.
        assert!(after.tick(5_000).is_empty());
        assert_eq!(after.len(), 1);
        after.tick(10_001);
        assert!(after.is_empty());
    }

    #[test]
    fn the_byte_codec_round_trips_a_funds_locked_session_and_it_still_refunds() {
        // G32: persist the session to BYTES (the on-disk form), then restart from those bytes — the
        // funds-locked swap is back and still refunds past the timeout.
        let swap_id = [0x5C; 16];
        let bytes = funds_locked_session(swap_id, [42u8; 32]).encode_snapshot();
        let mut after =
            SwapSession::restore_bytes(identity(), LadderParams::default(), &bytes).unwrap();
        assert!(after
            .coordinator(&swap_id)
            .unwrap()
            .phase()
            .has_funds_locked());
        after.tick(10_001);
        assert!(after.is_empty());
    }

    #[test]
    fn a_settled_reaped_initiator_swap_stays_tombstoned_across_restart() {
        // H2 (#189): a SETTLED initiator swap is reaped from the coordinator map before a
        // snapshot, so the tombstone must ride the snapshot bytes themselves — otherwise a
        // settle → reap → restart forgets the id and the deterministic secret source could
        // reissue its now-PUBLIC `S` to a re-derived swap.
        use crate::swap_messages::tx_envelope;
        use crate::swap_wire::SwapLegId;
        let swap_id = [0x5D; 16];
        let mut s = funds_locked_session(swap_id, [42u8; 32]);
        {
            let c = s.coordinator(&swap_id).unwrap();
            c.recv_funding_proof(&tx_envelope(
                swap_id,
                SwapLegId::Counterparty,
                vec![0x22; 120],
                [0xF2; 32],
            ))
            .unwrap();
            c.claim_and_reveal(0, vec![0x42; 32], [0xF3; 32]).unwrap();
            c.settle().unwrap();
        }
        s.tick(0); // the settled coordinator is reaped — only the tombstone remains
        assert!(s.is_empty());
        assert!(s.has_initiated(&swap_id));

        let bytes = s.encode_snapshot();
        let restored =
            SwapSession::restore_bytes(identity(), LadderParams::default(), &bytes).unwrap();
        assert!(restored.is_empty());
        assert!(
            restored.has_initiated(&swap_id),
            "the tombstone must survive settle → reap → restart"
        );
    }

    #[test]
    fn a_pre_trailer_blob_still_restores_and_a_torn_trailer_is_refused() {
        // Backward compat: bytes written before the tombstone trailer existed (records
        // only) still restore — the in-flight initiator swap re-tombstones off its record.
        let swap_id = [0x5E; 16];
        let s = funds_locked_session(swap_id, [42u8; 32]);
        let old_bytes = super::encode_snapshot(&s.snapshot()); // record bytes, no trailer
        let mut restored =
            SwapSession::restore_bytes(identity(), LadderParams::default(), &old_bytes).unwrap();
        assert!(restored.has_initiated(&swap_id));
        assert!(restored.coordinator(&swap_id).is_some());

        // A trailer that CLAIMS tombstones but ends short is a hard error — never silently
        // drop a tombstone.
        let mut torn = s.encode_snapshot();
        torn.truncate(torn.len() - 1);
        assert!(matches!(
            SwapSession::restore_bytes(identity(), LadderParams::default(), &torn),
            Err(super::SnapshotError::Truncated)
        ));
    }

    #[test]
    fn decode_rejects_truncated_and_malformed_bytes_without_panicking() {
        use super::{decode_snapshot, SnapshotError};
        // `matches!` (not assert_eq) because the Ok payload deliberately has no Debug (it holds the
        // secret). Empty / too short for the count → Truncated.
        assert!(matches!(
            decode_snapshot(&[]),
            Err(SnapshotError::Truncated)
        ));
        assert!(matches!(
            decode_snapshot(&[0x00]),
            Err(SnapshotError::Truncated)
        ));
        // Claims one record but no bytes follow → Truncated.
        assert!(matches!(
            decode_snapshot(&[0x00, 0x01]),
            Err(SnapshotError::Truncated)
        ));
        // One record with a bad role tag → Malformed.
        assert!(matches!(
            decode_snapshot(&[0x00, 0x01, 0xFF]),
            Err(SnapshotError::Malformed)
        ));
        // A zero-count blob is a valid empty session.
        assert!(matches!(decode_snapshot(&[0x00, 0x00]), Ok(v) if v.is_empty()));
    }
}
