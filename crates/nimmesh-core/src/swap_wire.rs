//! # Swap wire envelope (mesh swap F2)
//!
//! The Bitchat-style **type-length-value** payload carried inside the swap packets
//! (`0x40`–`0x44`, see [`crate::packet::MessageType`]) that negotiate and settle an offline
//! cross-chain HTLC atomic swap (`docs/swap/SWAP.md`). Mirrors [`crate::envelope`]: each field
//! is `1B type | 1B len | value`, so a value is at most 255 bytes (a signed funding/claim tx is
//! ≤ ~248 B — it fits in one TLV).
//!
//! One [`SwapEnvelope`] struct carries every field; which are **required** depends on the
//! [`SwapKind`] (mirrored from the packet's `MessageType`). All payloads here are
//! broadcast-safe / public: addresses, a hashlock, block-height timeouts, and **opaque** signed
//! tx blobs. **No key material, no preimage-before-reveal, nothing money-path** — the preimage
//! only ever appears inside a signed claim tx that is meant to go on-chain (LOOP guardrails).
//!
//! ## Strictness
//!
//! [`decode_swap`] is panic-free over arbitrary input (property-tested): every TLV is
//! bounds-checked, fixed-width fields must carry their exact length, unknown TLV types are
//! skipped for forward-compat, and the per-kind required fields must be present.

use std::fmt;

use crate::packet::MessageType;

/// Length of a swap id in bytes (a random per-swap correlator).
pub const SWAP_ID_LEN: usize = 16;
/// Length of a hashlock / txId hash in bytes.
pub const HASH_LEN: usize = 32;
/// Length of a raw Nimiq address in bytes.
pub const NIM_ADDRESS_LEN: usize = 20;
/// Maximum length of any single TLV value (the `len` field is one byte).
pub const MAX_TLV_VALUE_LEN: usize = u8::MAX as usize;

// --- TLV type bytes -------------------------------------------------------------------------
const T_SWAP_ID: u8 = 0x01;
const T_HASHLOCK: u8 = 0x02;
const T_GIVE_AMOUNT: u8 = 0x03;
const T_TAKE_AMOUNT: u8 = 0x04;
const T_NIM_TIMEOUT: u8 = 0x05;
const T_COUNTERPARTY_TIMEOUT: u8 = 0x06;
const T_NIM_ADDRESS: u8 = 0x07;
const T_COUNTERPARTY_ADDRESS: u8 = 0x08;
const T_LEG: u8 = 0x09;
const T_TX_WIRE: u8 = 0x0A;
const T_TX_ID: u8 = 0x0B;
const T_NETWORK_ID: u8 = 0x0C;
const T_REASON: u8 = 0x0D;

/// Which chain a leg / message refers to. The swap state machine and the mesh stay
/// chain-agnostic; this byte is the only place the pair is named on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SwapLegId {
    /// The Nimiq leg (the native-HTLC side we build now).
    Nim = 0x00,
    /// The counterparty leg (Bitcoin first; pluggable — see `docs/swap/SWAP.md`).
    Counterparty = 0x01,
}

impl SwapLegId {
    /// The on-wire byte.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }
    /// Parse a leg byte.
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(SwapLegId::Nim),
            0x01 => Some(SwapLegId::Counterparty),
            _ => None,
        }
    }
}

/// The kind of swap message, mirrored from the packet [`MessageType`] (`0x40`–`0x44`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapKind {
    /// `0x40` — initiator's proposed terms.
    Propose,
    /// `0x41` — responder's acceptance + addresses.
    Accept,
    /// `0x42` — a funded HTLC's signed tx + txId.
    FundingProof,
    /// `0x43` — the signed claim tx that reveals the preimage on-chain.
    PreimageReveal,
    /// `0x44` — a pre-funding courtesy abort.
    Abort,
}

