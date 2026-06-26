//! # Nimiq TLV envelope (G4)
//!
//! The Bitchat-style **type-length-value** payload that rides inside a `nimiqTx`
//! (`0x30`) packet. Each field is `1B type | 1B len | value`, so a value is at most
//! 255 bytes. Fields (PROTOCOL.md):
//!
//! | T    | field         | req | value                                            |
//! | ---- | ------------- | :-: | ------------------------------------------------ |
//! | 0x01 | `txWire`      | yes | canonical signed Nimiq bytes (**opaque** here)   |
//! | 0x02 | `networkId`   | yes | 1 B; **default testnet = 5**                     |
//! | 0x03 | `validUntil`  | opt | u32 BE block height                              |
//! | 0x04 | `txId`        | opt | 32 B hash (dedup key)                            |
//! | 0x05 | `encMemo`     | opt | Noise/NIP-44 encrypted blob                      |
//! | 0x06 | `wantReceipt` | opt | 1 B flag                                         |
//!
//! ## txWire is opaque
//!
//! `tx_wire` is treated as **opaque bytes**: this module never parses, signs, verifies,
//! or broadcasts it (that is G3 / G8, money-path and Andjroo-gated). The mesh only ever
//! carries public, broadcast-safe bytes (GOAL.md core value #1).
//!
//! ## Strictness
//!
//! [`decode_envelope`] is panic-free over arbitrary input (property-tested): every TLV
//! is bounds-checked, fixed-width fields (`networkId`/`validUntil`/`txId`/`wantReceipt`)
//! must have their exact length, unknown TLV types are skipped for forward-compat, and
//! the two **required** fields must be present.

use std::fmt;

/// TLV type byte for the required, opaque signed-tx bytes.
pub const T_TX_WIRE: u8 = 0x01;
/// TLV type byte for the required 1-byte Albatross network id.
pub const T_NETWORK_ID: u8 = 0x02;
/// TLV type byte for the optional u32 BE `validUntil` block height.
pub const T_VALID_UNTIL: u8 = 0x03;
/// TLV type byte for the optional 32-byte txId hash.
pub const T_TX_ID: u8 = 0x04;
/// TLV type byte for the optional encrypted memo blob.
pub const T_ENC_MEMO: u8 = 0x05;
/// TLV type byte for the optional 1-byte `wantReceipt` flag.
pub const T_WANT_RECEIPT: u8 = 0x06;

/// Default network id: Nimiq **testnet** (`5`). The loop never defaults to mainnet
/// (`24`) — that is Andjroo's switch (LOOP.md guardrails).
pub const DEFAULT_NETWORK_ID: u8 = 5;

/// Length of a Nimiq txId hash in bytes.
pub const TX_ID_LEN: usize = 32;

/// Maximum length of any single TLV value (the `len` field is one byte).
pub const MAX_TLV_VALUE_LEN: usize = u8::MAX as usize;

/// The decoded Nimiq TLV envelope carried as a `nimiqTx` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NimiqEnvelope {
    /// `0x01` — canonical signed Nimiq bytes, **opaque** to this layer.
    pub tx_wire: Vec<u8>,
    /// `0x02` — the Albatross network id (default [`DEFAULT_NETWORK_ID`]).
    pub network_id: u8,
    /// `0x03` — optional u32 block height after which a gateway drops the tx.
    pub valid_until: Option<u32>,
    /// `0x04` — optional 32-byte tx hash used as the dedup key.
    pub tx_id: Option<[u8; TX_ID_LEN]>,
    /// `0x05` — optional encrypted memo blob (Noise / NIP-44).
    pub enc_memo: Option<Vec<u8>>,
    /// `0x06` — whether the sender wants a `nimiqTxReceipt` back.
    pub want_receipt: bool,
}

impl NimiqEnvelope {
    /// A minimal envelope: the required opaque `tx_wire` plus the default testnet
    /// network id, with every optional field absent.
    pub fn new(tx_wire: Vec<u8>) -> Self {
        NimiqEnvelope {
            tx_wire,
            network_id: DEFAULT_NETWORK_ID,
            valid_until: None,
            tx_id: None,
            enc_memo: None,
            want_receipt: false,
        }
    }
}

