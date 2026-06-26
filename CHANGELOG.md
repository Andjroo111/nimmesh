# Changelog

All notable changes to nimiq.bitmesh. Each PR bumps the version and adds an entry.

## [Unreleased]

### Added — G4: bitmesh wire protocol + packet codec (pure Rust)

- `crates/bitmesh-core/src/packet.rs` — the in-memory **packet model** and on-wire
  constants: the 14-byte big-endian header (`version=1`, `MessageType`, `ttl=7`,
  `timestamp`, `flags`, `payloadLength`), the five flag bits (`0x01` hasRecipient ·
  `0x02` hasSignature · `0x04` isCompressed · `0x08` hasRoute · `0x10` isRSR), and the
  `MessageType` enum (`fragment=0x20`, `requestSync=0x21`, `nimiqTx=0x30`,
  `nimiqTxReceipt=0x31`, `nimiqHeadBeacon=0x32`). `hasRecipient`/`hasSignature` are
  derived from field presence so the model can never disagree with the bytes.
- `crates/bitmesh-core/src/codec.rs` — byte-level `encode()` / `decode()` with strict,
  panic-free bounds checking, plus **PKCS#7-style block padding** up to the smallest of
  `[256, 512, 1024, 2048]`. Decode recomputes the exact packet length from the header
  and ignores trailing padding (no PKCS#7 unpad oracle); a real ~205-B `nimiqTx` pads
  cleanly into the 256 block. A typed `CodecError` rejects unknown version / type /
  flag bits and truncated frames.
- `crates/bitmesh-core/src/envelope.rs` — the **Nimiq TLV envelope** (`1B type | 1B len
  | value`): `0x01` txWire (required, **opaque** bytes), `0x02` networkId (required, 1B,
  default **testnet = 5**), `0x03` validUntil (u32 BE), `0x04` txId (32B), `0x05`
  encMemo, `0x06` wantReceipt. Unknown TLV types are skipped for forward-compat; fixed-
  width fields must carry their exact length; the two required fields must be present.
- `crates/bitmesh-core/tests/wire_proptests.rs` — `proptest` property/fuzz tests: `decode`
  and `decode_envelope` **never panic** on arbitrary/malformed input, and any valid
  packet / envelope (incl. an envelope nested inside a `nimiqTx` packet) round-trips
  byte-for-byte. Plus per-message-type round-trip, padding-block, and rejection unit
  tests.
- `txWire` is carried as **opaque bytes** end to end — no signing, no broadcast, no key
  material (that is G3/G8, money-path and Andjroo-gated). Non-money-path; auto-merge.
- Local gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo test --all` (28 tests), and `scripts/size-guard.sh`.

## [0.1.0] — 2026-06-26

### Added — G1: Rust core scaffold + UniFFI + CI

- Cargo **workspace** (`Cargo.toml`) + `crates/bitmesh-core/` — the shared, headless
  Rust core crate, built with `crate-type = ["cdylib", "staticlib", "lib"]` so it can
  back an Android `.so`, an iOS `.xcframework`, and the local Rust unit tests.
- UniFFI proc-macro surface (`uniffi::setup_scaffolding!()`) exposing a small but
  **real, unit-tested** API that proves the FFI boundary: `core_version()`,
  `default_network()`, a `NetworkId` enum (Testnet/Mainnet) with exported
  `network_wire_id()` / `network_is_loop_safe()` helpers, and a `echo_bytes()` binary
  round-trip. `G3:`/`G4:`/`G8:` TODO anchors mark where signing, the packet codec, and
  gateway broadcast land — none implemented (no money path in G1).
- `uniffi-bindgen` binary (`src/bin/uniffi-bindgen.rs`) so Swift + Kotlin bindings
  generate **without** Xcode or the Android SDK/NDK. Confirmed both languages emit
  cleanly into the git-ignored `bindings/generated/`.
- `scripts/size-guard.sh` — fails if any tracked `*.rs|*.swift|*.kt` file exceeds 800 lines.
- `.github/workflows/ci.yml` — a `core` job on `ubuntu-latest` mirroring the local
  gate (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --all`, size-guard).
- `rust-toolchain.toml` pinning stable (+ clippy/rustfmt).
- Local gate green on the Mini: `cargo fmt --check`, `cargo clippy --all-targets
  --all-features -- -D warnings`, and `cargo test --all` (5 unit tests) all pass.

## [0.0.1] — 2026-06-26

### Added
- Project bootstrap: `docs/GOAL.md` (north star, demo loop, core values),
  `docs/LOOP.md` (autonomous build contract, goals G1–G13, money-path gating),
  `docs/adr/0001` (native Swift + Kotlin + shared Rust core via UniFFI),
  `docs/PROTOCOL.md` (bitmesh wire format), `docs/RISKS.md` (offline-payment hazards),
  `nimiq-stack.json` (fleet manifest, marked exempt — native, not a web PWA), and the
  CI plan in `docs/ci/`.
- Outcome of the `bitmesh-design-spike` dynamic workflow (4 research agents + synthesis):
  empirically confirmed 139-byte signed transfer, RFC-8032 Ed25519 signing, Bitchat =
  Unlicense (portable), ~2 h validity window.