impl SwapKind {
    /// The packet message type that carries this swap kind.
    pub const fn message_type(self) -> MessageType {
        match self {
            SwapKind::Propose => MessageType::SwapPropose,
            SwapKind::Accept => MessageType::SwapAccept,
            SwapKind::FundingProof => MessageType::SwapFundingProof,
            SwapKind::PreimageReveal => MessageType::SwapPreimageReveal,
            SwapKind::Abort => MessageType::SwapAbort,
        }
    }

    /// The swap kind a packet message type denotes, if any.
    pub const fn from_message_type(mt: MessageType) -> Option<Self> {
        match mt {
            MessageType::SwapPropose => Some(SwapKind::Propose),
            MessageType::SwapAccept => Some(SwapKind::Accept),
            MessageType::SwapFundingProof => Some(SwapKind::FundingProof),
            MessageType::SwapPreimageReveal => Some(SwapKind::PreimageReveal),
            MessageType::SwapAbort => Some(SwapKind::Abort),
            _ => None,
        }
    }
}

/// A decoded swap message. One struct for every kind; [`SwapKind`] selects which fields are
/// required (see [`required_fields`]). All fields are public, broadcast-safe data.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwapEnvelope {
    /// `0x01` — the 16-byte per-swap correlator (required for every kind).
    pub swap_id: [u8; SWAP_ID_LEN],
    /// `0x02` — `H = hash(secret)`, the shared hashlock (propose).
    pub hashlock: Option<[u8; HASH_LEN]>,
    /// `0x03` — amount the initiator **gives** on its leg, in that chain's base unit (propose).
    pub give_amount: Option<u64>,
    /// `0x04` — amount the initiator **wants** on the counterparty leg (propose).
    pub take_amount: Option<u64>,
    /// `0x05` — the proposed NIM-leg HTLC timeout, a **block height** `T_A` (propose/accept).
    pub nim_timeout: Option<u64>,
    /// `0x06` — the proposed counterparty-leg timeout `T_B` (`T_B < T_A`; propose/accept).
    pub counterparty_timeout: Option<u64>,
    /// `0x07` — a 20-byte Nimiq address: the proposer's NIM refund / responder's NIM claim addr.
    pub nim_address: Option<[u8; NIM_ADDRESS_LEN]>,
    /// `0x08` — the counterparty-chain address bytes (chain-agnostic; e.g. a BTC address).
    pub counterparty_address: Option<Vec<u8>>,
    /// `0x09` — which leg a funding/reveal message refers to (funding/reveal).
    pub leg: Option<SwapLegId>,
    /// `0x0A` — the **opaque** signed funding/claim tx blob, broadcast verbatim (funding/reveal).
    pub tx_wire: Option<Vec<u8>>,
    /// `0x0B` — the 32-byte txId of `tx_wire` (funding/reveal; dedup + settlement key).
    pub tx_id: Option<[u8; HASH_LEN]>,
    /// `0x0C` — the Albatross network id for the NIM leg (propose/funding). Default testnet.
    pub network_id: Option<u8>,
    /// `0x0D` — a 1-byte abort reason code (abort).
    pub reason: Option<u8>,
}

impl SwapEnvelope {
    /// A bare envelope with just the swap id set (build it up field by field).
    pub fn new(swap_id: [u8; SWAP_ID_LEN]) -> Self {
        SwapEnvelope {
            swap_id,
            ..Default::default()
        }
    }
}

/// The TLV types that **must** be present for a given [`SwapKind`] (besides `swap_id`, which is
/// always required). Decode enforces these; this is also the single source of truth a builder
/// can check against before sending.
pub fn required_fields(kind: SwapKind) -> &'static [u8] {
    match kind {
        SwapKind::Propose => &[
            T_HASHLOCK,
            T_GIVE_AMOUNT,
            T_TAKE_AMOUNT,
            T_NIM_TIMEOUT,
            T_COUNTERPARTY_TIMEOUT,
            T_NIM_ADDRESS,
            T_COUNTERPARTY_ADDRESS,
            T_NETWORK_ID,
        ],
        // The responder supplies the addresses its side will use.
        SwapKind::Accept => &[T_NIM_ADDRESS, T_COUNTERPARTY_ADDRESS],
        SwapKind::FundingProof => &[T_LEG, T_TX_WIRE, T_TX_ID],
        SwapKind::PreimageReveal => &[T_LEG, T_TX_WIRE, T_TX_ID],
        SwapKind::Abort => &[T_REASON],
    }
}

