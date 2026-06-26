//! # noise — optional encrypted memo / 1:1 chat (G11, PROTOCOL.md "Encryption")
//!
//! A **pure transport-privacy** layer: a `Noise_XX_25519_ChaChaPoly_SHA256` mutual-auth,
//! identity-hiding handshake between two mesh peers, then two ChaChaPoly cipher states
//! (one per direction) guarded by a **1024-message sliding-window** anti-replay filter.
//! It exists so an *optional* memo can ride a `nimiqTx` encrypted ([`crate::envelope`]
//! `encMemo` TLV `0x05`) and so two peers can exchange an *optional* 1:1 encrypted chat
//! message ([`crate::packet::MessageType::NoiseEncrypted`] `0x11`).
//!
//! ## Transport identity ≠ wallet
//!
//! The handshake identity is a **long-term Curve25519 *static* keypair generated for this
//! app's mesh session** ([`StaticIdentity`]). It is **explicitly separate** from any Nimiq
//! wallet seed — there is no wallet here, and there never will be in this module. G3/G10
//! own seed/signing (money-path, gated); this key only authenticates a *transport* session
//! and **never signs, broadcasts, or derives a Nimiq address**. The **fingerprint** is the
//! SHA-256 of the static public key, for out-of-band ("compare these 32 bytes") verification.
//!
//! ## Wire of an encrypted blob
//!
//! Every sealed blob is `nonce(8, BE) || ChaChaPoly(ciphertext||tag)`. The explicit nonce
//! lets the receiver run the sliding-window replay guard; the inner Noise cipher state
//! authenticates the nonce as associated data implicitly (a forged nonce fails the AEAD).
//! For a 1:1 chat / targeted payload the *plaintext* is `innerType(1) || message`, where
//! `innerType` is a [`NoisePayloadType`] (PROTOCOL.md: `nimiqTx = 0x04`,
//! `nimiqTxReceipt = 0x05`, plus our `chat = 0x01`).
//!
//! **Non-money-path.** Nothing here touches a wallet seed, signs a Nimiq tx, or broadcasts.

use std::fmt;

use sha2::{Digest, Sha256};

/// The Noise parameter string this module speaks (PROTOCOL.md "Encryption").
pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Length of a Curve25519 static public/secret key in bytes.
pub const STATIC_KEY_LEN: usize = 32;

/// Length of a static-key fingerprint (SHA-256 output) in bytes.
pub const FINGERPRINT_LEN: usize = 32;

/// The anti-replay sliding window: the receiver remembers the last **1024** message
/// counters and rejects any replayed or too-old nonce (PROTOCOL.md "1024-msg sliding
/// window replay guard").
pub const REPLAY_WINDOW: u64 = 1024;

/// Bytes of the big-endian message-counter nonce prepended to every sealed blob.
pub const NONCE_PREFIX_LEN: usize = 8;

/// The ChaChaPoly authentication tag length appended by the AEAD.
pub const NOISE_TAG_LEN: usize = 16;

/// The minimum length of a well-formed sealed blob: the nonce prefix plus an (empty)
/// ciphertext's authentication tag.
pub const MIN_SEALED_LEN: usize = NONCE_PREFIX_LEN + NOISE_TAG_LEN;

/// The inner payload type carried *inside* a `noiseEncrypted` (`0x11`) message, or used to
/// tag a sealed 1:1 chat (PROTOCOL.md "Inner `NoisePayloadType`").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NoisePayloadType {
    /// `0x01` — a plain 1:1 encrypted chat message (this fork's reservation).
    Chat = 0x01,
    /// `0x04` — a targeted (private) `nimiqTx` to a chosen gateway (Option B; the inner
    /// bytes stay **opaque** here — no signing/broadcast, money-path is G3/G8).
    NimiqTx = 0x04,
    /// `0x05` — a targeted (private) `nimiqTxReceipt`.
    NimiqTxReceipt = 0x05,
}

impl NoisePayloadType {
    /// The on-wire inner-type byte.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Parse an inner-type byte, returning `None` for any value outside the reserved set.
    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(NoisePayloadType::Chat),
            0x04 => Some(NoisePayloadType::NimiqTx),
            0x05 => Some(NoisePayloadType::NimiqTxReceipt),
            _ => None,
        }
    }
}

