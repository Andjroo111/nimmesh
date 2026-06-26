//! # bitmesh packet codec (G4)
//!
//! Byte-level [`encode`] / [`decode`] for a [`Packet`], plus the PKCS#7-style block
//! padding that gives every frame one of a handful of fixed sizes for traffic-analysis
//! resistance. Big-endian throughout.
//!
//! ## Padding
//!
//! After the structured packet is serialized it is padded up to the **smallest** of
//! `[256, 512, 1024, 2048]` that fits, using PKCS#7-style bytes (every pad byte equals
//! the pad length). Two faithful edge cases, matching the Bitchat reference:
//!
//! - PKCS#7 can only encode a pad length of `1..=255`. If the gap to the next block is
//!   larger than 255 bytes (or the packet already exactly fills a block, or it exceeds
//!   2048), the frame is left **un-padded**. Our real ~205-B `nimiqTx` always lands in
//!   the 256 block, so this never bites in practice.
//! - Decoding does **not** trust the trailing pad bytes: it recomputes the exact packet
//!   length from the header (`hasRecipient`, `payloadLength`, `hasSignature`) and
//!   ignores everything after it. This makes padding unambiguous and removes any
//!   PKCS#7 unpad oracle.
//!
//! ## Strictness
//!
//! [`decode`] never panics on arbitrary input (property-tested): every field read is
//! bounds-checked with checked arithmetic, an unknown version / message type / flag bit
//! is a hard error, and a buffer shorter than the header-declared length is rejected.
//!
//! **Non-money-path** — opaque bytes only; no signing, no broadcast.

use crate::packet::{
    MessageType, Packet, PacketFlags, HEADER_LEN, PEER_ID_LEN, SIGNATURE_LEN, VERSION,
};
use std::fmt;

/// The padding block sizes, smallest first (PROTOCOL.md).
pub const BLOCK_SIZES: [usize; 4] = [256, 512, 1024, 2048];

/// Maximum payload length, bounded by the 2-byte `payloadLength` header field.
pub const MAX_PAYLOAD_LEN: usize = u16::MAX as usize;