/// An error from [`encode_swap`] or [`decode_swap`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapWireError {
    /// `encode`: a TLV value is longer than the 1-byte `len` field allows.
    ValueTooLong {
        /// The TLV type whose value overflowed.
        tlv_type: u8,
        /// The offending value length.
        len: usize,
    },
    /// `decode`: a TLV header or value runs past the end of the buffer.
    Truncated {
        /// Byte offset at which the read ran out of input.
        offset: usize,
    },
    /// `decode`: a fixed-width field carried the wrong length.
    BadFieldLength {
        /// The TLV type with the wrong length.
        tlv_type: u8,
        /// The length seen on the wire.
        len: usize,
    },
    /// `decode`: a field carried an out-of-range enum value (e.g. an unknown leg byte).
    BadFieldValue {
        /// The TLV type with the bad value.
        tlv_type: u8,
    },
    /// `decode`: a field required for this [`SwapKind`] was absent.
    MissingRequiredField {
        /// The TLV type that was required but not present.
        tlv_type: u8,
    },
}

impl fmt::Display for SwapWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwapWireError::ValueTooLong { tlv_type, len } => write!(
                f,
                "swap TLV 0x{tlv_type:02x} value of {len} bytes exceeds the {MAX_TLV_VALUE_LEN}-byte max"
            ),
            SwapWireError::Truncated { offset } => {
                write!(f, "truncated swap TLV stream at offset {offset}")
            }
            SwapWireError::BadFieldLength { tlv_type, len } => {
                write!(f, "swap TLV 0x{tlv_type:02x} has invalid length {len}")
            }
            SwapWireError::BadFieldValue { tlv_type } => {
                write!(f, "swap TLV 0x{tlv_type:02x} has an out-of-range value")
            }
            SwapWireError::MissingRequiredField { tlv_type } => {
                write!(f, "required swap TLV 0x{tlv_type:02x} is missing")
            }
        }
    }
}

impl std::error::Error for SwapWireError {}

/// Append one `type | len | value` TLV, erroring if the value is too long.
fn push_tlv(buf: &mut Vec<u8>, tlv_type: u8, value: &[u8]) -> Result<(), SwapWireError> {
    if value.len() > MAX_TLV_VALUE_LEN {
        return Err(SwapWireError::ValueTooLong {
            tlv_type,
            len: value.len(),
        });
    }
    buf.push(tlv_type);
    buf.push(value.len() as u8);
    buf.extend_from_slice(value);
    Ok(())
}

/// Serialize a [`SwapEnvelope`] to its TLV payload bytes, in ascending type order. Only the
/// fields that are set are emitted; callers should ensure the [`required_fields`] for the kind
/// they are sending are populated (decode enforces this on the far side).
pub fn encode_swap(env: &SwapEnvelope) -> Result<Vec<u8>, SwapWireError> {
    let mut buf = Vec::new();
    push_tlv(&mut buf, T_SWAP_ID, &env.swap_id)?;
    if let Some(h) = &env.hashlock {
        push_tlv(&mut buf, T_HASHLOCK, h)?;
    }
    if let Some(v) = env.give_amount {
        push_tlv(&mut buf, T_GIVE_AMOUNT, &v.to_be_bytes())?;
    }
    if let Some(v) = env.take_amount {
        push_tlv(&mut buf, T_TAKE_AMOUNT, &v.to_be_bytes())?;
    }
    if let Some(v) = env.nim_timeout {
        push_tlv(&mut buf, T_NIM_TIMEOUT, &v.to_be_bytes())?;
    }
    if let Some(v) = env.counterparty_timeout {
        push_tlv(&mut buf, T_COUNTERPARTY_TIMEOUT, &v.to_be_bytes())?;
    }
    if let Some(a) = &env.nim_address {
        push_tlv(&mut buf, T_NIM_ADDRESS, a)?;
    }
    if let Some(a) = &env.counterparty_address {
        push_tlv(&mut buf, T_COUNTERPARTY_ADDRESS, a)?;
    }
    if let Some(l) = env.leg {
        push_tlv(&mut buf, T_LEG, &[l.to_u8()])?;
    }
    if let Some(w) = &env.tx_wire {
        push_tlv(&mut buf, T_TX_WIRE, w)?;
    }
    if let Some(id) = &env.tx_id {
        push_tlv(&mut buf, T_TX_ID, id)?;
    }
    if let Some(n) = env.network_id {
        push_tlv(&mut buf, T_NETWORK_ID, &[n])?;
    }
    if let Some(r) = env.reason {
        push_tlv(&mut buf, T_REASON, &[r])?;
    }
    Ok(buf)
}