/// An error from the Noise handshake / session layer.
#[derive(Debug)]
pub enum NoiseError {
    /// The underlying `snow` Noise implementation rejected an operation (bad handshake
    /// message, AEAD authentication failure on decrypt, exhausted nonce, …).
    Noise(snow::Error),
    /// A sealed blob was shorter than [`MIN_SEALED_LEN`].
    TooShort {
        /// Bytes actually available.
        got: usize,
    },
    /// The message counter was a replay or older than the sliding window allows.
    Replay {
        /// The offending counter value.
        counter: u64,
    },
    /// `into_session` was called before the handshake completed all three XX messages.
    HandshakeIncomplete,
    /// A decrypted targeted payload was empty (no inner-type byte) or carried an unknown
    /// [`NoisePayloadType`].
    BadPayloadType {
        /// The inner-type byte seen (or `None` for an empty plaintext).
        found: Option<u8>,
    },
}

impl fmt::Display for NoiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoiseError::Noise(e) => write!(f, "noise error: {e}"),
            NoiseError::TooShort { got } => {
                write!(f, "sealed blob too short: {got} < {MIN_SEALED_LEN} bytes")
            }
            NoiseError::Replay { counter } => {
                write!(f, "replayed or stale message counter {counter}")
            }
            NoiseError::HandshakeIncomplete => {
                write!(f, "noise handshake not finished")
            }
            NoiseError::BadPayloadType { found } => match found {
                Some(b) => write!(f, "unknown inner NoisePayloadType 0x{b:02x}"),
                None => write!(f, "empty targeted payload (no inner type byte)"),
            },
        }
    }
}

impl std::error::Error for NoiseError {}

impl From<snow::Error> for NoiseError {
    fn from(e: snow::Error) -> Self {
        NoiseError::Noise(e)
    }
}

/// Parse the shared `Noise_XX_25519_ChaChaPoly_SHA256` parameters. Infallible in practice
/// (the string is a compile-time constant), but surfaced as a `Result` so a bad build can
/// never `unwrap`-panic on the hot path.
fn noise_params() -> Result<snow::params::NoiseParams, NoiseError> {
    NOISE_PARAMS.parse().map_err(NoiseError::from)
}

/// SHA-256 fingerprint of a static public key (for out-of-band verification).
pub fn fingerprint_of(public_key: &[u8]) -> [u8; FINGERPRINT_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    hasher.finalize().into()
}

/// A long-term **Curve25519 static keypair** identifying this app's mesh transport session.
///
/// **Not a wallet.** This key authenticates a Noise transport session and nothing else; it
/// is generated independently of (and is never derived from) any Nimiq seed. The public
/// half's SHA-256 is the [`StaticIdentity::fingerprint`] used for out-of-band verification.
#[derive(Clone)]
pub struct StaticIdentity {
    secret: [u8; STATIC_KEY_LEN],
    public: [u8; STATIC_KEY_LEN],
}

impl StaticIdentity {
    /// Generate a fresh random transport identity (OS entropy via `x25519-dalek`).
    pub fn generate() -> Self {
        let secret = x25519_dalek::StaticSecret::random();
        let public = x25519_dalek::PublicKey::from(&secret);
        StaticIdentity {
            secret: secret.to_bytes(),
            public: public.to_bytes(),
        }
    }

    /// Build a transport identity from a known 32-byte Curve25519 secret (deterministic;
    /// used by tests and reproducible setups). The public key is derived via X25519, so it
    /// matches whatever the Noise handshake transmits as the static `s`.
    ///
    /// The `secret` here is a **transport** key — never a wallet seed (those don't exist in
    /// this crate and are G3/G10's money-path concern).
    pub fn from_secret_key(secret: [u8; STATIC_KEY_LEN]) -> Self {
        let sk = x25519_dalek::StaticSecret::from(secret);
        let public = x25519_dalek::PublicKey::from(&sk);
        StaticIdentity {
            secret,
            public: public.to_bytes(),
        }
    }

