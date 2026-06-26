//! # nimiq — offline Nimiq Albatross signing (G3, TESTNET, money-path / gated)
//!
//! The money-critical layer the rest of the core was built to carry: turn a payment
//! *intent* into a self-contained, self-authenticating **signed transaction blob** that
//! can ride the BLE mesh and be broadcast at the final hop (GOAL.md north star). Pure
//! offline crypto — **no network, no RPC, no broadcast** (that is G8); only public,
//! broadcast-safe bytes ever leave this module (core value #1).
//!
//! Layers, bottom-up:
//! - [`hex`] — pure hex + big-endian byte helpers (ported from the fleet's `sendhome`).
//! - [`address`] — the 20-byte address + its user-friendly `NQ`-IBAN base32 codec.
//! - [`tx`] — byte-exact `serializeContent` (67 B) + full `Basic`-format wire (139 B) +
//!   the `Blake2b-256` tx hash, asserted equal to `@nimiq/core` against committed fixtures.
//! - [`signer`] — the pluggable [`signer::KeyOrigin`] seam: [`signer::AppSigner`] (offline,
//!   app-enclave) and [`signer::DelegatedSigner`] (Nimiq Pay / Hub), both emitting a
//!   [`signer::SignedTransfer`]. The seed never crosses the FFI boundary.
//!
//! **Testnet-by-default, Andjroo-gated.** Nothing here broadcasts; the signed blob is handed
//! to the mesh via [`crate::node::MeshNode::submit_signed_transfer`] as opaque bytes.

pub mod address;
pub mod hex;
pub mod signer;
pub mod tx;

pub use address::{Address, AddressError};
pub use signer::{
    AppSigner, DelegatedSigner, EnclaveKey, KeyOrigin, SignError, SignedTransfer, TransferIntent,
    VALIDITY_WINDOW,
};
pub use tx::Transfer;
