# nimmesh swap — NIM⇄USDC (Polygon) leg (autonomous build loop)

The standing agenda for adding a **USDC-on-Polygon** counterparty leg to the mesh swap, alongside the
existing NIM⇄BTC. The swap *engine* is already chain-agnostic (`swap_leg::SwapLeg` — `fund`/`claim`/
`refund` on a SHA-256 hashlock), so "switch coin" = add a new leg behind its own feature, exactly like
the BTC leg (`bitcoin-leg` / `rust-bitcoin`). Branch: `feat/usdc-polygon` (off `feat/mesh-swap`; never
`main`). Same loop contract as the discovery loop.

## Why USDC-on-Polygon fits (feasibility)

- **Cross-chain hashlock works.** A Solidity hashed-timelock contract can hash with the **SHA-256
  precompile** (address `0x02`), so the Polygon HTLC uses the SAME `H = SHA-256(S)` as the NIM and BTC
  legs — one secret unlocks both sides. (Most reference EVM HTLCs use `keccak256`; we use `sha256` for
  cross-chain compatibility — a one-line contract choice.)
- **No VM on Nimiq, EVM on Polygon.** Nimiq has no general VM (the NIM leg is a native HTLC tx); Polygon
  is full EVM (the USDC leg is contract calls). Both are just `SwapLeg` implementations to the engine.
- **USDC is an ERC-20** (6 decimals on Polygon). The HTLC contract pulls USDC via `transferFrom` after
  an `approve`; claim/refund move it to the recipient/funder. Amounts are `u64` micro-USDC.
- **Timelock ladder.** Polygon ~2 s blocks; the existing Δ_safe ladder (`swap.rs`) maps to EVM block
  heights / timestamps the same way it maps to NIM/BTC heights.
- **Gas abstraction (later).** A claimer needs MATIC for gas; a relayer / meta-tx (EIP-2771) or a
  paymaster can sponsor it — noted for a gated goal, not v1.

## The contract (per iteration)

Read this + the off-repo log (`~/nimmesh-polygon-loop/LOG.md`), build the next unchecked goal on
`feat/usdc-polygon`, gate it, commit on green, push, check it off, log one line, schedule the next.
**Money-path stays gated**: sim/testnet only — no mainnet, no real funds, no live broadcast, no real
tx signing. The EVM leg lives behind a `polygon-leg` cargo feature (OFF by default, so the core stays
lean + WASM-friendly), mirroring `bitcoin-leg`.

**Gate** = `cargo test -p nimmesh-core` (default) + `--features bitcoin-leg` + `--features polygon-leg`,
`cargo clippy` (no warnings) for each feature set, `cargo fmt --check`, `bash scripts/size-guard.sh`
(≤800 lines/.rs/.swift/.kt), `nq lint` for any UI. Validate EVM bytes against known vectors (the BTC
leg was validated vs `bitcoinjs-lib`; the EVM leg vs known keccak/ABI vectors + later ethers.js).

## Goal ladder

- [x] **P1 — EVM primitives.** A `polygon-leg`-gated `evm` module: `keccak256` (RustCrypto `sha3`),
      EVM address derivation (`keccak256(uncompressed_pubkey[1..])[12..]`), and the ABI 4-byte function
      selector (`keccak256(sig)[..4]`). Validate vs known vectors (privkey-1 address
      `0x7e5f…395Bdf`, `transfer(address,uint256)` = `0xa9059cbb`). Pure-Rust, no RPC/funds.
      Done: `evm.rs` (`keccak256` / `evm_address` / `function_selector`) behind a new `polygon-leg`
      feature (`sha3` optional dep, OFF by default → core stays lean, verified no `sha3` in the default
      `cargo tree`). 3 tests vs known vectors: Keccak-256("") (distinct from SHA3-256, proving the EVM
      variant), privkey-1 → `0x7e5f…395Bdf`, and `transfer`/`balanceOf`/`transferFrom` selectors. Gate
      now runs default + `bitcoin-leg` + `polygon-leg`; all green (12 binaries each), clippy clean ×3.
