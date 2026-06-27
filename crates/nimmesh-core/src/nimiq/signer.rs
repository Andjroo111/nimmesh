//! # nimiq::signer — the pluggable `KeyOrigin` seam (offline TESTNET signing)
//!
//! Andjroo's decision (LOOP.md / RISKS.md #5): support **both** key origins behind one
//! seam, so the mesh layer never learns where a signature came from — it only ever sees a
//! finished, broadcast-safe [`SignedTransfer`].
//!
//! - [`AppSigner`] — the **offline-first** path. It builds the [`Transfer`], asks an
//!   [`EnclaveKey`] (a `with_foreign` trait the native Secure Enclave / Android Keystore
//!   implements) to sign the 67-byte content, and assembles the 139-byte wire blob. The
//!   **secret never crosses the FFI boundary**: only a public key comes *out* and a
//!   64-byte signature comes *out*; the seed stays in the enclave behind an opaque handle.
//!   [`InMemoryEnclaveKey`] is the in-process reference impl used by tests/dev (NOT
//!   FFI-exported, so no secret-bearing constructor is ever on the wire surface).
//! - [`DelegatedSigner`] — a `with_foreign` seam the native layer implements by calling
//!   **Nimiq Pay / Hub**, returning a pre-signed serialized blob. Tested with a mock that
//!   replays a known-good fixture blob. See the `// NATIVE:` note for the real
//!   sign-but-DON'T-broadcast SDK path verified on-device later (G8 owns broadcast).
//!
//! Both feed the internal [`KeyOrigin`] trait, whose `SignedTransfer.raw_hex` flows into
//! [`crate::node::MeshNode::submit_signed_transfer`] and rides the existing mesh path as
//! opaque bytes downstream. **Testnet-only, money-path / Andjroo-gated. No broadcast here.**

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};

use super::address::{Address, AddressError};
use super::hex::{bytes_to_hex, hex_to_bytes};
use super::tx::{Transfer, PUBLIC_KEY_LEN, SIGNATURE_LEN};
use crate::NetworkId;

/// Albatross validity window in blocks (~2 h at 1 block/s). RISKS.md #1: a signed tx is
/// dropped by honest nodes past `vsh + VALIDITY_WINDOW`, so the UI must surface an expiry.
pub const VALIDITY_WINDOW: u32 = 7200;

/// A request to sign a basic transfer. The fee is always 0 (G3 basic format) and the data
/// field is empty. `network` defaults to testnet via [`TransferIntent::testnet`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TransferIntent {
    /// Recipient in user-friendly `NQ…` form (validated + canonicalized at sign time).
    pub recipient: String,
    /// Amount in luna (1 NIM = 100_000 luna).
    pub value: u64,
    /// `validityStartHeight` — anchor to the latest known head (RISKS.md #1).
    pub validity_start_height: u32,
    /// Target network. The loop only ever builds [`NetworkId::Testnet`].
    pub network: NetworkId,
}

/// A finished, broadcast-safe signed transfer — the only thing that leaves the signer.
///
/// `raw_hex` is the self-contained 139-byte `Basic`-format blob (GOAL.md "~139-byte blob");
/// `tx_hash` is its canonical `Blake2b-256(serializeContent)` id. The validity bounds let
/// the UI show a countdown and the gateway reject an expired tx (RISKS.md #1).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SignedTransfer {
    /// The full serialized transaction as lowercase hex (`tx.serialize()`).
    pub raw_hex: String,
    /// The transaction hash as lowercase hex (`tx.hash()`).
    pub tx_hash: String,
    /// The `validityStartHeight` the tx was anchored to.
    pub validity_start_height: u32,
    /// `validity_start_height + VALIDITY_WINDOW`: past this, honest nodes drop the tx.
    pub valid_until_height: u32,
}