    /// The 32-byte static **public** key (safe to share / advertise).
    pub fn public_key(&self) -> &[u8; STATIC_KEY_LEN] {
        &self.public
    }

    /// The SHA-256 fingerprint of the static public key (stable, 32 bytes) — compare this
    /// out of band to defeat a man-in-the-middle.
    pub fn fingerprint(&self) -> [u8; FINGERPRINT_LEN] {
        fingerprint_of(&self.public)
    }

    /// Start an XX handshake as the **initiator** using this static identity.
    pub fn start_initiator(&self) -> Result<Handshake, NoiseError> {
        Handshake::new(self, true)
    }

    /// Start an XX handshake as the **responder** using this static identity.
    pub fn start_responder(&self) -> Result<Handshake, NoiseError> {
        Handshake::new(self, false)
    }
}

impl fmt::Debug for StaticIdentity {
    /// Never print the secret. Show only a short fingerprint prefix.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fp = self.fingerprint();
        write!(
            f,
            "StaticIdentity(fp={:02x}{:02x}{:02x}{:02x}..)",
            fp[0], fp[1], fp[2], fp[3]
        )
    }
}

/// An in-progress `Noise_XX` handshake. Drive it message-by-message, then convert the
/// finished state into a [`Session`].
///
/// XX is three messages: initiator `-> e`; responder `-> e, ee, s, es`; initiator
/// `-> s, se`. Both ends learn each other's static key only *after* it is encrypted under
/// the ephemeral DH — that is the identity-hiding property.
pub struct Handshake {
    state: snow::HandshakeState,
}

impl Handshake {
    fn new(identity: &StaticIdentity, initiator: bool) -> Result<Self, NoiseError> {
        let params = noise_params()?;
        let builder = snow::Builder::new(params).local_private_key(&identity.secret);
        let state = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        Ok(Handshake { state })
    }

    /// Whether this is the handshake initiator.
    pub fn is_initiator(&self) -> bool {
        self.state.is_initiator()
    }