- [x] **P2 — USDC HTLC model + cross-chain hashlock.** The Solidity HTLC interface (newSwap/withdraw/
      refund with a SHA-256 hashlock), the on-chain swap_id derivation, and a `SwapLeg`-implementing
      USDC leg in SIM proving NIM⇄USDC atomicity end to end via the existing engine (no EVM dep).
      Done: `swap_usdc_leg::UsdcLeg` — a faithful in-memory model of the Polygon HTLC contract
      implementing the chain-agnostic `swap_leg::SwapLeg` trait, so it plugs into the engine exactly
      like `MockLeg`. **SHA-256-precompile choice:** the contract verifies `withdraw`'s secret with the
      `0x02` precompile (`sha256`), NOT keccak256, so its hashlock `H = SHA-256(S)` is byte-identical to
      the NIM/BTC legs and one secret unlocks all three — proven by the
      `hashlock_is_sha256_not_keccak_so_the_lock_is_cross_chain` test (a keccak-locked HTLC rejects the
      same secret). **swap_id derivation:** `usdc_swap_id` = `keccak256(abi.encodePacked(sender,
      receiver, amount, hashlock, timelock))` (20+20+32+32+32 = 136 bytes, big-endian `uint256` words),
      derived on-chain so it's deterministic + bound to every param (no slot collision/replay) —
      `swap_id_is_deterministic_and_bound_to_every_param`. USDC amounts in 6-decimal micro-USDC
      (`MICRO_USDC`), recipient/sender are EVM addresses. `swap_usdc_e2e_tests` mirrors the BTC/MockLeg
      e2e with `UsdcLeg` as the counterparty leg: happy path settles both, every adversarial path
      unwinds to a clean two-sided refund, no one-sided settlement. 12 new tests (6 unit + 6 e2e);
      gate green default + bitcoin-leg + polygon-leg, clippy clean ×3, `sha3` still absent from the
      default tree. Full design note lives in the `swap_usdc_leg` module docs.
- [x] **P3 — ABI calldata builders.** Hand-built ABI calldata for the HTLC lock/claim/refund + ERC-20
      `approve`/`transferFrom` (selector + 32-byte-padded args), validated vs known encodings.
      Done: `evm_abi` (no ethers/web3 dep) — the standard ABI **head** encoder (`word_u256` big-endian
      32-byte word, `word_address` left-padded; `bytes32` passthrough — distinct from the
      `abi.encodePacked` layout `usdc_swap_id` uses) + six builders: `erc20_approve`/
      `erc20_transfer_from` and `htlc_new_swap`/`htlc_withdraw`/`htlc_refund` (each = selector ++
      32-byte words). 8 tests: the ERC-20 selectors pinned to their public constants (`approve` =
      `095ea7b3`, `transferFrom` = `23b872dd`), `approve(UniV2-router, 1)` matched byte-for-byte vs a
      hardcoded known calldata, and every builder's full word layout asserted in order (length +
      per-arg offsets; `bytes32` verbatim, not re-padded). Byte-builder only — no signing/broadcast
      (P4, gated). Gate green default + bitcoin-leg + polygon-leg, clippy clean ×3, `evm.rs` untouched
      at 87 lines, `sha3` still absent from the default tree.
- [x] **P4a — EVM RLP + EIP-155 unsigned signing-hash (KEY-FREE).** The deterministic, key-free half
      of P4. Done: `evm_rlp` — a minimal RLP encoder (`rlp_bytes`/`rlp_list`/`rlp_u64`, minimal
      big-endian ints, short + length-of-length forms) validated vs canonical RLP vectors ("dog",
      empty, single bytes, `["cat","dog"]`, 1024, a 56-byte long string); a `LegacyTx` EIP-155 legacy
      assembler producing `rlp([nonce,gasPrice,gasLimit,to,value,data,chainId,0,0])` + the
      `signing_hash` = keccak256(that). **Validated vs the canonical EIP-155 spec vector**: the
      signing data RLP matches `ec0985…8080` byte-for-byte, and the signing hash is its keccak256 (and
      keccak is itself pinned to external vectors — empty `c5d2…` + "abc" `4e03657…`). `LegacyTx::
      polygon_amoy` hard-codes Amoy testnet chainId 80002; mainnet 137 is never emitted. Wires P3
      `evm_abi` calldata as `data` (proven with a `refund(swapId)` tx → long-list form). KEY-FREE — no
      secp256k1, no key, no RPC, no broadcast. Gate green ×3, clippy clean ×3, `evm.rs` still 87 lines.
