# ADR-0012 — Secrets at the FFI door render redacted, from one auditable module

- **Status:** accepted (G11 / #82, 2026-07-15)
- **Context:** G11's done-condition (`docs/swap/INTEGRATION-AGENDA.md`) is *"assert no secret/seed
  material crosses FFI or hits logs/mesh."* The *crosses-FFI* half is mostly held by the
  `EnclaveKey` / `BtcEnclaveKey` foreign traits — only a pubkey and signed bytes come back. The
  *hits-logs* half was **unenforced and false**: four `uniffi::Record` config types take raw key
  material inbound and every one of them **derived `Debug`**.

  | Record | Secret field | What it controls |
  |---|---|---|
  | `FfiLiveInitiatorConfig` | `intent_seed` | the master for **every** per-swap secret `S` this node draws |
  | | `evm_gas_secret` | a spendable Amoy account |
  | `FfiLiveResponderConfig` | `nim_claim_seed` | the Ed25519 key that **redeems the NIM HTLC** |
  | | `evm_funding_secret` | the **funded** Amoy account escrowing the USDC |
  | `FfiParticipantConfig` | `intent_seed` | ephemeral identity + secret master |
  | `FfiUsdcSendConfig` | `source_secret` | the account holding the USDC |

  These are `Record`s, so the app may `print` / `dbg!` / `os_log` one, and a UniFFI panic can carry
  one into a crash report. A derived `Debug` renders the raw bytes at every one of those. Logs are
  not a trust boundary — they hit disk, crash reporters, and pasted issue threads. Leaking
  `intent_seed` is the sharpest: an attacker who learns the initiator's secret master can derive
  `S` for every swap it will ever run and pre-claim the counterparty leg — the same theft S1
  closed on-chain, reopened through stdout.

## 1. The decision

Secret-bearing FFI records **do not derive `Debug`**. Each gets a hand-written impl in a single
module — `crates/nimmesh-core/src/ffi_secret_redaction.rs` — where a key-material field renders as
`<redacted N bytes>` via the `Redacted` newtype and every public field renders normally.

Three things follow from putting them together rather than beside each struct:

1. **One auditable inventory.** "What is secret at the door" is a list in one file, not a property
   you re-derive by reading four modules. A reviewer checks one place.
2. **One regression suite.** `ffi_secret_redaction_tests.rs` asserts the rendering never contains
   `format!("{:?}", the_secret_vec)` — the literal `[82, 82, …]` a derive emits. Re-deriving
   `Debug` on any of these fails the test loudly. The guard is executable, not a comment.
3. **The derive sites carry a pointer**, so the next person to reach for `#[derive(Debug)]` sees
   why it isn't there.

**Length is rendered; the value never is** — not a prefix, not a hash. A length is public, and a
wrong-length seed is the field's most common real bug. A hash prefix would be a (weak, offline-
grindable) leak for no diagnostic gain. This differs from `noise.rs`'s `StaticIdentity`, which
prints a 4-byte *fingerprint* — that is a hash of a **public** key, so it leaks nothing; these
fields are the private material itself.

**Public fields keep rendering.** Redaction that blinds the operator to a bad RPC url or a wrong
amount gets reverted the first time someone debugs a config, and then the secrets come back with
it. A test pins the public fields too.

## 2. What this does NOT do

- **It does not stop the seed from crossing FFI.** `FfiLiveResponderConfig` still takes a raw
  Ed25519 seed and a funded secp256k1 key inbound — G11's clause 2 in full would move the
  responder's NIM claim key behind `EnclaveKey` (as the initiator's funding key already is) and
  wire `BtcEnclaveKey` on the live path. Redaction narrows the *egress*; the *ingress* is a
  separate slice.
- **It does not zeroize.** The secrets still sit in `Vec<u8>` heap memory for the config's life.
  `zeroize` is present transitively (via the dalek/k256 stack) but is not a direct dep; adopting
  `Zeroize` + `ZeroizeOnDrop` on these records is worth its own slice.
- **It does not cover the broadcast path**, and deliberately: `FfiSwapEffect::Broadcast { tx }`
  carries a claim tx whose witness contains `S`, but that tx is published to a public chain by
  design — the reveal *is* the protocol. Redacting it would hide public data.
- **It does not validate entropy.** The CSPRNG draw is still the caller's job across FFI; the core
  never checks the seed isn't all-zeros. That is G11 clause 1's remainder.

## 3. Alternatives rejected

- **A `Secret(Vec<u8>)` newtype field.** Strongest (redaction by construction, and it could carry
  `Zeroize`), but it changes the `uniffi::Record` field type, so every Swift/Kotlin call site and
  the generated bindings churn. The leak is worth closing now, at zero ABI cost; the newtype stays
  open as a follow-up once the ingress slice moves these fields anyway.
- **Dropping `Debug` entirely** (the `InMemoryEnclaveKey` pattern). Free and airtight, but
  `uniffi::Record`s are ergonomically expected to be printable, and a `Debug`-less config makes
  `assert_eq!` diagnostics in the existing suites useless. Redaction keeps the diagnostics and
  kills the leak.
- **A clippy lint / grep in CI.** No lint expresses "this field is secret." The executable test is
  the enforcement.
