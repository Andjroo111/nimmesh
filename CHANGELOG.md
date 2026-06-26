# Changelog

All notable changes to nimiq.bitmesh. Each PR bumps the version and adds an entry.

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
