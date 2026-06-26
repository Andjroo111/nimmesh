//! # transport — shared mesh value types (ids + errors)
//!
//! G2 stood up a *temporary* broadcast transport seam here (`MeshTransport` +
//! `MockMeshTransport`) wrapped around an ad-hoc `MeshFrame` mock framing. **ADR-0002**
//! supersedes that: the byte-stream transport seam is now the native **`BleRadio`**
//! foreign trait (see [`crate::radio`]), driven by the Rust **[`crate::node::MeshNode`]**,
//! and the real **G4 packet codec** ([`crate::codec`]) replaces the mock framing on the
//! relay path. The radio-free virtual test substrate moved to [`crate::mock_radio`].
//!
//! What survives here is the small set of **shared value types** every seam still needs:
//! a transaction id ([`TxId`]) used as the receipt + dedup key, its deterministic mock
//! derivation ([`mock_tx_id`]), and the tiny [`MeshError`] surfaced by the provider seam.
//! None of it touches key material — only opaque, broadcast-safe references (core value #1).

use std::fmt;

/// Errors a mesh seam can surface. Deliberately tiny and `Clone`/`Eq` so tests and the
/// FFI layer can match on them without a heavyweight error crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeshError {
    /// The real provider ([`crate::provider::MeshProvider::real`]) was requested but its
    /// backing (G5 BLE radio + G8 RPC gateway) is not wired yet.
    NotStarted,
}

impl fmt::Display for MeshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeshError::NotStarted => write!(f, "mesh backing is not available yet"),
        }
    }
}

impl std::error::Error for MeshError {}

/// A transaction identity used as the receipt + dedup key.
///
/// In the real protocol this is the 32-byte Blake2b hash of the signed tx wire. The
/// mock derives it deterministically from the opaque payload via [`mock_tx_id`] so the
/// harness can key receipts and dedup without pulling a hash dependency.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxId(pub [u8; 32]);

impl fmt::Debug for TxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TxId(")?;
        for b in &self.0[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "..)")
    }
}

/// Derive a deterministic 32-byte id from an opaque payload (mock only).
///
/// G4/G8: the real `txId` is the 32-byte Blake2b hash of the canonical signed Nimiq tx
/// wire. This FNV-1a expansion is a stand-in so dedup + receipt keying work in the
/// harness; it is never used on the money path.
pub fn mock_tx_id(payload: &[u8]) -> TxId {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut h = FNV_OFFSET;
    for b in payload {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h ^= payload.len() as u64;
    h = h.wrapping_mul(FNV_PRIME);

    let mut out = [0u8; 32];
    for (chunk, slot) in out.chunks_mut(8).enumerate() {
        h = h.wrapping_mul(FNV_PRIME) ^ (chunk as u64 + 1);
        slot.copy_from_slice(&h.to_be_bytes());
    }
    TxId(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_tx_id_is_deterministic_and_payload_sensitive() {
        assert_eq!(mock_tx_id(b"abc"), mock_tx_id(b"abc"));
        assert_ne!(mock_tx_id(b"abc"), mock_tx_id(b"abd"));
        // Even the empty payload yields a non-zero id (FNV is seeded).
        assert_ne!(mock_tx_id(b""), TxId([0u8; 32]));
    }

    #[test]
    fn tx_id_debug_is_truncated_hex() {
        let id = TxId([0xAB; 32]);
        assert_eq!(format!("{id:?}"), "TxId(abababab..)");
    }

    #[test]
    fn mesh_error_displays() {
        assert_eq!(
            MeshError::NotStarted.to_string(),
            "mesh backing is not available yet"
        );
    }
}