/// An error from [`encode`] or [`decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// `encode`: the payload is longer than the 2-byte `payloadLength` field allows.
    PayloadTooLarge {
        /// The offending payload length.
        len: usize,
    },
    /// `decode`: the buffer is shorter than the bytes the header says are present.
    Truncated {
        /// Bytes the header implies must be present.
        needed: usize,
        /// Bytes actually available.
        got: usize,
    },
    /// `decode`: header version byte is not [`VERSION`].
    UnsupportedVersion {
        /// The version byte seen on the wire.
        found: u8,
    },
    /// `decode`: header type byte is not a known [`MessageType`].
    UnknownMessageType {
        /// The type byte seen on the wire.
        found: u8,
    },
    /// `decode`: the flags byte carries a bit outside the five defined ones.
    UnknownFlagBits {
        /// The flags byte seen on the wire.
        found: u8,
    },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::PayloadTooLarge { len } => {
                write!(
                    f,
                    "payload of {len} bytes exceeds the {MAX_PAYLOAD_LEN}-byte maximum"
                )
            }
            CodecError::Truncated { needed, got } => {
                write!(
                    f,
                    "truncated packet: header implies {needed} bytes, only {got} available"
                )
            }
            CodecError::UnsupportedVersion { found } => {
                write!(
                    f,
                    "unsupported protocol version {found} (expected {VERSION})"
                )
            }
            CodecError::UnknownMessageType { found } => {
                write!(f, "unknown message type 0x{found:02x}")
            }
            CodecError::UnknownFlagBits { found } => {
                write!(f, "unknown flag bits set in 0x{found:02x}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// The smallest block size that fits `n` bytes, or `None` if `n` exceeds the largest
/// block (`2048`) and so should ride un-padded.
pub fn optimal_block_size(n: usize) -> Option<usize> {
    BLOCK_SIZES.into_iter().find(|&block| n <= block)
}

/// Pad `buf` in place up to its [`optimal_block_size`] using PKCS#7-style bytes.
///
/// Leaves `buf` untouched when there is no representable padding (gap `0` or `> 255`,
/// or the data exceeds the largest block) — decoding recovers the real length from the
/// header regardless, so an un-padded frame is still correct, just less uniform.
fn apply_padding(buf: &mut Vec<u8>) {
    let Some(target) = optimal_block_size(buf.len()) else {
        return;
    };
    let pad_needed = target - buf.len();
    if pad_needed == 0 || pad_needed > 255 {
        return;
    }
    let pad_byte = pad_needed as u8;
    buf.resize(target, pad_byte);
}

/// Serialize a [`Packet`] to its padded on-wire bytes.
///
/// Always writes `version`, `type`, `ttl`, `timestamp`, `flags`, `payloadLength`, then
/// senderID, the optional recipientID, the payload, and the optional signature, then
/// pads. The flags byte is derived from the packet's structure plus its standalone
/// [`PacketFlags`], so the bytes can never disagree with the model.
pub fn encode(packet: &Packet) -> Result<Vec<u8>, CodecError> {
    let payload_len = packet.payload.len();
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(CodecError::PayloadTooLarge { len: payload_len });
    }

    let flags_byte = packet
        .flags
        .to_wire(packet.has_recipient(), packet.has_signature());

    let mut buf = Vec::with_capacity(packet.unpadded_len());
    buf.push(packet.version);
    buf.push(packet.msg_type.to_u8());
    buf.push(packet.ttl);
    buf.extend_from_slice(&packet.timestamp_ms.to_be_bytes());
    buf.push(flags_byte);
    buf.extend_from_slice(&(payload_len as u16).to_be_bytes());

    buf.extend_from_slice(&packet.sender_id);
    if let Some(recipient) = &packet.recipient_id {
        buf.extend_from_slice(recipient);
    }
    buf.extend_from_slice(&packet.payload);
    if let Some(signature) = &packet.signature {
        buf.extend_from_slice(signature);
    }

    apply_padding(&mut buf);
    Ok(buf)
}

/// Parse a padded on-wire frame back into a [`Packet`].
///
/// Strict and panic-free: rejects an unknown version, type, or flag bit, and a buffer
/// shorter than the header-declared length. Trailing padding past the structured
/// packet is ignored.
pub fn decode(bytes: &[u8]) -> Result<Packet, CodecError> {
    // The minimum possible frame is header + the always-present senderID.
    let min_len = HEADER_LEN + PEER_ID_LEN;
    if bytes.len() < min_len {
        return Err(CodecError::Truncated {
            needed: min_len,
            got: bytes.len(),
        });
    }

    let version = bytes[0];
    if version != VERSION {
        return Err(CodecError::UnsupportedVersion { found: version });
    }

    let msg_type =
        MessageType::from_u8(bytes[1]).ok_or(CodecError::UnknownMessageType { found: bytes[1] })?;
    let ttl = bytes[2];

    // timestamp(8) at bytes[3..11].
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&bytes[3..11]);
    let timestamp_ms = u64::from_be_bytes(ts);

    let flags_byte = bytes[11];
    if PacketFlags::wire_has_unknown_bits(flags_byte) {
        return Err(CodecError::UnknownFlagBits { found: flags_byte });
    }
    let flags = PacketFlags::from_wire(flags_byte);
    let has_recipient = flags_byte & crate::packet::FLAG_HAS_RECIPIENT != 0;
    let has_signature = flags_byte & crate::packet::FLAG_HAS_SIGNATURE != 0;

    let payload_len = u16::from_be_bytes([bytes[12], bytes[13]]) as usize;

    // Compute the exact structured length with checked arithmetic; payload_len is at
    // most 65535 so this never realistically overflows, but stay defensive.
    let needed = HEADER_LEN
        .checked_add(PEER_ID_LEN)
        .and_then(|n| n.checked_add(if has_recipient { PEER_ID_LEN } else { 0 }))
        .and_then(|n| n.checked_add(payload_len))
        .and_then(|n| n.checked_add(if has_signature { SIGNATURE_LEN } else { 0 }))
        .ok_or(CodecError::Truncated {
            needed: usize::MAX,
            got: bytes.len(),
        })?;
    if bytes.len() < needed {
        return Err(CodecError::Truncated {
            needed,
            got: bytes.len(),
        });
    }

    let mut off = HEADER_LEN;

    let mut sender_id = [0u8; PEER_ID_LEN];
    sender_id.copy_from_slice(&bytes[off..off + PEER_ID_LEN]);
    off += PEER_ID_LEN;

    let recipient_id = if has_recipient {
        let mut r = [0u8; PEER_ID_LEN];
        r.copy_from_slice(&bytes[off..off + PEER_ID_LEN]);
        off += PEER_ID_LEN;
        Some(r)
    } else {
        None
    };

    let payload = bytes[off..off + payload_len].to_vec();
    off += payload_len;

    let signature = if has_signature {
        let mut s = [0u8; SIGNATURE_LEN];
        s.copy_from_slice(&bytes[off..off + SIGNATURE_LEN]);
        Some(s)
    } else {
        None
    };

    Ok(Packet {
        version,
        msg_type,
        ttl,
        timestamp_ms,
        flags,
        sender_id,
        recipient_id,
        payload,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{BROADCAST_RECIPIENT, DEFAULT_TTL};

    fn sample(msg_type: MessageType, payload: Vec<u8>) -> Packet {
        let mut p = Packet::new(msg_type, [1, 2, 3, 4, 5, 6, 7, 8], payload);
        p.timestamp_ms = 0x0102_0304_0506_0708;
        p
    }

    #[test]
    fn round_trip_minimal_packet() {
        let p = sample(MessageType::RequestSync, Vec::new());
        let bytes = encode(&p).unwrap();
        assert_eq!(decode(&bytes).unwrap(), p);
    }

    #[test]
    fn round_trip_every_message_type() {
        for ty in [
            MessageType::Fragment,
            MessageType::RequestSync,
            MessageType::NoiseEncrypted,
            MessageType::NimiqTx,
            MessageType::NimiqTxReceipt,
            MessageType::NimiqHeadBeacon,
        ] {
            let p = sample(ty, vec![0xAB; 50]);
            let bytes = encode(&p).unwrap();
            let back = decode(&bytes).unwrap();
            assert_eq!(back.msg_type, ty);
            assert_eq!(back, p);
        }
    }

    #[test]
    fn round_trip_full_nimiq_tx_flood() {
        // The canonical broadcast: nimiqTx, recipient = FF×8, ttl 7, no signature.
        let mut p = sample(MessageType::NimiqTx, vec![0x42; 205]);
        p.recipient_id = Some(BROADCAST_RECIPIENT);
        p.ttl = DEFAULT_TTL;
        let bytes = encode(&p).unwrap();
        // ~205-B tx + headers pads cleanly into the 256 block.
        assert_eq!(bytes.len(), 256);
        let back = decode(&bytes).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.recipient_id, Some(BROADCAST_RECIPIENT));
        assert!(back.has_recipient());
        assert!(!back.has_signature());
    }

    #[test]
    fn round_trip_with_signature() {
        let mut p = sample(MessageType::NimiqTxReceipt, vec![9; 10]);
        p.signature = Some([0x77; SIGNATURE_LEN]);
        let bytes = encode(&p).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.signature, Some([0x77; SIGNATURE_LEN]));
        assert_eq!(back, p);
    }

    #[test]
    fn round_trip_all_standalone_flags() {
        let mut p = sample(MessageType::NimiqTx, vec![1, 2, 3]);
        p.flags = PacketFlags {
            is_compressed: true,
            has_route: true,
            is_rsr: true,
        };
        let bytes = encode(&p).unwrap();
        // flags byte (index 11) carries the three standalone bits, no recipient/sig.
        assert_eq!(bytes[11], 0x04 | 0x08 | 0x10);
        assert_eq!(decode(&bytes).unwrap(), p);
    }

    #[test]
    fn header_field_byte_offsets() {
        let mut p = sample(MessageType::NimiqTx, vec![0xEE; 3]);
        p.ttl = 7;
        let bytes = encode(&p).unwrap();
        assert_eq!(bytes[0], VERSION);
        assert_eq!(bytes[1], 0x30); // nimiqTx
        assert_eq!(bytes[2], 7); // ttl
        assert_eq!(&bytes[3..11], &0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(&bytes[12..14], &3u16.to_be_bytes()); // payloadLength
    }

    #[test]
    fn padding_picks_smallest_fitting_block() {
        // payload sized so the whole packet lands inside each block with a pad gap
        // <= 255 (PKCS#7-representable). base overhead = header(14) + sender(8) = 22.
        for (payload, expected_block) in [(10usize, 256), (300, 512), (800, 1024)] {
            let p = sample(MessageType::NimiqTx, vec![0; payload]);
            let bytes = encode(&p).unwrap();
            assert_eq!(bytes.len(), expected_block, "payload {payload}");
        }
    }

    #[test]
    fn padding_bytes_are_pkcs7_style() {
        let p = sample(MessageType::NimiqTx, vec![0; 10]);
        let bytes = encode(&p).unwrap();
        let unpadded = p.unpadded_len();
        let pad_len = bytes.len() - unpadded;
        assert!(pad_len > 0 && pad_len <= 255);
        for &b in &bytes[unpadded..] {
            assert_eq!(b as usize, pad_len);
        }
    }

    #[test]
    fn oversize_packet_rides_unpadded() {
        // > 2048 bytes total -> no block fits -> left un-padded, still round-trips.
        let p = sample(MessageType::NimiqTx, vec![0xCD; 3000]);
        let bytes = encode(&p).unwrap();
        assert_eq!(bytes.len(), p.unpadded_len());
        assert!(optimal_block_size(bytes.len()).is_none());
        assert_eq!(decode(&bytes).unwrap(), p);
    }

    #[test]
    fn decode_rejects_truncated() {
        let p = sample(MessageType::NimiqTx, vec![1; 40]);
        let bytes = encode(&p).unwrap();
        // Chop into the declared payload region (but keep > header).
        let err = decode(&bytes[..30]).unwrap_err();
        assert!(matches!(err, CodecError::Truncated { .. }));
        // Shorter than even the header.
        assert!(matches!(
            decode(&[0u8; 5]).unwrap_err(),
            CodecError::Truncated { .. }
        ));
    }

    #[test]
    fn decode_rejects_bad_version_type_flags() {
        let p = sample(MessageType::NimiqTx, vec![0; 8]);
        let mut bytes = encode(&p).unwrap();
        let good = bytes.clone();

        bytes[0] = 2;
        assert!(matches!(
            decode(&bytes).unwrap_err(),
            CodecError::UnsupportedVersion { found: 2 }
        ));

        bytes = good.clone();
        bytes[1] = 0x99;
        assert!(matches!(
            decode(&bytes).unwrap_err(),
            CodecError::UnknownMessageType { found: 0x99 }
        ));

        bytes = good;
        bytes[11] = 0x80; // bit outside the five defined flags
        assert!(matches!(
            decode(&bytes).unwrap_err(),
            CodecError::UnknownFlagBits { found: 0x80 }
        ));
    }
}
