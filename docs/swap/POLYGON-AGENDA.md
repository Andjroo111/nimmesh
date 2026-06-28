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
- [ ] **P2 — USDC HTLC model + cross-chain hashlock.** The Solidity HTLC interface (newSwap/withdraw/
      refund with a SHA-256 hashlock), the on-chain swap_id derivation, and a `SwapLeg`-implementing
      USDC leg in SIM proving NIM⇄USDC atomicity end to end via the existing engine (no EVM dep).
- [ ] **P3 — ABI calldata builders.** Hand-built ABI calldata for the HTLC lock/claim/refund + ERC-20
      `approve`/`transferFrom` (selector + 32-byte-padded args), validated vs known encodings.
- [ ] **P4 — EVM tx + signing (GATED).** EIP-155 legacy/1559 tx RLP + secp256k1 signing behind a key
      seam (like `btc::BtcEnclaveKey`); testnet (Amoy) only, mainnet gated, no broadcast.
- [ ] **P5 — Discovery for USDC.** Extend `SwapIntent`'s `Asset` to include `Usdc` so NIM⇄USDC pairs
      discover over the mesh exactly like NIM⇄BTC; tests.

(Re-scan + append 2–4 goals when the ladder is exhausted: Polygon RPC client (gated), gas abstraction,
a USDC demo, mainnet — all owner-gated where they touch real funds.)
