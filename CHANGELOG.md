# Changelog

All notable changes to nimiq.bitmesh. Each PR bumps the version and adds an entry.

## [0.4.0] — 2026-06-26

### Added — G6 (Rust core): relay-engine refinements (PROTOCOL.md "TTL / hop cap & relay")

Builds the PROTOCOL.md relay sophistication on top of the G5 basic relay (which already
did blind LRU dedup → TTL-decrement → flood). New logic is split into dedicated modules
to keep every file well under the 800-line ceiling.

- `crates/bitmesh-core/src/relay.rs` — the **G6 relay policy**:
  - **Degree-adaptive probabilistic relay.** In a sparse mesh (peer-degree below the
    high-degree threshold **6**) every flooded packet is always relayed; in a dense mesh
    each is relayed only with probability **0.5**, damping broadcast storms. The decision
    rides an **injectable, seeded `RelayRng`** (`XorShiftRng`) so tests are deterministic.
  - **Relay jitter 10–220 ms** before a rebroadcast, via an **injectable `RelayDelay`
    trait** — `RealDelay` (sleeps the worker thread) in production, `NoDelay` (zero-cost)
    in tests, so the suite never actually sleeps.
  - **`relayed_ttl`** — the loop-free TTL hop cap (`min(ttl, 7)` then decrement, drop at
    the floor); capping a hostile over-large TTL is what makes the flood provably
    loop-free.
  - A `RelayPolicy` bundles the RNG + delay + tunables; `production()` (real jitter, time
    seed) vs `deterministic()` (zero sleep, fixed seed) — the harness/tests use the latter.
- `crates/bitmesh-core/src/fragment.rs` — the **`fragment = 0x20`** split/reassemble path
  (defined-but-unused for today's ~205-B `nimiqTx`, implemented for larger/future
  payloads). Fragment header **8 B fragmentID + 2 B index + 2 B total + 1 B originalType**;
  `fragment_message` splits a payload at the BLE chunk (~469 B), and a bounded
  **`Reassembler`** (≤ **128** in-flight, oldest evicted; **30 s** lifetime via a
  caller-supplied logical clock) rebuilds it. Reassembled messages are dispatched with
  **TTL zeroed** (delivered locally, never re-flooded).
- `crates/bitmesh-core/src/engine.rs` — wires the above into the worker:
  - relays now run the **degree-adaptive decision → jitter → TTL hop cap → flood**;
  - **source-link exclusion** — a relay never echoes a packet back out the peer it
    arrived on (new `flood_excluding`; the inbound source peer is threaded from the radio
    callback through `process_inbound`);
  - the `fragment` type feeds the reassembler and dispatches the rebuilt message locally;
  - `WorkerState` now carries the `RelayPolicy` + `Reassembler` (worker-thread-local, no
    locks on the hot path). `txWire` stays **opaque** — no signing/broadcast (`// G3:` /
    `// G8:` anchors kept).
- `crates/bitmesh-core/src/node.rs` — new **`on_packet_received_from(peer, bytes)`** FFI
  method so the shim can attribute the source link (the source-unaware
  `on_packet_received` still works); the relay policy is injected at construction
  (production default; harness injects deterministic).
- `crates/bitmesh-core/src/mock_radio.rs` — the harness delivers with the source peer
  attributed and builds nodes with the **deterministic** (zero-sleep) policy.
- `crates/bitmesh-core/tests/relay_proptests.rs` — **property tests** (proptest):
  **loop-freedom** (TTL relay strictly terminates within the hop cap from any start),
  **dedup correctness** (a key is reported "fresh" at most once), **fragment round-trip**
  (any payload reassembles byte-for-byte, in any order), and **adaptive relay reaches the
  gateway** across a random connected sparse tree. Plus engine-level e2e tests for
  source-link exclusion and fragmented-receipt reassembly-then-settle.
- Local gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo test --all` (67 unit + 4 relay proptests + 5 wire proptests),
  and `scripts/size-guard.sh`. Non-money-path; opaque bytes only.

## [0.3.0] — 2026-06-26

### Added — G5 (Rust core): BLE mesh node + `BleRadio` seam (ADR-0002)

Builds the **architecture ADR-0002 fixes**: the BLE radio stays native and the Rust core
owns everything above the byte-stream seam, wired with UniFFI foreign traits as two
objects pointing at each other. Native iOS/Android shim is deferred (needs Xcode +
Andjroo's Apple ID); this is the **Rust-core part of #5**.

- `crates/bitmesh-core/src/radio.rs` — the **`BleRadio` foreign trait**
  (`#[uniffi::export(with_foreign)]`) the native shim implements: `start_advertising`,
  `start_scanning`, `send(peer_id, bytes)` (**fire-and-forget**), `disconnect(peer_id)`,
  `stop`. Rust holds `Arc<dyn BleRadio>` and only ever calls **out** to it. A "peer" is an
  opaque BLE connection identity — the radio never sees a TTL or a packet.
- `crates/bitmesh-core/src/node.rs` — **`MeshNode`** (`#[derive(uniffi::Object)]`), the
  object the shim calls **in** to on every BLE event: `on_peer_connected`,
  `on_peer_disconnected`, `on_packet_received(bytes)`, `on_send_result(peer, ok)`,
  `submit_local_tx(tx_wire)`. `on_packet_received` is **NON-BLOCKING** — it only enqueues
  to an internal channel and returns; a dedicated worker thread drains the queue and runs
  decode → dedup → TTL-relay, calling `radio.send` **off** the callback thread.