/// An error from [`encode_envelope`] or [`decode_envelope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
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
    /// `decode`: a fixed-width field (`networkId`/`validUntil`/`txId`/`wantReceipt`)
    /// carried the wrong length.
    BadFieldLength {
        /// The TLV type with the wrong length.
        tlv_type: u8,
        /// The length seen on the wire.
        len: usize,
    },
    /// `decode`: a required field (`txWire` and/or `networkId`) was absent.
    MissingRequiredField {
        /// The TLV type that was required but not present.
        tlv_type: u8,
    },
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvelopeError::ValueTooLong { tlv_type, len } => write!(
                f,
                "TLV 0x{tlv_type:02x} value of {len} bytes exceeds the {MAX_TLV_VALUE_LEN}-byte max"
            ),
            EnvelopeError::Truncated { offset } => {
                write!(f, "truncated TLV stream at offset {offset}")
            }
            EnvelopeError::BadFieldLength { tlv_type, len } => {
                write!(f, "TLV 0x{tlv_type:02x} has invalid length {len}")
            }
            EnvelopeError::MissingRequiredField { tlv_type } => {
                write!(f, "required TLV 0x{tlv_type:02x} is missing")
            }
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Append one `type | len | value` TLV, erroring if the value is too long for the
/// 1-byte length field.
fn push_tlv(buf: &mut Vec<u8>, tlv_type: u8, value: &[u8]) -> Result<(), EnvelopeError> {
    if value.len() > MAX_TLV_VALUE_LEN {
        return Err(EnvelopeError::ValueTooLong {
            tlv_type,
            len: value.len(),
        });
    }
    buf.push(tlv_type);
    buf.push(value.len() as u8);
    buf.extend_from_slice(value);
    Ok(())
}

/// Serialize a [`NimiqEnvelope`] to its TLV bytes, in ascending type order.
///
/// `tx_wire` and `network_id` are always emitted (required); the optional fields are
/// emitted only when present / true.
pub fn encode_envelope(env: &NimiqEnvelope) -> Result<Vec<u8>, EnvelopeError> {
    let mut buf = Vec::new();
    push_tlv(&mut buf, T_TX_WIRE, &env.tx_wire)?;
    push_tlv(&mut buf, T_NETWORK_ID, &[env.network_id])?;
    if let Some(vu) = env.valid_until {
        push_tlv(&mut buf, T_VALID_UNTIL, &vu.to_be_bytes())?;
    }
    if let Some(id) = &env.tx_id {
        push_tlv(&mut buf, T_TX_ID, id)?;
    }
    if let Some(memo) = &env.enc_memo {
        push_tlv(&mut buf, T_ENC_MEMO, memo)?;
    }
    if env.want_receipt {
        push_tlv(&mut buf, T_WANT_RECEIPT, &[1])?;
    }
    Ok(buf)
}

