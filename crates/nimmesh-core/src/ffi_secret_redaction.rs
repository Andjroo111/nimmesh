//! G11 / #82 — the FFI door's secret inventory: hand-written `Debug` for every record that
//! carries key material into the core, so a secret can never reach a log.
//!
//! The live/participant/send configs each take raw secrets across UniFFI — an Ed25519 seed that
//! redeems a NIM HTLC, secp256k1 secrets that escrow and spend USDC, the CSPRNG seed that is the
//! master for the per-swap secret `S`. They are `uniffi::Record`s, so the app is free to `print`,
//! `dbg!`, or `os_log` one; a `#[derive(Debug)]` would render the raw bytes at any of those, and
//! a leaked seed is a stolen swap (an attacker who learns the initiator's `S` master claims the
//! counterparty leg). Logs are not a trust boundary: they get written to disk, shipped to crash
//! reporters, and pasted into issues.
//!
//! So the derives are replaced by the impls below, and they live TOGETHER rather than next to
//! each struct: this file is the one auditable list of what counts as secret at the door, and
//! `ffi_secret_redaction_tests.rs` holds the regression that fails the moment a `Debug` derive
//! comes back or a new secret field renders itself.
//!
//! The rule: a field holding key material renders as [`Redacted`] — its LENGTH only (public, and
//! the field's most common bug), never a prefix, never a hash. Public fields render normally;
//! redaction that blinds the operator to a bad endpoint just gets reverted later.

use std::fmt;

use crate::swap_live_ffi::{FfiLiveInitiatorConfig, FfiLiveResponderConfig};
use crate::swap_participant_ffi::FfiParticipantConfig;
use crate::usdc_send_ffi::FfiUsdcSendConfig;

/// A secret field's stand-in inside a `Debug` rendering: prints `<redacted N bytes>` and nothing
/// of the value. Holds the length only, so it cannot leak what it is standing in for.
pub(crate) struct Redacted(pub(crate) usize);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted {} bytes>", self.0)
    }
}

impl fmt::Debug for FfiLiveInitiatorConfig {
    /// Never print `intent_seed` (the per-swap secret master — leaking it hands an attacker
    /// every `S` this node will ever draw) or `evm_gas_secret` (a spendable Amoy key).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiLiveInitiatorConfig")
            .field("nim_luna", &self.nim_luna)
            .field("usdc_micro", &self.usdc_micro)
            .field("expiry_height", &self.expiry_height)
            .field("intent_seed", &Redacted(self.intent_seed.len()))
            .field("evm_claim_address", &self.evm_claim_address)
            .field("evm_gas_secret", &Redacted(self.evm_gas_secret.len()))
            .field("nim_rpc_url", &self.nim_rpc_url)
            .field("amoy_rpc_url", &self.amoy_rpc_url)
            .field("htlc_address", &self.htlc_address)
            .field("delta_safe_blocks", &self.delta_safe_blocks)
            .field("min_claim_window_blocks", &self.min_claim_window_blocks)
            .finish()
    }
}

impl fmt::Debug for FfiLiveResponderConfig {
    /// Never print `nim_claim_seed` (the key that redeems this node's NIM HTLC) or
    /// `evm_funding_secret` (the FUNDED Amoy account that escrows the USDC).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiLiveResponderConfig")
            .field("usdc_micro", &self.usdc_micro)
            .field("nim_luna", &self.nim_luna)
            .field("expiry_height", &self.expiry_height)
            .field("nim_claim_seed", &Redacted(self.nim_claim_seed.len()))
            .field(
                "evm_funding_secret",
                &Redacted(self.evm_funding_secret.len()),
            )
            .field("nim_rpc_url", &self.nim_rpc_url)
            .field("amoy_rpc_url", &self.amoy_rpc_url)
            .field("htlc_address", &self.htlc_address)
            .field("usdc_address", &self.usdc_address)
            .field("delta_safe_blocks", &self.delta_safe_blocks)
            .field("min_claim_window_blocks", &self.min_claim_window_blocks)
            .finish()
    }
}

impl fmt::Debug for FfiParticipantConfig {
    /// Never print `intent_seed` — the ephemeral identity's seed AND the per-swap secret master.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiParticipantConfig")
            .field("btc_pubkey", &self.btc_pubkey)
            .field("btc_address", &self.btc_address)
            .field("max_concurrent_swaps", &self.max_concurrent_swaps)
            .field("delta_safe_blocks", &self.delta_safe_blocks)
            .field("min_claim_window_blocks", &self.min_claim_window_blocks)
            .field("standing_intent", &self.standing_intent)
            .field("intent_seed", &Redacted(self.intent_seed.len()))
            .finish()
    }
}

impl fmt::Debug for FfiUsdcSendConfig {
    /// Never print `source_secret` — the spendable key of the account holding the USDC.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiUsdcSendConfig")
            .field("source_secret", &Redacted(self.source_secret.len()))
            .field("to_address", &self.to_address)
            .field("amount_micro", &self.amount_micro)
            .field("rpc_url", &self.rpc_url)
            .finish()
    }
}

#[cfg(test)]
#[path = "ffi_secret_redaction_tests.rs"]
mod tests;