/// A failure producing a [`SignedTransfer`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum SignError {
    /// The recipient address was malformed or failed its checksum.
    BadAddress { reason: String },
    /// The enclave returned a key/signature of the wrong length (corrupt seam).
    BadKeyMaterial { reason: String },
    /// The delegated (Nimiq Pay / Hub) signer rejected or failed the request.
    Delegated { reason: String },
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignError::BadAddress { reason } => write!(f, "bad recipient address: {reason}"),
            SignError::BadKeyMaterial { reason } => write!(f, "bad key material: {reason}"),
            SignError::Delegated { reason } => write!(f, "delegated signer failed: {reason}"),
        }
    }
}

impl std::error::Error for SignError {}

impl From<AddressError> for SignError {
    fn from(e: AddressError) -> Self {
        SignError::BadAddress {
            reason: e.to_string(),
        }
    }
}

/// The seam every key origin satisfies. Internal (not FFI-exported): the native layer talks
/// to the concrete [`AppSigner`] / [`DelegatedSigner`], the mesh layer talks to
/// [`SignedTransfer`]. Both worlds meet here.
pub trait KeyOrigin {
    /// Sign `intent` and return the broadcast-safe blob. Never exposes key material.
    fn sign_transfer(&self, intent: &TransferIntent) -> Result<SignedTransfer, SignError>;
}

/// Assemble a [`SignedTransfer`] from the wire bytes built by either origin. Shared so the
/// validity-window math and hex rendering live in exactly one place.
fn finished(transfer: &Transfer, public_key: &[u8; 32], signature: &[u8; 64]) -> SignedTransfer {
    let raw = transfer.serialize_basic(public_key, signature);
    SignedTransfer {
        raw_hex: bytes_to_hex(&raw),
        tx_hash: bytes_to_hex(&transfer.tx_hash()),
        validity_start_height: transfer.validity_start_height,
        valid_until_height: transfer
            .validity_start_height
            .saturating_add(VALIDITY_WINDOW),
    }
}

// --- EnclaveKey: the native key seam (the secret stays behind this trait) ----------------

/// The opaque native key seam: the Secure Enclave / Android Keystore implements this so the
/// **seed never crosses the FFI boundary**. Only a public key leaves (to derive the sender
/// address) and a detached 64-byte Ed25519 signature leaves (over the content bytes). The
/// app passes an opaque enclave *handle* when it constructs the native impl — never bytes.
///
/// `// NATIVE:` on iOS the real impl signs via a non-exportable `SecKey`; on Android via a
/// hardware-backed `KeyStore` entry. Both are verified on-device; here we model the shape.
#[uniffi::export(with_foreign)]
pub trait EnclaveKey: Send + Sync {
    /// The 32-byte Ed25519 public key (used only to derive the sender address; safe to leak).
    fn public_key(&self) -> Vec<u8>;
    /// Sign `content` (the 67-byte `serializeContent`) and return the 64-byte signature.
    /// The private key stays in the enclave; only the signature crosses back.
    fn sign_content(&self, content: Vec<u8>) -> Vec<u8>;
}

/// In-process reference [`EnclaveKey`] for tests/dev ONLY. **Not** FFI-exported: a
/// secret-bearing constructor must never appear on the UniFFI surface (the seed never
/// crosses FFI). Production uses a native enclave-backed impl instead.
pub struct InMemoryEnclaveKey {
    signing_key: SigningKey,
}

impl InMemoryEnclaveKey {
    /// Build from a 32-byte Ed25519 secret seed (Rust-side test/dev only).
    pub fn from_secret(secret: &[u8; 32]) -> Self {
        InMemoryEnclaveKey {
            signing_key: SigningKey::from_bytes(secret),
        }
    }
}

impl EnclaveKey for InMemoryEnclaveKey {
    fn public_key(&self) -> Vec<u8> {
        self.signing_key.verifying_key().to_bytes().to_vec()
    }