/// Read an 8-byte big-endian u64 from a fixed-width TLV value.
fn read_u64(value: &[u8], tlv_type: u8) -> Result<u64, SwapWireError> {
    let arr: [u8; 8] = value
        .try_into()
        .map_err(|_| SwapWireError::BadFieldLength {
            tlv_type,
            len: value.len(),
        })?;
    Ok(u64::from_be_bytes(arr))
}

/// Parse a TLV stream back into a [`SwapEnvelope`], enforcing the [`required_fields`] for `kind`.
///
/// Panic-free and strict: each TLV is bounds-checked, fixed-width fields must carry their exact
/// length, enum fields must be in range, unknown TLV types are skipped (forward-compat), and the
/// kind's required fields (plus `swap_id`) must be present.
pub fn decode_swap(kind: SwapKind, bytes: &[u8]) -> Result<SwapEnvelope, SwapWireError> {
    let mut env = SwapEnvelope::default();
    let mut seen_swap_id = false;
    let mut present: Vec<u8> = Vec::new();

    let mut off = 0usize;
    while off < bytes.len() {
        if off + 2 > bytes.len() {
            return Err(SwapWireError::Truncated { offset: off });
        }
        let tlv_type = bytes[off];
        let len = bytes[off + 1] as usize;
        let value_start = off + 2;
        let value_end = value_start
            .checked_add(len)
            .ok_or(SwapWireError::Truncated {
                offset: value_start,
            })?;
        if value_end > bytes.len() {
            return Err(SwapWireError::Truncated {
                offset: value_start,
            });
        }
        let value = &bytes[value_start..value_end];
        present.push(tlv_type);

        match tlv_type {
            T_SWAP_ID => {
                env.swap_id = value
                    .try_into()
                    .map_err(|_| SwapWireError::BadFieldLength { tlv_type, len })?;
                seen_swap_id = true;
            }
            T_HASHLOCK => {
                env.hashlock = Some(
                    value
                        .try_into()
                        .map_err(|_| SwapWireError::BadFieldLength { tlv_type, len })?,
                );
            }
            T_GIVE_AMOUNT => env.give_amount = Some(read_u64(value, tlv_type)?),
            T_TAKE_AMOUNT => env.take_amount = Some(read_u64(value, tlv_type)?),
            T_NIM_TIMEOUT => env.nim_timeout = Some(read_u64(value, tlv_type)?),
            T_COUNTERPARTY_TIMEOUT => env.counterparty_timeout = Some(read_u64(value, tlv_type)?),
            T_NIM_ADDRESS => {
                env.nim_address = Some(
                    value
                        .try_into()
                        .map_err(|_| SwapWireError::BadFieldLength { tlv_type, len })?,
                );
            }
            T_COUNTERPARTY_ADDRESS => env.counterparty_address = Some(value.to_vec()),
            T_LEG => {
                if len != 1 {
                    return Err(SwapWireError::BadFieldLength { tlv_type, len });
                }
                env.leg = Some(
                    SwapLegId::from_u8(value[0])
                        .ok_or(SwapWireError::BadFieldValue { tlv_type })?,
                );
            }
            T_TX_WIRE => env.tx_wire = Some(value.to_vec()),
            T_TX_ID => {
                env.tx_id = Some(
                    value
                        .try_into()
                        .map_err(|_| SwapWireError::BadFieldLength { tlv_type, len })?,
                );
            }
            T_NETWORK_ID => {
                if len != 1 {
                    return Err(SwapWireError::BadFieldLength { tlv_type, len });
                }
                env.network_id = Some(value[0]);
            }
            T_REASON => {
                if len != 1 {
                    return Err(SwapWireError::BadFieldLength { tlv_type, len });
                }
                env.reason = Some(value[0]);
            }
            // Unknown TLV type: skip its value for forward-compatibility.
            _ => {}
        }

        off = value_end;
    }

    if !seen_swap_id {
        return Err(SwapWireError::MissingRequiredField {
            tlv_type: T_SWAP_ID,
        });
    }
    for &req in required_fields(kind) {
        if !present.contains(&req) {
            return Err(SwapWireError::MissingRequiredField { tlv_type: req });
        }
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn propose() -> SwapEnvelope {
        SwapEnvelope {
            swap_id: [0xA1; SWAP_ID_LEN],
            hashlock: Some([0x7E; HASH_LEN]),
            give_amount: Some(100_000),
            take_amount: Some(50_000),
            nim_timeout: Some(4_500_000),
            counterparty_timeout: Some(4_490_000),
            nim_address: Some([0x4D; NIM_ADDRESS_LEN]),
            counterparty_address: Some(b"bc1qexampleaddress".to_vec()),
            network_id: Some(5),
            ..Default::default()
        }
    }

    fn funding() -> SwapEnvelope {
        SwapEnvelope {
            swap_id: [0xB2; SWAP_ID_LEN],
            leg: Some(SwapLegId::Nim),
            tx_wire: Some(vec![0x11; 248]), // a signed HTLC funding tx is ~248 B (fits one TLV)
            tx_id: Some([0xCD; HASH_LEN]),
            network_id: Some(5),
            ..Default::default()
        }
    }

    #[test]
    fn message_type_maps_both_ways() {
        for k in [
            SwapKind::Propose,
            SwapKind::Accept,
            SwapKind::FundingProof,
            SwapKind::PreimageReveal,
            SwapKind::Abort,
        ] {
            assert_eq!(SwapKind::from_message_type(k.message_type()), Some(k));
        }
        // A non-swap type is not a swap kind.
        assert_eq!(SwapKind::from_message_type(MessageType::NimiqTx), None);
    }

    #[test]
    fn propose_round_trips() {
        let env = propose();
        let bytes = encode_swap(&env).unwrap();
        assert_eq!(decode_swap(SwapKind::Propose, &bytes).unwrap(), env);
    }

    #[test]
    fn funding_round_trips_with_opaque_tx() {
        let env = funding();
        let bytes = encode_swap(&env).unwrap();
        let decoded = decode_swap(SwapKind::FundingProof, &bytes).unwrap();
        assert_eq!(decoded, env);
        assert_eq!(decoded.tx_wire.unwrap().len(), 248);
        assert_eq!(decoded.leg, Some(SwapLegId::Nim));
    }

    #[test]
    fn reveal_and_abort_round_trip() {
        let reveal = SwapEnvelope {
            swap_id: [0x33; SWAP_ID_LEN],
            leg: Some(SwapLegId::Counterparty),
            tx_wire: Some(vec![0x22; 120]),
            tx_id: Some([0x44; HASH_LEN]),
            ..Default::default()
        };
        let bytes = encode_swap(&reveal).unwrap();
        assert_eq!(
            decode_swap(SwapKind::PreimageReveal, &bytes).unwrap(),
            reveal
        );

        let abort = SwapEnvelope {
            swap_id: [0x55; SWAP_ID_LEN],
            reason: Some(2),
            ..Default::default()
        };
        let bytes = encode_swap(&abort).unwrap();
        assert_eq!(decode_swap(SwapKind::Abort, &bytes).unwrap(), abort);
    }

    #[test]
    fn tlv_layout_is_type_len_value() {
        let env = SwapEnvelope::new([0x09; SWAP_ID_LEN]);
        let bytes = encode_swap(&env).unwrap();
        assert_eq!(bytes[0], T_SWAP_ID);
        assert_eq!(bytes[1], SWAP_ID_LEN as u8);
        assert_eq!(&bytes[2..2 + SWAP_ID_LEN], &[0x09; SWAP_ID_LEN]);
    }

    #[test]
    fn decode_enforces_required_fields_per_kind() {
        // A propose missing the hashlock is rejected.
        let mut env = propose();
        env.hashlock = None;
        let bytes = encode_swap(&env).unwrap();
        assert_eq!(
            decode_swap(SwapKind::Propose, &bytes).unwrap_err(),
            SwapWireError::MissingRequiredField {
                tlv_type: T_HASHLOCK
            }
        );
        // ...but the SAME bytes decode fine as an Abort, which doesn't require a hashlock —
        // proving requirements are per-kind. (Abort needs a reason; add one.)
        let mut abortish = propose();
        abortish.hashlock = None;
        abortish.reason = Some(1);
        let bytes = encode_swap(&abortish).unwrap();
        assert!(decode_swap(SwapKind::Abort, &bytes).is_ok());
    }

    #[test]
    fn decode_requires_swap_id() {
        // Hand-build a TLV stream with only a reason, no swap id.
        let bytes = [T_REASON, 1, 0x00];
        assert_eq!(
            decode_swap(SwapKind::Abort, &bytes).unwrap_err(),
            SwapWireError::MissingRequiredField {
                tlv_type: T_SWAP_ID
            }
        );
    }

    #[test]
    fn decode_skips_unknown_tlv_types() {
        let mut bytes = encode_swap(&funding()).unwrap();
        bytes.extend_from_slice(&[0x7F, 0x03, 0xAA, 0xBB, 0xCC]); // unknown TLV
        let env = decode_swap(SwapKind::FundingProof, &bytes).unwrap();
        assert_eq!(env.swap_id, [0xB2; SWAP_ID_LEN]);
    }

    #[test]
    fn decode_rejects_truncation_and_bad_lengths() {
        // Declared len 10, only 2 bytes follow.
        let bytes = [T_SWAP_ID, 10, 0xAA, 0xBB];
        assert!(matches!(
            decode_swap(SwapKind::Abort, &bytes).unwrap_err(),
            SwapWireError::Truncated { .. }
        ));
        // swap_id with the wrong length.
        let bytes = [T_SWAP_ID, 2, 0x00, 0x00];
        assert!(matches!(
            decode_swap(SwapKind::Abort, &bytes).unwrap_err(),
            SwapWireError::BadFieldLength {
                tlv_type: T_SWAP_ID,
                ..
            }
        ));
    }

    #[test]
    fn decode_rejects_bad_leg_value() {
        let mut bytes = Vec::new();
        push_tlv(&mut bytes, T_SWAP_ID, &[0u8; SWAP_ID_LEN]).unwrap();
        push_tlv(&mut bytes, T_LEG, &[0x09]).unwrap(); // 0x09 is not a valid leg
        push_tlv(&mut bytes, T_TX_WIRE, &[0u8; 10]).unwrap();
        push_tlv(&mut bytes, T_TX_ID, &[0u8; HASH_LEN]).unwrap();
        assert_eq!(
            decode_swap(SwapKind::FundingProof, &bytes).unwrap_err(),
            SwapWireError::BadFieldValue { tlv_type: T_LEG }
        );
    }

    #[test]
    fn encode_rejects_overlong_value() {
        let env = SwapEnvelope {
            counterparty_address: Some(vec![0u8; 256]),
            ..SwapEnvelope::new([0; SWAP_ID_LEN])
        };
        assert!(matches!(
            encode_swap(&env).unwrap_err(),
            SwapWireError::ValueTooLong {
                tlv_type: T_COUNTERPARTY_ADDRESS,
                len: 256
            }
        ));
    }
}