- `crates/bitmesh-core/src/engine.rs` — the real-packet **relay / gateway / origin**
  logic. Wires the **G4 codec** into the mesh: the temporary `MeshFrame` framing is gone,
  replaced by real bitmesh packets (`codec::encode`/`decode`, `MessageType::NimiqTx 0x30`
  + the TLV envelope, `nimiqTxReceipt 0x31`). Relays operate on **real packet headers**
  (TTL-decrement, blind LRU dedup on the `(type, senderID, timestamp)` header identity);
  `txWire` stays **opaque** (no signing/broadcast — `// G3:` / `// G8:` anchors kept).
- `crates/bitmesh-core/src/dedup.rs` — a bounded O(1) **LRU** "have I seen this?" set
  (not a bloom filter; capped against hostile-flood DoS, RISKS.md #4).
- `crates/bitmesh-core/src/mock_radio.rs` — **`MockRadio`** (a pure-Rust `BleRadio`) + a
  **`MockEther`** virtual topology with controllable **latency / loss / partition**, and a
  **`MeshHarness`** that wires N `MeshNode`s into a mesh. The headless `kind: mock` test
  substrate (RISKS.md Part A) — the whole demo loop runs under `cargo test`, no phone.
- `crates/bitmesh-core/src/e2e_tests.rs` — the **full headless end-to-end test**:
  `submit_local_tx(opaque_bytes)` on an offline origin → real-packet flood at TTL=7 → a
  blind relay (TTL-decrement + LRU dedup) → a mock gateway records the bytes + emits a
  `nimiqTxReceipt 0x31` → receipt propagates back → origin observes **Settled**. Plus the
  diamond-path single-submit, the rejected→Failed, latency, total-loss→Pending, and
  partition cases.
- **The four ADR-0002 callback gotchas, each engineered + tested:** (a) `on_packet_received`
  is non-blocking — a test asserts `radio.send` never re-enters synchronously on the
  callback thread (it only ever runs on the worker thread); (b) `send` is fire-and-forget,
  outcomes arrive via `on_send_result` — tested for both delivered + dropped hops; (c) the
  worker wraps each job in `catch_unwind` so a **panicking handler can't abort** the worker
  (tested with a panicking gateway) — the hot path is infallible; (d) the node↔radio
  refcount cycle is broken by a **weak edge** (node holds the radio strongly, the radio
  holds the node weakly) — a teardown/leak test proves `shutdown` releases the radio and
  the node is reclaimed with no leak.
- **Replaced** the G2 temporary `MeshFrame` framing + the `MeshTransport`/`MockMesh`
  broadcast substrate (and the `payment.rs` orchestrator) with the canonical ADR-0002
  radio model. `transport.rs` is slimmed to the shared value types (`TxId`, `mock_tx_id`,
  `MeshError`); `provider.rs` now bundles a `BleRadio` + `MeshGateway` behind
  `kind: mock | real`.
- Generated Swift + Kotlin bindings verified (`BleRadio` foreign protocol + `MeshNode`
  Rust-backed class) without Xcode / Android SDK.
- Local gate green: `cargo fmt --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo test --all` (50 unit + 5 proptests), and `scripts/size-guard.sh`.

## [0.2.0] — 2026-06-26

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
### Added — G2: provider seam + `MockMeshTransport` (mock pay-loop, no radio)

- **`transport.rs`** — the frozen `MeshTransport` seam (start/stop/`broadcast`/
  `set_receiver` + a `PacketHandler` deliver-via-callback), plus an in-memory,
  channel-based `MockMeshTransport` and a `MockMesh` graph that wires several virtual
  nodes into a relay topology — **no Bluetooth, no `tokio`, no external deps**. Carries
  **opaque `Vec<u8>`** payloads end to end. Includes a temporary `MeshFrame` mock
  framing (TTL + a `(type, txId)` dedup key around an opaque payload) with `// G4:`
  anchors marking where the real bitmesh packet codec replaces it.
- **`gateway.rs`** — the `MeshGateway` seam (`submit(txWire) -> Receipt`) + a
  record-only `MockGateway`/mock-RPC that stores submissions and emits a `Receipt`
  (`Accepted`/`Expired`/`Failed`), **no real network**. `// G8:` anchor marks where the
  real `sendRawTransaction` lands (money-path, gated).
- **`provider.rs`** — a `MeshProvider { kind: Mock | Real }` factory mirroring the
  fleet `ChainProvider kind:mock|real` pattern; `Mock` is fully wired, `Real` is a
  documented `// G5:` / `// G8:` seam stub.
- **`payment.rs`** — the `MeshPayment` orchestrator tying **origin → relay → gateway →
  receipt** (`OriginNode`/`RelayNode`/`GatewayNode`, `PaymentStatus`). Blind relays
  dedup + TTL-decrement + re-flood and **never inspect the opaque payload**.
  `// G3:` anchor marks where the real signed-tx bytes from `sign_offline()` ride the
  same `Vec<u8>` path.
- **End-to-end mock pay-loop test**: an opaque payload floods from an offline origin
  through ≥1 relay to a gateway, which records the submission and emits a receipt that
  propagates back; the origin observes `Settled`. Plus dedup-across-two-paths (one
  submission), reject→`Failed` (unconfirmed-until-inclusion honesty), and
  unreachable-gateway→`Pending` cases. 19 unit tests green locally
  (`fmt`/`clippy -D warnings`/`test --all`/size-guard). No version bump (loop tags at
  merge); non-money-path.

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
