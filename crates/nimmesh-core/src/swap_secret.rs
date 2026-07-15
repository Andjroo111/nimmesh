//! G11 (#82): the ONE place a swap seed is drawn, health-checked, and stretched into per-swap
//! secrets. One module = one auditable entropy inventory, mirroring `ffi_secret_redaction`.
//!
//! ## Why a gate exists at all
//! A live node's `intent_seed` is doubly load-bearing: it is the PRF master every per-swap secret
//! `S` is derived from, AND (via `InMemoryEnclaveKey::from_secret`) the Ed25519 key that IS the
//! node's swap identity. Until this module the doors validated it for **length only**.
//!
//! An all-zero seed is not a hypothetical. It is exactly what a swallowed CSPRNG error produces:
//! the caller allocates a zero-filled buffer, the RNG fails, the status is discarded, and the
//! zeros are used. With a zero seed every `S` is `sha256(master ‖ swap_id ‖ label)` over a
//! *publicly known* master — and `swap_id` travels on the wire in cleartext. Any relay that sees a
//! Propose recomputes `S` and claims the counterparty leg before the honest party does. That is
//! S1, the CRITICAL theft this agenda was built to close, reopened through the app's RNG.
//!
//! [`crate::swap_session::SwapSession::live_safety`]'s C1 gate cannot catch it: it refuses a
//! session whose secret source is still the sim default, but a zero-seeded PRF *is* a replacement,
//! so `secret_is_sim` flips false and the gate passes. C1 checks **that** you replaced the sim
//! source, not that what you replaced it with carries entropy. This module checks the entropy.
//!
//! ## What the health check can and cannot do
//! [`check_seed_entropy`] is a **health test in the NIST SP 800-90B sense** (a repetition /
//! stuck-output check), not an entropy estimator — no function of 32 bytes can be. It catches the
//! catastrophic and canonical failures: an unwritten buffer, a latched hardware RNG, a hand-typed
//! placeholder. It cannot detect a seed from a well-formed but weak PRNG. So the check is the
//! **backstop**, not the fix; the fix is [`draw_swap_seed`] — Rust drawing the seed itself, so a
//! caller never has to be trusted to do it right.
//!
//! The floor is on *distinct byte values* (≥ [`MIN_DISTINCT_BYTES`]). For a real 32-byte CSPRNG
//! draw the expected distinct count is ≈ 30.2; the union bound on landing inside any 8-value
//! alphabet is `C(256,8) · (8/256)^32 ≈ 2^-111`, so the false-reject rate is unreachable in
//! practice while every degenerate seed above is caught. `every_drawn_seed_clears_the_gate` pins
//! the two halves together.

use crate::swap_leg::sha256;
use crate::swap_session::SecretSource;
use crate::swap_wire::SWAP_ID_LEN;

/// The distinct-byte-value floor a 32-byte seed must clear. See the module docs for the
/// `2^-111` false-reject bound that justifies the number.
const MIN_DISTINCT_BYTES: usize = 8;

/// Domain separator stretching a caller seed into the per-swap secret master.
const SECRET_MASTER_LABEL: &[u8] = b"nimmesh-swap-secret-master-v1";

/// Domain separator deriving one swap's `S` from the master.
const SECRET_LABEL: &[u8] = b"nimmesh-swap-secret-v1";

/// Refuse a seed showing a canonical entropy failure. Applied at **every live FFI door** before
/// the seed becomes a swap identity or a secret master — Rust does not trust the app's RNG, the
/// same way it does not trust a peer's message (G1).
///
/// The error is a short, non-revealing reason: it names the *class* of failure, never a byte of
/// the seed (an error string is a log string — see `ffi_secret_redaction`).
pub(crate) fn check_seed_entropy(seed: &[u8; 32]) -> Result<(), &'static str> {
    if seed.iter().all(|&b| b == 0) {
        return Err(
            "seed is all zeros — the CSPRNG almost certainly failed and its error was \
                    discarded; a zero seed makes every swap secret public-derivable",
        );
    }

    let mut seen = [false; 256];
    let mut distinct = 0usize;
    for &b in seed.iter() {
        if !seen[b as usize] {
            seen[b as usize] = true;
            distinct += 1;
        }
    }
    if distinct < MIN_DISTINCT_BYTES {
        return Err(
            "seed has too few distinct byte values to have come from a CSPRNG (a stuck \
                    RNG or a hand-typed placeholder)",
        );
    }
    Ok(())
}

/// Draw a fresh 32-byte swap seed from the OS CSPRNG. **This is the production drawer** — prefer
/// it over any caller-supplied seed, since it removes the swallowed-RNG-error failure mode by
/// construction rather than merely detecting it.
///
/// Panics if the OS RNG is unavailable. That is deliberate and matches the wallet path's
/// `precondition(rc == errSecSuccess)`: a swap that proceeds without entropy loses funds, so
/// failing loudly is the only safe outcome. `getrandom` is a direct dependency and adds no new
/// surface — `x25519-dalek` already links it unconditionally.
pub(crate) fn draw_swap_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed)
        .expect("OS CSPRNG unavailable — refusing to swap without entropy");
    debug_assert!(check_seed_entropy(&seed).is_ok());
    seed
}

/// The per-swap secret PRF over a seed: `S = sha256(master ‖ swap_id ‖ label)` where
/// `master = sha256(seed ‖ master_label)`. Unpredictable without the seed, distinct per swap, and
/// domain-separated from the seed's identity use so the same bytes can key both.
///
/// This is the single definition of the recipe both live doors previously copy-pasted.
pub(crate) fn secret_source(seed: &[u8; 32]) -> SecretSource {
    let master = {
        let mut buf = seed.to_vec();
        buf.extend_from_slice(SECRET_MASTER_LABEL);
        sha256(&buf)
    };
    Box::new(move |swap_id| {
        let mut buf = master.to_vec();
        buf.extend_from_slice(swap_id);
        buf.extend_from_slice(SECRET_LABEL);
        sha256(&buf)
    })
}

/// A SIM stand-in for the swap secret `S`. **GATED:** production MUST draw `S` from a CSPRNG — a
/// predictable secret lets an attacker pre-claim the BTC leg. This deterministic placeholder exists
/// only so the no-RNG sim/test is reproducible; it is never used on a real-fund path
/// ([`crate::swap_session::SwapSession::live_safety`] refuses a live pairing that still holds it).
///
/// Moved here from `swap_node` so the entropy inventory is one module; the name is pinned by the
/// OWNER-GATED ledger doclint (`tests/owner_gated_doclint.rs`).
pub(crate) fn sim_secret(swap_id: &[u8; SWAP_ID_LEN]) -> [u8; 32] {
    let mut buf = b"NIMMESH-SIM-INTENT-SECRET-DO-NOT-USE-IN-PROD".to_vec();
    buf.extend_from_slice(swap_id);
    sha256(&buf)
}

/// A deterministic, gate-clearing seed for the no-RNG suites. Tests must not use `[7; 32]`-style
/// placeholders: those are precisely what [`check_seed_entropy`] refuses, so a suite built on them
/// would either force the gate to be weakened or quietly route around it.
#[cfg(test)]
pub(crate) fn test_seed(tag: u8) -> [u8; 32] {
    sha256(&[b"nimmesh-test-seed-v1".as_slice(), &[tag]].concat())
}