- [x] **P4b — EIP-155 signed-tx RLP assembly + the `EvmSigner` seam (KEY-FREE).** Done: `evm_rlp`
      extended with `rlp_int` (RLP integer = leading-zero-stripped big-endian, for `r`/`s`), `eip155_v`
      (`recovery_id + chainId*2 + 35`), the `EvmSigner` SEAM trait (`sign_hash(hash) -> (r,s,recovery)`,
      analogous to `btc::BtcEnclaveKey` — NO real signer here), and `LegacyTx::signed_tx_rlp` /
      `sign_with` building `rlp([nonce,gasPrice,gasLimit,to,value,data,v,r,s])`. **Validated vs the
      published EIP-155 spec SIGNED vector**: feeding the spec's `(r=0x28ef61…6276, s=0x67cbe9…6d83,
      recovery=0)` for the canonical tx (privkey 0x4646…46, chainId 1) into `signed_tx_rlp` yields the
      exact raw tx `f86c098504a817c800…a3b6d83` byte-for-byte with `v=37` (0x25); the same result is
      reproduced through the `EvmSigner` seam (a test-only `VectorSigner`). Also `eip155_v` (0/1 ×
      chainId 1 + Amoy 80002) + `rlp_int` strip cases. KEY-FREE — no secp256k1, no key, no RPC, no
      broadcast. Gate green ×3, clippy clean ×3, `evm_rlp.rs` 361 lines, `sha3` absent from default tree.
- [x] **P4c — real secp256k1 `EvmSigner`.** Done: `evm_signer::LocalEvmKey` implements the P4b
      `EvmSigner` trait with **RFC-6979 deterministic** ECDSA + **EIP-2 low-s** via `k256` (RustCrypto,
      PURE RUST → WASM-friendly, OFF by default — verified `k256` is absent from the default `cargo
      tree`, present only under `polygon-leg`). The 32-byte secret lives behind the seam (EVM mirror of
      `btc::InMemoryBtcEnclaveKey`; secret-bearing ctor not FFI-exported). **Validated end to end vs the
      published EIP-155 spec vector**: signing the canonical tx hash with the PUBLIC test key 0x4646…46
      yields exactly r=0x28ef61…6276, s=0x67cbe9…6d83, recovery=0; `LegacyTx::sign_with(&key)`
      reproduces the published raw tx f86c09…a3b6d83 byte-for-byte; and `key.address()` =
      `0x9d8a62f656a8d1615c1294fd71e9cfb3e4855a4f` (known address for that key). RFC-6979 → deterministic
      (not flaky); zero-secret rejected. Testnet/Amoy only — **real funded key + mainnet + broadcast =
      needs:owner** (no RPC/broadcast here). Gate green ×3, clippy clean ×3, `evm_signer.rs` 166 lines.

**P4 (EVM tx + signing) is COMPLETE** — P4a (RLP + EIP-155 signing-hash), P4b (signed-tx assembly +
`EvmSigner` seam), P4c (real `k256` signer), all validated vs published EIP-155 vectors. The signing
stack can produce a byte-exact signed Polygon tx; what stays owner-gated is a real funded key, mainnet,
and the actual broadcast (an RPC client).
- [ ] **P5 — Discovery for USDC.** Extend `SwapIntent`'s `Asset` to include `Usdc` so NIM⇄USDC pairs
      discover over the mesh exactly like NIM⇄BTC; tests.

(Re-scan + append 2–4 goals when the ladder is exhausted: Polygon RPC client (gated), gas abstraction,
a USDC demo, mainnet — all owner-gated where they touch real funds.)