    /// Whether all three XX messages have been exchanged and the state is ready to become
    /// a [`Session`].
    pub fn is_finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// Produce the next outgoing handshake message (optionally carrying a small payload —
    /// usually empty for XX).
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
        // Noise messages add at most the DH (32) + tag (16) overhead per token; a generous
        // fixed scratch buffer covers any XX message plus a modest payload.
        let mut buf = vec![0u8; payload.len() + 256];
        let n = self.state.write_message(payload, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume an incoming handshake message, returning any inner payload it carried.
    pub fn read_message(&mut self, message: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; message.len() + 256];
        let n = self.state.read_message(message, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// The remote peer's static public key, available once the handshake has exchanged it.
    pub fn remote_static(&self) -> Option<&[u8]> {
        self.state.get_remote_static()
    }

    /// Finish the handshake, yielding the bidirectional [`Session`]. Errors if the three
    /// XX messages have not all been exchanged yet.
    pub fn into_session(self) -> Result<Session, NoiseError> {
        if !self.state.is_handshake_finished() {
            return Err(NoiseError::HandshakeIncomplete);
        }
        let remote_fingerprint = self.state.get_remote_static().map(fingerprint_of);
        let transport = self.state.into_stateless_transport_mode()?;
        Ok(Session {
            transport,
            send_counter: 0,
            replay: ReplayWindow::new(),
            remote_fingerprint,
        })
    }
}

/// A 1024-message sliding-window anti-replay filter (RFC-6479 style).
///
/// Message counters are **1-based** (the sender's first message is counter `1`), so `0` is
/// never a valid counter and "nothing accepted yet" is unambiguous. The window tracks the
/// highest accepted counter and a [`REPLAY_WINDOW`]-bit bitmap of recently-seen counters;
/// it rejects a counter that is a replay (bit already set) or older than the window.
struct ReplayWindow {
    /// Highest counter accepted so far (`0` = none yet).
    highest: u64,
    /// Bitmap of seen counters within the window, indexed by `counter % REPLAY_WINDOW`.
    bitmap: [u64; (REPLAY_WINDOW / 64) as usize],
}

impl ReplayWindow {
    const BITS: usize = 64;

    fn new() -> Self {
        ReplayWindow {
            highest: 0,
            bitmap: [0u64; (REPLAY_WINDOW / 64) as usize],
        }
    }

    /// Set the bitmap bit for `counter`'s slot.
    fn set(&mut self, counter: u64) {
        let slot = (counter % REPLAY_WINDOW) as usize;
        self.bitmap[slot / Self::BITS] |= 1u64 << (slot % Self::BITS);
    }

    /// Clear the bitmap bit for `counter`'s slot.
    fn clear(&mut self, counter: u64) {
        let slot = (counter % REPLAY_WINDOW) as usize;
        self.bitmap[slot / Self::BITS] &= !(1u64 << (slot % Self::BITS));
    }

    /// Whether `counter`'s slot bit is currently set.
    fn is_set(&self, counter: u64) -> bool {
        let slot = (counter % REPLAY_WINDOW) as usize;
        self.bitmap[slot / Self::BITS] & (1u64 << (slot % Self::BITS)) != 0
    }

    /// Check-and-accept `counter`, returning `false` for a replayed / stale value. Must be
    /// called **after** the AEAD authenticates the message, so a forged counter can never
    /// poison the window.
    fn accept(&mut self, counter: u64) -> bool {
        if counter == 0 {
            return false; // counters are 1-based; 0 is never valid.
        }
        if counter > self.highest {
            // Advance the window: clear every slot we slide past so a future wrap-around
            // counter can't collide with a stale bit, then mark this one.
            let first_new = self.highest + 1;
            let span = (counter - self.highest).min(REPLAY_WINDOW);
            for c in (counter - span + 1).max(first_new)..=counter {
                self.clear(c);
            }
            if counter - self.highest >= REPLAY_WINDOW {
                self.bitmap = [0u64; (REPLAY_WINDOW / 64) as usize];
            }
            self.highest = counter;
            self.set(counter);
            return true;
        }
        // counter <= highest: inside or below the window.
        if self.highest - counter >= REPLAY_WINDOW {
            return false; // too old — outside the window.
        }
        if self.is_set(counter) {
            return false; // already seen — replay.
        }
        self.set(counter);
        true
    }
}

/// An established bidirectional Noise transport session: a sending cipher state with a
/// monotonic counter, and a receiving cipher state guarded by the [`ReplayWindow`].
///
/// **Stateless cipher states** (the `snow` "stateless" transport): the per-message counter
/// is carried explicitly on the wire (the 8-byte nonce prefix) so the receiver can run the
/// replay filter. The two directions use independent ChaChaPoly keys derived by the XX
/// handshake.
pub struct Session {
    transport: snow::StatelessTransportState,
    /// Monotonic 1-based counter for *outgoing* messages (the nonce we encrypt under).
    send_counter: u64,
    /// Sliding-window guard over *incoming* counters.
    replay: ReplayWindow,
    /// The remote peer's static-key fingerprint (for out-of-band verification).
    remote_fingerprint: Option<[u8; FINGERPRINT_LEN]>,
}

impl Session {
    /// The remote peer's static-key fingerprint, captured at handshake completion.
    pub fn remote_fingerprint(&self) -> Option<[u8; FINGERPRINT_LEN]> {
        self.remote_fingerprint
    }

    /// Encrypt `plaintext` into a self-describing sealed blob `nonce(8) || ciphertext||tag`.
    ///
    /// Each call advances the send counter, so two identical plaintexts never produce the
    /// same blob and the receiver's replay window stays monotonic.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        self.send_counter += 1;
        let counter = self.send_counter;
        let mut cipher = vec![0u8; plaintext.len() + NOISE_TAG_LEN];
        let n = self
            .transport
            .write_message(counter, plaintext, &mut cipher)?;
        cipher.truncate(n);
        let mut out = Vec::with_capacity(NONCE_PREFIX_LEN + n);
        out.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&cipher);
        Ok(out)
    }

