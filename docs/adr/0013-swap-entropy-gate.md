# ADR-0013 — Rust draws and health-checks swap entropy; a degenerate seed dies at the door

- **Status:** accepted
- **Date:** 2026-07-15
- **Goal:** G11 (#82) — real secret + real signer on device (OG-2)
- **Supersedes/relates:** [ADR-0012](0012-ffi-secret-redaction.md) (the *other* half of "a secret
  must not leak at the FFI door" — that one stopped secrets going *out* through `Debug`; this one
  stops non-secrets coming *in*).

## Context

A live node's `intent_seed` (and the responder's `nim_claim_seed`) arrives over FFI from the app.
It is doubly load-bearing:

1. it is the **PRF master** every per-swap secret `S` is derived from
   (`S = sha256(sha256(seed ‖ master_label) ‖ swap_id ‖ label)`), and
2. it is the **Ed25519 swap identity** (`InMemoryEnclaveKey::from_secret(&seed)`).

Until now, every door validated it for **length only**. Three separate mechanisms that look like
they would catch a bad seed do not:

- **Type/parse validation** does not: *every* 32-byte value is a valid Ed25519 seed. (The secp256k1
  secrets — `evm_gas_secret`, `evm_funding_secret` — were incidentally protected, since zero is not
  a valid scalar. That accident is narrow: it does not reject a stuck-byte key.)
- **`live_safety`'s C1 gate** does not: it refuses a session whose secret source is still the sim
  default, but a zero-seeded PRF *is* a replacement, so `secret_is_sim` flips false and C1 passes.
  **C1 asserts that you replaced the sim source, not that the replacement carries entropy.**
- **The app's own draw** did not: `SwapMesh.swift` called
  `_ = seed.withUnsafeMutableBytes { SecRandomCopyBytes(...) }` — discarding the `OSStatus` over a
  zero-filled `Data(count: 32)`. A failing RNG left the seed all zeros, silently. The wallet path
  (`Mnemonic.swift`) `precondition`s the identical call; only the swap money path dropped it.

The consequence is not a hygiene nit. With a zero seed the PRF master is *public*, and `swap_id`
travels the wire in cleartext — so any relay that observes a Propose recomputes `S` and claims the
counterparty leg before the honest party does. That is **S1**, the CRITICAL theft this whole agenda
was built to close, reopened through the app's random number generator.

This is the recurring shape of every finding this loop has closed: **an invariant that is asserted
somewhere but enforced nowhere on the path that matters.**

## Decision

**Rust owns swap entropy.** Three parts, in order of how much they actually buy:

1. **`draw_swap_seed()` — Rust draws it (the fix).** Exported over UniFFI as `drawSwapSeed()`; the
   Swift money path calls it instead of rolling its own. This *removes* the swallowed-error failure
   mode by construction. A failing OS RNG panics rather than returning zeros — deliberately matching
   the wallet path's `precondition(rc == errSecSuccess)`, because a swap that proceeds without
   entropy loses funds, so failing loudly is the only safe outcome.
2. **`check_seed_entropy()` at every live door (the backstop).** Rust does not trust the app's RNG,
   for the same reason G1 taught it not to trust a peer's message: the door is the last place that
   can still refuse. In `swap_live_ffi_live_impl` the gate lives inside `seed32`, the one chokepoint
   all four secrets funnel through, so a *future* secret field inherits the gate without anyone
   remembering to add it.
3. **One module, `swap_secret`** — the drawer, the gate, the PRF, and `sim_secret` together, so the
   entropy inventory is auditable in one place. (It also consolidated the PRF recipe, which the two
   live doors carried as copy-pasted twins, and moving `sim_secret` out brought `swap_node` back
   under the 800-line ceiling it was sitting exactly on.)

### What the gate is, and what it is not

`check_seed_entropy` is a **health test in the NIST SP 800-90B sense** (a repetition / stuck-output
check). **It is not an entropy estimator, and cannot be** — no function of 32 bytes can distinguish
a CSPRNG draw from a well-formed but weak PRNG's output. Being explicit about this is the point:
the gate catches the *canonical catastrophic* failures — an unwritten buffer, a latched hardware
RNG, a hand-typed placeholder — and nothing subtler. It is the seatbelt, not the brakes. Part 1 is
the brakes.

The rule is a floor on **distinct byte values** (≥ 8), plus an explicit all-zero case kept separate
purely for its diagnostic value (it names the swallowed-RNG-error cause in the error text).

**Why 8 is safe.** A real 32-byte CSPRNG draw has ≈ 30.2 distinct values expected. The union bound
on all 32 bytes landing inside *any* 8-value alphabet is `C(256,8) · (8/256)^32 ≈ 2^48.7 · 2^-160 ≈
2^-111` — a false-reject rate unreachable in practice, while every degenerate seed above is caught.
`every_drawn_seed_clears_the_gate` pins the drawer and the gate together, so the two halves cannot
drift into disagreeing and locking the app out.

## Consequences

- **A zero/stuck seed can no longer reach a live swap identity or secret master.** The door tests
  (`the_participant_door_refuses_a_seed_with_no_entropy`,
  `the_live_doors_refuse_secrets_with_no_entropy`) were verified red before the gate existed.
- **Error text names the failure class, never a byte of the seed** — an error string is a log
  string, which is ADR-0012's whole subject.
- **Test suites had to stop using `[7; 32]` / `[0x5A; 32]`**, which the gate rightly refuses. They
  now use `swap_secret::test_seed(tag)`: deterministic (the no-RNG suites stay reproducible) and
  gate-clearing. This is a feature — a suite built on degenerate seeds would have forced the gate to
  be weakened or routed around.
- **`getrandom` becomes a direct, non-optional dependency.** No new surface: `x25519-dalek` already
  linked it unconditionally. It was optional only because, historically, just the gateway *examples*
  seeded a keypair; entropy is now on the core money path.
- **Panicking on RNG failure is a deliberate liveness-for-safety trade.** On a device whose OS
  CSPRNG is unavailable, the swap aborts. That is correct: the alternative is swapping with a known
  key.

## What this does *not* close (G11 remains open)

- **The seed still crosses FFI inbound.** `FfiLiveResponderConfig` still passes a raw Ed25519 seed
  and a funded secp256k1 key straight across, which contradicts the goal's "seed never crosses FFI".
  The gate makes the crossing *safer*, not *absent*. The initiator's funding key is already behind
  `EnclaveKey`; the responder's claim key should follow.
- `sim_secret` is still `SwapSession::new`'s default (C1 refuses it on a live pairing, so it is
  gated, not eliminated).
- `BtcEnclaveKey` is still unwired on the live path.
- No `zeroize`: secrets still sit in `Vec<u8>` for the config's lifetime.
