//! # nimmesh wire packet (G4)
//!
//! The in-memory model of a nimmesh wire packet plus its on-wire constants. This
//! module owns the *shape* of a packet (header fields, flag bits, message types);
//! the byte-level [`encode`](crate::codec::encode) / [`decode`](crate::codec::decode)
//! live next door in [`crate::codec`], and the Nimiq TLV payload that rides inside a
//! `nimiqTx` packet lives in [`crate::envelope`].
//!
//! ## On-wire layout (big-endian, header v1 = 14 bytes)
//!
//! ```text
//! version(1)=1 | type(1) MessageType | ttl(1) default 7 | timestamp(8, ms)
//!             | flags(1) | payloadLength(2)
//! then, in order:
//! senderID(8, always) | recipientID(8, if hasRecipient) | payload(payloadLength)
//!             | signature(64, if hasSignature)
//! ```
//!
//! The whole packet is then PKCS#7-style padded (see [`crate::codec`]) up to the
//! smallest of `[256, 512, 1024, 2048]` for traffic-analysis resistance.
//!
//! **Non-money-path.** A packet here is opaque bytes in / opaque bytes out: nothing
//! in this module signs, broadcasts, or inspects key material (LOOP.md guardrails).
//! `payload` and `signature` are treated as untyped blobs.

/// Protocol version this codec emits and accepts. Header byte 0 is always `1`.
pub const VERSION: u8 = 1;

/// Fixed header length in bytes: `version(1) + type(1) + ttl(1) + timestamp(8) +
/// flags(1) + payloadLength(2)`.
pub const HEADER_LEN: usize = 14;

/// Length of a sender / recipient peer id in bytes. A senderID is always present;
/// a recipientID only when [`FLAG_HAS_RECIPIENT`] is set.
pub const PEER_ID_LEN: usize = 8;

/// Length of an appended Ed25519 packet signature in bytes (present only when
/// [`FLAG_HAS_SIGNATURE`] is set).
pub const SIGNATURE_LEN: usize = 64;

/// Default and hop cap for a packet's time-to-live (PROTOCOL.md: `TTL = 7`).
pub const DEFAULT_TTL: u8 = 7;

/// The broadcast recipient id: `FF` ×8. A `nimiqTx` floods to this address.
pub const BROADCAST_RECIPIENT: [u8; PEER_ID_LEN] = [0xFF; PEER_ID_LEN];

// --- Flag bits (header byte 11) --------------------------------------------------

/// `0x01` — a `recipientID(8)` follows the senderID.
pub const FLAG_HAS_RECIPIENT: u8 = 0x01;
/// `0x02` — a `signature(64)` is appended after the payload.
pub const FLAG_HAS_SIGNATURE: u8 = 0x02;
/// `0x04` — the payload is zlib-compressed (compression itself is a later goal; this
/// codec only carries the bit faithfully).
pub const FLAG_IS_COMPRESSED: u8 = 0x04;
/// `0x08` — a v2 source route is present (reserved; carried but not interpreted here).
pub const FLAG_HAS_ROUTE: u8 = 0x08;
/// `0x10` — this is a gossip-sync reply (`isRSR`).
pub const FLAG_IS_RSR: u8 = 0x10;

/// Mask of the flag bits that are *derived* from packet structure (recipient /
/// signature presence) rather than stored independently in [`PacketFlags`].
const DERIVED_FLAG_MASK: u8 = FLAG_HAS_RECIPIENT | FLAG_HAS_SIGNATURE;

/// A nimmesh message type (header byte 1).
///
/// The standard relay/sync types (`fragment`, `requestSync`) are placeholders for
/// later goals; the `nimiq*` types are this fork's reservations from the free
/// `0x23–0xFF` range (PROTOCOL.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageType {
    /// `0x20` — a reassembly fragment (defined; unused for our sub-256-B payloads).
    Fragment = 0x20,
    /// `0x21` — a GCS gossip-sync request (store-and-forward; ttl 0, local-only).
    RequestSync = 0x21,
    /// `0x11` — an optional 1:1 **Noise-encrypted** message (encrypted memo / chat, G11).
    /// The payload is an opaque sealed blob; relays carry it blindly (transport-privacy
    /// only — never a money-path). Inner type per [`crate::noise::NoisePayloadType`].
    NoiseEncrypted = 0x11,
    /// `0x30` — a signed Nimiq tx, flooded broadcast-safe over the mesh.
    NimiqTx = 0x30,
    /// `0x31` — a gateway → mesh accepted/expired/failed ack, keyed by txId.
    NimiqTxReceipt = 0x31,
    /// `0x32` — a gateway → mesh head beacon `{height, blockHash, networkId}`.
    NimiqHeadBeacon = 0x32,
    /// `0x33` — a mesh → gateway **balance query** `{address}` (G15). Read-only: asks any
    /// internet-bearing gateway for an address's on-chain balance. Carries no key material.
    NimiqBalanceQuery = 0x33,
    /// `0x34` — a gateway → mesh **balance response** `{address, balance, headHeight, networkId}`
    /// (G15). The balance as the gateway read it at `headHeight`; **unverified** (a relay is
    /// untrusted) until a future accounts-proof. Read-only, non-money-path.
    NimiqBalanceResponse = 0x34,
}