    /// Decrypt a sealed blob produced by the peer's [`Session::encrypt`].
    ///
    /// Authenticates **first** (the AEAD verifies the nonce + ciphertext), then runs the
    /// replay guard — so a forged or replayed nonce cannot disturb the window.
    pub fn decrypt(&mut self, sealed: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if sealed.len() < MIN_SEALED_LEN {
            return Err(NoiseError::TooShort { got: sealed.len() });
        }
        let mut nonce_bytes = [0u8; NONCE_PREFIX_LEN];
        nonce_bytes.copy_from_slice(&sealed[..NONCE_PREFIX_LEN]);
        let counter = u64::from_be_bytes(nonce_bytes);
        let cipher = &sealed[NONCE_PREFIX_LEN..];
        let mut plain = vec![0u8; cipher.len()];
        // Authenticate + decrypt before touching the replay window.
        let n = self.transport.read_message(counter, cipher, &mut plain)?;
        plain.truncate(n);
        if !self.replay.accept(counter) {
            return Err(NoiseError::Replay { counter });
        }
        Ok(plain)
    }

    /// Seal an *optional encrypted memo* — the blob that rides a `nimiqTx` packet's
    /// `encMemo` TLV (`0x05`, [`crate::envelope`]). Just a direct [`Session::encrypt`].
    pub fn seal_memo(&mut self, memo: &[u8]) -> Result<Vec<u8>, NoiseError> {
        self.encrypt(memo)
    }

    /// Open an encrypted memo recovered from a packet's `encMemo` TLV.
    pub fn open_memo(&mut self, blob: &[u8]) -> Result<Vec<u8>, NoiseError> {
        self.decrypt(blob)
    }

    /// Seal a **targeted payload** for a `noiseEncrypted` (`0x11`) message: the plaintext is
    /// `innerType(1) || inner`, then encrypted. Use [`NoisePayloadType::Chat`] for a 1:1
    /// encrypted chat message.
    pub fn seal_payload(
        &mut self,
        inner_type: NoisePayloadType,
        inner: &[u8],
    ) -> Result<Vec<u8>, NoiseError> {
        let mut plaintext = Vec::with_capacity(1 + inner.len());
        plaintext.push(inner_type.to_u8());
        plaintext.extend_from_slice(inner);
        self.encrypt(&plaintext)
    }

    /// Open a targeted payload sealed by [`Session::seal_payload`], returning the inner type
    /// and the inner bytes. The inner bytes stay **opaque** — a `nimiqTx` inner payload is
    /// never signed or broadcast here (money-path is G3/G8).
    pub fn open_payload(&mut self, blob: &[u8]) -> Result<(NoisePayloadType, Vec<u8>), NoiseError> {
        let plaintext = self.decrypt(blob)?;
        let Some((&type_byte, inner)) = plaintext.split_first() else {
            return Err(NoiseError::BadPayloadType { found: None });
        };
        let inner_type =
            NoisePayloadType::from_u8(type_byte).ok_or(NoiseError::BadPayloadType {
                found: Some(type_byte),
            })?;
        Ok((inner_type, inner.to_vec()))
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("send_counter", &self.send_counter)
            .field("highest_recv", &self.replay.highest)
            .finish()
    }
}

