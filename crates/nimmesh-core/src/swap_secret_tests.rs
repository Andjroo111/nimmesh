//! G11 (#82): the swap-entropy gate — [`crate::swap_secret`].
//!
//! The load-bearing assertion here is [`a_zero_seed_is_refused`]: an all-zero seed is what a
//! swallowed CSPRNG error actually produces (a zero-filled buffer the RNG never wrote), and it
//! makes every per-swap `S` public-derivable from the on-wire `swap_id` — the S1 theft, reopened
//! through the app's RNG. The door tests live beside their doors (`swap_participant_ffi`,
//! `swap_live_ffi_tests`); this suite pins the primitive.

use super::swap_secret::{
    check_seed_entropy, draw_swap_seed, secret_source, sim_secret, test_seed,
};

/// The canonical failure: a buffer the CSPRNG never wrote. `Data(count: 32)` in Swift and
/// `[0u8; 32]` in Rust are both zero-filled, so a discarded/ignored RNG status hands the swap a
/// seed of zeros — from which anyone recomputes `S`.
#[test]
fn a_zero_seed_is_refused() {
    assert!(check_seed_entropy(&[0u8; 32]).is_err());
}

/// A stuck byte (hardware RNG latched, or a hand-typed placeholder like `[7; 32]`) is the other
/// canonical failure the repetition test catches.
#[test]
fn a_stuck_byte_seed_is_refused() {
    for b in [0x07u8, 0x5A, 0xFF] {
        assert!(
            check_seed_entropy(&[b; 32]).is_err(),
            "a seed of all-{b:#04x} must be refused"
        );
    }
}

/// A seed drawn from too small an alphabet is refused even when no single byte is stuck: the floor
/// is on *distinct values*, not on the top byte differing.
#[test]
fn a_low_alphabet_seed_is_refused() {
    let mut seed = [0u8; 32];
    for (i, b) in seed.iter_mut().enumerate() {
        *b = (i % 4) as u8; // 4 distinct values over 32 bytes
    }
    assert!(check_seed_entropy(&seed).is_err());
}

/// The gate must not reject real entropy. `draw_swap_seed` is the production drawer — every seed it
/// produces has to clear the very gate the doors apply, or the two halves disagree and the app is
/// locked out. 256 draws would catch a threshold set anywhere near the distribution's mass.
#[test]
fn every_drawn_seed_clears_the_gate() {
    for i in 0..256 {
        let seed = draw_swap_seed();
        assert!(
            check_seed_entropy(&seed).is_ok(),
            "draw #{i} produced a seed its own gate refuses"
        );
    }
}

/// Two draws must differ — a drawer that returns a constant would pass every other test here.
#[test]
fn two_drawn_seeds_differ() {
    assert_ne!(draw_swap_seed(), draw_swap_seed());
}

/// The deterministic test seeds the suite uses in place of `[7; 32]` must clear the gate (so the
/// no-RNG suites stay reproducible without smuggling degenerate entropy past the doors).
#[test]
fn deterministic_test_seeds_clear_the_gate() {
    for tag in 0..64u8 {
        assert!(
            check_seed_entropy(&test_seed(tag)).is_ok(),
            "test_seed({tag}) is degenerate"
        );
    }
    assert_ne!(test_seed(1), test_seed(2));
}

/// `sim_secret` is unchanged by the move out of `swap_node` — it stays the public-derivable,
/// never-for-real-funds stand-in, and `live_safety`'s C1 gate still refuses it by identity.
#[test]
fn sim_secret_is_still_the_deterministic_stand_in() {
    let id = [9u8; crate::swap_wire::SWAP_ID_LEN];
    assert_eq!(sim_secret(&id), sim_secret(&id), "sim_secret must be pure");
    assert_ne!(
        sim_secret(&id),
        sim_secret(&[8u8; crate::swap_wire::SWAP_ID_LEN])
    );
}

/// The consolidated PRF: distinct per swap, and unpredictable without the seed. This pins the
/// recipe both live doors previously copy-pasted, so a drift in one can no longer go unnoticed.
#[test]
fn the_secret_prf_is_per_swap_and_seed_bound() {
    let a = secret_source(&test_seed(1));
    let b = secret_source(&test_seed(2));
    let id1 = [1u8; crate::swap_wire::SWAP_ID_LEN];
    let id2 = [2u8; crate::swap_wire::SWAP_ID_LEN];

    assert_ne!(
        a(&id1),
        a(&id2),
        "the same node must not reuse S across swaps"
    );
    assert_ne!(
        a(&id1),
        b(&id1),
        "S must be bound to the drawing node's seed"
    );
    assert_eq!(
        a(&id1),
        a(&id1),
        "the PRF must be deterministic for one swap"
    );
}

/// The whole point, stated as a test: with a zero seed the PRF's output is computable by anyone who
/// saw the `swap_id` on the wire. This is what the door gate exists to make unreachable — it
/// documents *why* `check_seed_entropy` is on the money path rather than in a lint.
#[test]
fn a_zero_seed_makes_s_public_derivable() {
    let victim = secret_source(&[0u8; 32]);
    let attacker = secret_source(&[0u8; 32]); // an observer who guesses the seed is zeros
    let swap_id = [3u8; crate::swap_wire::SWAP_ID_LEN]; // read off the wire

    assert_eq!(
        victim(&swap_id),
        attacker(&swap_id),
        "S is recomputable from a public swap_id — the S1 theft"
    );
    assert!(
        check_seed_entropy(&[0u8; 32]).is_err(),
        "so the seed that allows it must never reach a live door"
    );
}