    fn sign_content(&self, content: Vec<u8>) -> Vec<u8> {
        self.signing_key.sign(&content).to_bytes().to_vec()
    }
}

// --- AppSigner: the offline-first, app-managed-enclave origin ----------------------------

/// Signs locally with an [`EnclaveKey`]. The offline-first path: no network, no wallet
/// round-trip — exactly what airplane-mode origination needs (GOAL.md "the magic moment").
#[derive(uniffi::Object)]
pub struct AppSigner {
    key: Arc<dyn EnclaveKey>,
}

#[uniffi::export]
impl AppSigner {
    /// Construct over a native (or test) [`EnclaveKey`]. The signer holds only the opaque
    /// key seam — never seed bytes.
    #[uniffi::constructor]
    pub fn new(enclave_key: Arc<dyn EnclaveKey>) -> Arc<Self> {
        Arc::new(AppSigner { key: enclave_key })
    }

    /// This signer's own user-friendly NQ address (derived from the enclave public key).
    pub fn address(&self) -> Result<String, SignError> {
        Ok(self.sender_address()?.to_user_friendly())
    }

    /// Sign a transfer locally. The seed stays in the enclave; out comes a [`SignedTransfer`].
    pub fn sign_transfer(&self, intent: TransferIntent) -> Result<SignedTransfer, SignError> {
        KeyOrigin::sign_transfer(self, &intent)
    }
}

impl AppSigner {
    /// Pull + validate the 32-byte public key from the enclave seam.
    fn public_key(&self) -> Result<[u8; PUBLIC_KEY_LEN], SignError> {
        let pk = self.key.public_key();
        pk.try_into()
            .map_err(|v: Vec<u8>| SignError::BadKeyMaterial {
                reason: format!("public key must be {PUBLIC_KEY_LEN} bytes, got {}", v.len()),
            })
    }

    /// Derive the sender address from the enclave public key.
    fn sender_address(&self) -> Result<Address, SignError> {
        Ok(Address::from_public_key(&self.public_key()?))
    }
}

impl KeyOrigin for AppSigner {
    fn sign_transfer(&self, intent: &TransferIntent) -> Result<SignedTransfer, SignError> {
        let public_key = self.public_key()?;
        let sender = Address::from_public_key(&public_key);
        let recipient = Address::from_user_friendly(&intent.recipient)?;
        let transfer = Transfer {
            sender,
            recipient,
            value: intent.value,
            fee: 0, // G3: basic transfers are fee-0.
            validity_start_height: intent.validity_start_height,
            network_id: intent.network.wire_id(),
        };
        let content = transfer.serialize_content();
        let signature: [u8; SIGNATURE_LEN] =
            self.key
                .sign_content(content)
                .try_into()
                .map_err(|v: Vec<u8>| SignError::BadKeyMaterial {
                    reason: format!("signature must be {SIGNATURE_LEN} bytes, got {}", v.len()),
                })?;
        Ok(finished(&transfer, &public_key, &signature))
    }
}

// --- DelegatedSigner: the external-wallet (Nimiq Pay / Hub) origin -----------------------

/// A `with_foreign` seam the native layer implements by delegating to **Nimiq Pay / Hub**:
/// it builds + signs the transfer inside the wallet and returns the finished
/// [`SignedTransfer`] blob. The Rust core never sees the wallet's key.
///
/// `// NATIVE:` the real impl calls the Nimiq mini-app SDK / Hub to **sign but NOT
/// broadcast** (broadcast is the gateway's job at G8), then returns the serialized blob.
/// That path is verified on-device later; here we exercise it with a mock that replays a
/// known-good fixture blob, proving the seam carries the bytes faithfully.
#[uniffi::export(with_foreign)]
pub trait DelegatedSigner: Send + Sync {
    /// Sign `intent` via the external wallet and return the broadcast-safe blob.
    fn sign_transfer_delegated(&self, intent: TransferIntent) -> Result<SignedTransfer, SignError>;
}