/// Run a full XX handshake between an initiator and a responder identity, returning both
/// finished sessions. Shared by the tests and any caller that drives both ends locally.
pub fn handshake_xx(
    initiator: &StaticIdentity,
    responder: &StaticIdentity,
) -> Result<(Session, Session), NoiseError> {
    let mut ini = initiator.start_initiator()?;
    let mut res = responder.start_responder()?;

    // msg1: initiator -> responder (e)
    let m1 = ini.write_message(&[])?;
    res.read_message(&m1)?;
    // msg2: responder -> initiator (e, ee, s, es)
    let m2 = res.write_message(&[])?;
    ini.read_message(&m2)?;
    // msg3: initiator -> responder (s, se)
    let m3 = ini.write_message(&[])?;
    res.read_message(&m3)?;

    Ok((ini.into_session()?, res.into_session()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{decode_envelope, encode_envelope, NimiqEnvelope};

    // A fixed transport secret → deterministic identity (NOT a wallet seed).
    fn id_from(byte: u8) -> StaticIdentity {
        StaticIdentity::from_secret_key([byte; STATIC_KEY_LEN])
    }

    #[test]
    fn xx_handshake_completes_between_two_parties() {
        let alice = id_from(0x11);
        let bob = id_from(0x22);
        let (a, b) = handshake_xx(&alice, &bob).unwrap();
        // Each side learned the other's static fingerprint (mutual auth) and it matches the
        // out-of-band identity fingerprint.
        assert_eq!(a.remote_fingerprint(), Some(bob.fingerprint()));
        assert_eq!(b.remote_fingerprint(), Some(alice.fingerprint()));
    }

    #[test]
    fn fingerprint_is_stable_and_32_bytes() {
        let id = id_from(0x42);
        let fp1 = id.fingerprint();
        let fp2 = id.fingerprint();
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), FINGERPRINT_LEN);
        assert_eq!(FINGERPRINT_LEN, 32);
        // A generated identity's fingerprint is also exactly 32 bytes and stable.
        let g = StaticIdentity::generate();
        assert_eq!(g.fingerprint(), g.fingerprint());
        assert_eq!(g.fingerprint().len(), 32);
        // The fingerprint is the SHA-256 of the public key (out-of-band verification).
        assert_eq!(id.fingerprint(), fingerprint_of(id.public_key()));
    }

    #[test]
    fn memo_round_trip_through_envelope() {
        let (mut a, mut b) = handshake_xx(&id_from(1), &id_from(2)).unwrap();
        let memo = b"lunch was 12 NIM, thanks!";
        let sealed = a.seal_memo(memo).unwrap();

        // The encrypted memo rides a nimiqTx envelope's encMemo TLV (0x05), opaque.
        let mut env = NimiqEnvelope::new(vec![0xAB; 80]);
        env.enc_memo = Some(sealed);
        let bytes = encode_envelope(&env).unwrap();
        let back = decode_envelope(&bytes).unwrap();
        let recovered = back.enc_memo.expect("encMemo present");

        let plain = b.open_memo(&recovered).unwrap();
        assert_eq!(plain, memo);
    }

    #[test]
    fn chat_payload_round_trip() {
        let (mut a, mut b) = handshake_xx(&id_from(3), &id_from(4)).unwrap();
        let msg = b"hey, are you near the gateway?";
        let blob = a.seal_payload(NoisePayloadType::Chat, msg).unwrap();
        let (ty, inner) = b.open_payload(&blob).unwrap();
        assert_eq!(ty, NoisePayloadType::Chat);
        assert_eq!(inner, msg);

        // Bidirectional: reply on the other cipher state.
        let reply = b
            .seal_payload(NoisePayloadType::Chat, b"yes, two rooms over")
            .unwrap();
        let (ty2, inner2) = a.open_payload(&reply).unwrap();
        assert_eq!(ty2, NoisePayloadType::Chat);
        assert_eq!(inner2, b"yes, two rooms over");
    }

    #[test]
    fn targeted_nimiq_tx_inner_type_round_trips_opaque() {
        let (mut a, mut b) = handshake_xx(&id_from(5), &id_from(6)).unwrap();
        // Option B: a private/targeted nimiqTx — inner bytes are OPAQUE (never signed here).
        let opaque_tx = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let blob = a
            .seal_payload(NoisePayloadType::NimiqTx, &opaque_tx)
            .unwrap();
        let (ty, inner) = b.open_payload(&blob).unwrap();
        assert_eq!(ty, NoisePayloadType::NimiqTx);
        assert_eq!(inner, opaque_tx);
    }

    #[test]
    fn replay_window_rejects_a_replayed_ciphertext() {
        let (mut a, mut b) = handshake_xx(&id_from(7), &id_from(8)).unwrap();
        let blob = a.encrypt(b"once").unwrap();
        // First delivery decrypts fine …
        assert_eq!(b.decrypt(&blob).unwrap(), b"once");
        // … a replay of the exact same ciphertext is rejected.
        match b.decrypt(&blob) {
            Err(NoiseError::Replay { counter }) => assert_eq!(counter, 1),
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[test]
    fn replay_window_accepts_out_of_order_but_not_duplicates() {
        let (mut a, mut b) = handshake_xx(&id_from(9), &id_from(10)).unwrap();
        let m1 = a.encrypt(b"one").unwrap();
        let m2 = a.encrypt(b"two").unwrap();
        let m3 = a.encrypt(b"three").unwrap();
        // Deliver out of order: 3, 1, 2 all succeed (within the window).
        assert_eq!(b.decrypt(&m3).unwrap(), b"three");
        assert_eq!(b.decrypt(&m1).unwrap(), b"one");
        assert_eq!(b.decrypt(&m2).unwrap(), b"two");
        // But re-delivering any of them is a replay.
        assert!(matches!(b.decrypt(&m2), Err(NoiseError::Replay { .. })));
        assert!(matches!(b.decrypt(&m1), Err(NoiseError::Replay { .. })));
    }

    #[test]
    fn replay_window_rejects_too_old_counter() {
        let mut w = ReplayWindow::new();
        // Accept a counter far ahead, advancing the window past the origin.
        assert!(w.accept(REPLAY_WINDOW + 100));
        // A counter older than the window is rejected (stale), even though never seen.
        assert!(!w.accept(1));
        assert!(!w.accept(50));
        // A counter just inside the window is still accepted.
        assert!(w.accept(REPLAY_WINDOW + 100 - (REPLAY_WINDOW - 1)));
        // Counter 0 is never valid.
        assert!(!w.accept(0));
    }

    #[test]
    fn wrong_static_key_fails_to_decrypt() {
        // Two independent handshakes → unrelated session keys.
        let (mut a1, _b1) = handshake_xx(&id_from(11), &id_from(12)).unwrap();
        let (_a2, mut b2) = handshake_xx(&id_from(13), &id_from(14)).unwrap();
        let blob = a1.encrypt(b"secret").unwrap();
        // b2 belongs to a different session (different static keys) → AEAD auth fails.
        match b2.decrypt(&blob) {
            Err(NoiseError::Noise(_)) => {}
            other => panic!("expected a Noise AEAD failure, got {other:?}"),
        }
    }

    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let (mut a, mut b) = handshake_xx(&id_from(15), &id_from(16)).unwrap();
        let mut blob = a.encrypt(b"integrity matters").unwrap();
        // Flip a bit in the ciphertext body (past the 8-byte nonce prefix).
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(b.decrypt(&blob).is_err());
    }

    #[test]
    fn decrypt_rejects_too_short_blob() {
        let (_a, mut b) = handshake_xx(&id_from(17), &id_from(18)).unwrap();
        assert!(matches!(
            b.decrypt(&[0u8; MIN_SEALED_LEN - 1]),
            Err(NoiseError::TooShort { .. })
        ));
    }

    #[test]
    fn open_payload_rejects_unknown_inner_type() {
        let (mut a, mut b) = handshake_xx(&id_from(19), &id_from(20)).unwrap();
        // Seal a plaintext whose first byte is not a known NoisePayloadType.
        let blob = a.encrypt(&[0x7F, 0xAA, 0xBB]).unwrap();
        match b.open_payload(&blob) {
            Err(NoiseError::BadPayloadType { found: Some(0x7F) }) => {}
            other => panic!("expected BadPayloadType, got {other:?}"),
        }
        // And an empty plaintext has no inner-type byte at all.
        let empty = a.encrypt(&[]).unwrap();
        assert!(matches!(
            b.open_payload(&empty),
            Err(NoiseError::BadPayloadType { found: None })
        ));
    }

    #[test]
    fn into_session_before_finish_errors() {
        let alice = id_from(21);
        let hs = alice.start_initiator().unwrap();
        assert!(!hs.is_finished());
        assert!(matches!(
            hs.into_session(),
            Err(NoiseError::HandshakeIncomplete)
        ));
    }

    #[test]
    fn payload_type_byte_mapping() {
        assert_eq!(NoisePayloadType::Chat.to_u8(), 0x01);
        assert_eq!(NoisePayloadType::NimiqTx.to_u8(), 0x04);
        assert_eq!(NoisePayloadType::NimiqTxReceipt.to_u8(), 0x05);
        assert_eq!(
            NoisePayloadType::from_u8(0x04),
            Some(NoisePayloadType::NimiqTx)
        );
        assert_eq!(NoisePayloadType::from_u8(0x00), None);
    }
}