/// Parse a TLV stream back into a [`NimiqEnvelope`].
///
/// Panic-free and strict: each TLV is bounds-checked, fixed-width fields must carry
/// their exact length, unknown TLV types are skipped (forward-compat), and the
/// required `txWire` + `networkId` must both be present.
pub fn decode_envelope(bytes: &[u8]) -> Result<NimiqEnvelope, EnvelopeError> {
    let mut tx_wire: Option<Vec<u8>> = None;
    let mut network_id: Option<u8> = None;
    let mut valid_until: Option<u32> = None;
    let mut tx_id: Option<[u8; TX_ID_LEN]> = None;
    let mut enc_memo: Option<Vec<u8>> = None;
    let mut want_receipt = false;

    let mut off = 0usize;
    while off < bytes.len() {
        // Need at least the 2-byte type|len header.
        if off + 2 > bytes.len() {
            return Err(EnvelopeError::Truncated { offset: off });
        }
        let tlv_type = bytes[off];
        let len = bytes[off + 1] as usize;
        let value_start = off + 2;
        let value_end = value_start
            .checked_add(len)
            .ok_or(EnvelopeError::Truncated {
                offset: value_start,
            })?;
        if value_end > bytes.len() {
            return Err(EnvelopeError::Truncated {
                offset: value_start,
            });
        }
        let value = &bytes[value_start..value_end];

        match tlv_type {
            T_TX_WIRE => tx_wire = Some(value.to_vec()),
            T_NETWORK_ID => {
                if len != 1 {
                    return Err(EnvelopeError::BadFieldLength { tlv_type, len });
                }
                network_id = Some(value[0]);
            }
            T_VALID_UNTIL => {
                if len != 4 {
                    return Err(EnvelopeError::BadFieldLength { tlv_type, len });
                }
                valid_until = Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]));
            }
            T_TX_ID => {
                if len != TX_ID_LEN {
                    return Err(EnvelopeError::BadFieldLength { tlv_type, len });
                }
                let mut id = [0u8; TX_ID_LEN];
                id.copy_from_slice(value);
                tx_id = Some(id);
            }
            T_ENC_MEMO => enc_memo = Some(value.to_vec()),
            T_WANT_RECEIPT => {
                if len != 1 {
                    return Err(EnvelopeError::BadFieldLength { tlv_type, len });
                }
                want_receipt = value[0] != 0;
            }
            // Unknown TLV type: skip its value for forward-compatibility.
            _ => {}
        }

        off = value_end;
    }

    let tx_wire = tx_wire.ok_or(EnvelopeError::MissingRequiredField {
        tlv_type: T_TX_WIRE,
    })?;
    let network_id = network_id.ok_or(EnvelopeError::MissingRequiredField {
        tlv_type: T_NETWORK_ID,
    })?;

    Ok(NimiqEnvelope {
        tx_wire,
        network_id,
        valid_until,
        tx_id,
        enc_memo,
        want_receipt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_minimal_envelope() {
        let env = NimiqEnvelope::new(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let bytes = encode_envelope(&env).unwrap();
        assert_eq!(decode_envelope(&bytes).unwrap(), env);
        // Default network is testnet.
        assert_eq!(
            decode_envelope(&bytes).unwrap().network_id,
            DEFAULT_NETWORK_ID
        );
    }

    #[test]
    fn round_trip_full_envelope() {
        let env = NimiqEnvelope {
            tx_wire: vec![0x11; 139],
            network_id: 5,
            valid_until: Some(0xABCD_1234),
            tx_id: Some([0x7E; TX_ID_LEN]),
            enc_memo: Some(vec![0x55; 64]),
            want_receipt: true,
        };
        let bytes = encode_envelope(&env).unwrap();
        assert_eq!(decode_envelope(&bytes).unwrap(), env);
    }

    #[test]
    fn tlv_layout_is_type_len_value() {
        let env = NimiqEnvelope::new(vec![0xAA, 0xBB]);
        let bytes = encode_envelope(&env).unwrap();
        // 0x01 len=2 value; then 0x02 len=1 value=5.
        assert_eq!(bytes[0], T_TX_WIRE);
        assert_eq!(bytes[1], 2);
        assert_eq!(&bytes[2..4], &[0xAA, 0xBB]);
        assert_eq!(bytes[4], T_NETWORK_ID);
        assert_eq!(bytes[5], 1);
        assert_eq!(bytes[6], DEFAULT_NETWORK_ID);
    }

    #[test]
    fn encode_rejects_overlong_value() {
        let env = NimiqEnvelope::new(vec![0; 256]);
        assert!(matches!(
            encode_envelope(&env).unwrap_err(),
            EnvelopeError::ValueTooLong {
                tlv_type: T_TX_WIRE,
                len: 256
            }
        ));
    }

    #[test]
    fn decode_skips_unknown_tlv_types() {
        let mut bytes = encode_envelope(&NimiqEnvelope::new(vec![1, 2, 3])).unwrap();
        // Append an unknown TLV type 0x7F with a 3-byte value.
        bytes.extend_from_slice(&[0x7F, 0x03, 0xAA, 0xBB, 0xCC]);
        let env = decode_envelope(&bytes).unwrap();
        assert_eq!(env.tx_wire, vec![1, 2, 3]);
    }

    #[test]
    fn decode_rejects_missing_required_fields() {
        // Only networkId, no txWire.
        let bytes = [T_NETWORK_ID, 1, 5];
        assert!(matches!(
            decode_envelope(&bytes).unwrap_err(),
            EnvelopeError::MissingRequiredField {
                tlv_type: T_TX_WIRE
            }
        ));
        // Only txWire, no networkId.
        let bytes = [T_TX_WIRE, 1, 0x00];
        assert!(matches!(
            decode_envelope(&bytes).unwrap_err(),
            EnvelopeError::MissingRequiredField {
                tlv_type: T_NETWORK_ID
            }
        ));
    }

    #[test]
    fn decode_rejects_truncated_and_bad_lengths() {
        // Declared len 10 but only 2 bytes follow.
        let bytes = [T_TX_WIRE, 10, 0xAA, 0xBB];
        assert!(matches!(
            decode_envelope(&bytes).unwrap_err(),
            EnvelopeError::Truncated { .. }
        ));
        // networkId with a 2-byte value is malformed.
        let bytes = [T_TX_WIRE, 1, 0x00, T_NETWORK_ID, 2, 5, 5];
        assert!(matches!(
            decode_envelope(&bytes).unwrap_err(),
            EnvelopeError::BadFieldLength {
                tlv_type: T_NETWORK_ID,
                len: 2
            }
        ));
    }
}