impl KeyOrigin for Arc<dyn DelegatedSigner> {
    fn sign_transfer(&self, intent: &TransferIntent) -> Result<SignedTransfer, SignError> {
        self.sign_transfer_delegated(intent.clone())
    }
}

/// Decode a [`SignedTransfer`]'s `raw_hex` into the opaque wire bytes the mesh floods.
/// Errors if the blob is not valid hex (it always is, coming from our own signer).
pub fn signed_transfer_wire(signed: &SignedTransfer) -> Result<Vec<u8>, SignError> {
    hex_to_bytes(&signed.raw_hex).map_err(|e| SignError::BadKeyMaterial {
        reason: format!("signed raw_hex is not valid hex: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(fill: impl Fn(usize) -> u8) -> [u8; 32] {
        let mut s = [0u8; 32];
        for (i, b) in s.iter_mut().enumerate() {
            *b = fill(i);
        }
        s
    }

    #[test]
    fn app_signer_round_trips_address_and_signs() {
        let key = Arc::new(InMemoryEnclaveKey::from_secret(&secret(|i| (i + 1) as u8)));
        let signer = AppSigner::new(key);
        // Sender address derived from the enclave key matches the @nimiq/core fixture.
        assert_eq!(
            signer.address().unwrap(),
            "NQ65 9M6T V4BM 8JQ7 J8HL H9KB KH5M LNJY 6JLY"
        );

        let intent = TransferIntent {
            recipient: "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".to_string(),
            value: 100000,
            validity_start_height: 1234,
            network: NetworkId::Testnet,
        };
        let signed = signer.sign_transfer(intent).unwrap();
        assert_eq!(signed.raw_hex.len() / 2, 139);
        assert_eq!(
            signed.tx_hash,
            "4df874dac3672974f31bc64d022d89ee8d744ac72ebc6be263663561fb479274"
        );
        assert_eq!(signed.validity_start_height, 1234);
        assert_eq!(signed.valid_until_height, 1234 + VALIDITY_WINDOW);
        // The wire bytes decode cleanly for the mesh path.
        assert_eq!(signed_transfer_wire(&signed).unwrap().len(), 139);
    }

    #[test]
    fn bad_recipient_is_rejected() {
        let key = Arc::new(InMemoryEnclaveKey::from_secret(&secret(|i| (i + 1) as u8)));
        let signer = AppSigner::new(key);
        let intent = TransferIntent {
            recipient: "NQ00 NOT A REAL ADDR".to_string(),
            value: 1,
            validity_start_height: 0,
            network: NetworkId::Testnet,
        };
        assert!(matches!(
            signer.sign_transfer(intent),
            Err(SignError::BadAddress { .. })
        ));
    }

    #[test]
    fn delegated_signer_seam_carries_blob() {
        // A mock external wallet that replays a known-good fixture blob.
        struct MockWallet {
            blob: SignedTransfer,
        }
        impl DelegatedSigner for MockWallet {
            fn sign_transfer_delegated(
                &self,
                _intent: TransferIntent,
            ) -> Result<SignedTransfer, SignError> {
                Ok(self.blob.clone())
            }
        }
        let fixture = SignedTransfer {
            raw_hex: "00".repeat(139),
            tx_hash: "ab".repeat(32),
            validity_start_height: 10,
            valid_until_height: 10 + VALIDITY_WINDOW,
        };
        let wallet: Arc<dyn DelegatedSigner> = Arc::new(MockWallet {
            blob: fixture.clone(),
        });
        let intent = TransferIntent {
            recipient: "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".to_string(),
            value: 5,
            validity_start_height: 10,
            network: NetworkId::Testnet,
        };
        let out = wallet.sign_transfer(&intent).unwrap();
        assert_eq!(out, fixture);
        assert_eq!(signed_transfer_wire(&out).unwrap().len(), 139);
    }
}