impl MessageType {
    /// The on-wire type byte.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Parse a type byte, returning `None` for any value not in our reserved set.
    ///
    /// Decoders treat an unknown type as a hard error (strict bounds checking) rather
    /// than silently guessing — mesh input is hostile (GOAL.md core value #6).
    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x11 => Some(MessageType::NoiseEncrypted),
            0x20 => Some(MessageType::Fragment),
            0x21 => Some(MessageType::RequestSync),
            0x30 => Some(MessageType::NimiqTx),
            0x31 => Some(MessageType::NimiqTxReceipt),
            0x32 => Some(MessageType::NimiqHeadBeacon),
            0x33 => Some(MessageType::NimiqBalanceQuery),
            0x34 => Some(MessageType::NimiqBalanceResponse),
            _ => None,
        }
    }
}

/// The independently-stored flag bits of a packet.
///
/// `hasRecipient` and `hasSignature` are intentionally **not** stored here — they are
/// derived from the presence of [`Packet::recipient_id`] / [`Packet::signature`], so
/// the in-memory model can never disagree with the bytes on the wire. Only the bits
/// that carry standalone meaning live here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketFlags {
    /// `0x04` isCompressed — payload is zlib-compressed.
    pub is_compressed: bool,
    /// `0x08` hasRoute — a v2 source route is present.
    pub has_route: bool,
    /// `0x10` isRSR — gossip-sync reply.
    pub is_rsr: bool,
}

impl PacketFlags {
    /// Assemble the full wire flags byte, OR-ing in the derived recipient/signature
    /// bits supplied by the encoder.
    pub const fn to_wire(self, has_recipient: bool, has_signature: bool) -> u8 {
        let mut byte = 0u8;
        if has_recipient {
            byte |= FLAG_HAS_RECIPIENT;
        }
        if has_signature {
            byte |= FLAG_HAS_SIGNATURE;
        }
        if self.is_compressed {
            byte |= FLAG_IS_COMPRESSED;
        }
        if self.has_route {
            byte |= FLAG_HAS_ROUTE;
        }
        if self.is_rsr {
            byte |= FLAG_IS_RSR;
        }
        byte
    }

    /// Extract only the standalone flag bits from a wire flags byte (ignoring the
    /// derived recipient/signature bits, which the decoder reads structurally).
    pub const fn from_wire(byte: u8) -> Self {
        PacketFlags {
            is_compressed: byte & FLAG_IS_COMPRESSED != 0,
            has_route: byte & FLAG_HAS_ROUTE != 0,
            is_rsr: byte & FLAG_IS_RSR != 0,
        }
    }

    /// Whether this flags byte carries any bit outside the five defined in the spec.
    /// Used by the decoder to reject malformed/unknown flag bits strictly.
    pub const fn wire_has_unknown_bits(byte: u8) -> bool {
        let known = DERIVED_FLAG_MASK | FLAG_IS_COMPRESSED | FLAG_HAS_ROUTE | FLAG_IS_RSR;
        byte & !known != 0
    }
}

/// A decoded nimmesh wire packet.
///
/// Construct one directly (all fields are public) or via [`Packet::new`] for a sane
/// `version = 1`, `ttl = 7` default. The `payload` is opaque to this layer; for a
/// `nimiqTx` it is a [`crate::envelope::NimiqEnvelope`] encoded to bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Protocol version. Always [`VERSION`] (`1`) for a packet this codec emits.
    pub version: u8,
    /// The message type (header byte 1).
    pub msg_type: MessageType,
    /// Time-to-live / hop budget; defaults to [`DEFAULT_TTL`].
    pub ttl: u8,
    /// Origination time in milliseconds since the Unix epoch (header bytes 3..11).
    pub timestamp_ms: u64,
    /// Standalone flag bits (compressed / route / RSR).
    pub flags: PacketFlags,
    /// The 8-byte sender id (always present).
    pub sender_id: [u8; PEER_ID_LEN],
    /// The 8-byte recipient id, present iff `hasRecipient`. `FF`×8 means broadcast.
    pub recipient_id: Option<[u8; PEER_ID_LEN]>,
    /// The opaque payload (`payloadLength` bytes on the wire).
    pub payload: Vec<u8>,
    /// An optional appended 64-byte Ed25519 signature, present iff `hasSignature`.
    pub signature: Option<[u8; SIGNATURE_LEN]>,
}

impl Packet {
    /// A new packet with `version = 1`, `ttl = 7`, `timestamp_ms = 0`, no recipient,
    /// no signature, and default flags. Callers set timestamp/recipient as needed.
    pub fn new(msg_type: MessageType, sender_id: [u8; PEER_ID_LEN], payload: Vec<u8>) -> Self {
        Packet {
            version: VERSION,
            msg_type,
            ttl: DEFAULT_TTL,
            timestamp_ms: 0,
            flags: PacketFlags::default(),
            sender_id,
            recipient_id: None,
            payload,
            signature: None,
        }
    }

    /// Whether a `recipientID(8)` is present (drives [`FLAG_HAS_RECIPIENT`]).
    pub const fn has_recipient(&self) -> bool {
        self.recipient_id.is_some()
    }

    /// Whether a `signature(64)` is present (drives [`FLAG_HAS_SIGNATURE`]).
    pub const fn has_signature(&self) -> bool {
        self.signature.is_some()
    }

    /// The exact serialized length of this packet **before** padding: the fixed
    /// header, the always-present senderID, plus the conditionally-present
    /// recipientID, payload, and signature.
    pub fn unpadded_len(&self) -> usize {
        HEADER_LEN
            + PEER_ID_LEN
            + if self.has_recipient() { PEER_ID_LEN } else { 0 }
            + self.payload.len()
            + if self.has_signature() {
                SIGNATURE_LEN
            } else {
                0
            }
    }
}
