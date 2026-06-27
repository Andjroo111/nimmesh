# Bitcoin leg — the cross-chain counterparty (signet) — B0 spike + plan

> The NIM leg is live-proven (`docs/swap/F0-HTLC-FINDINGS.md`). This is the BTC side of a real
> NIM⇄BTC atomic swap, on **Bitcoin signet** (Andjroo's pick — stable block times + reliable
> faucet). Testnet/play-money; **mainnet stays gated**. Network = `rust-bitcoin` `Network::Signet`
> (signet reuses testnet `tb1…` address params). API = `mempool.space/signet/api`. No node needed —
> same model as the Nimiq leg (Rust signer + public API + faucet + a JS lib to cross-check).

## We are NOT starting from scratch

Hashmark (Andjroo's prod project) already runs a **cross-chain HTLC swap engine** (BTC + Polygon-EVM
⇄ NIM). Its `app/src/chains/bitcoin/` is the **proven reference** to port:

| Hashmark file | What it gives us |
| --- | --- |
| `chains/bitcoin/script.ts` | the exact HTLC redeem script + P2WSH (locked-in spec) |
| `chains/bitcoin/htlc-pure.ts` | witness assembly (claim/refund), preimage verify/extract, BIP143 |
| `chains/bitcoin/htlc-presign.ts` | presigned claim/refund tx construction |
| `chains/bitcoin/network.ts` / `mempool.ts` | signet params + the mempool.space API client |
| `shared/htlc-window.ts` | the timelock-ladder safety logic (CLTV→ms, refund anchor + grace) |

We **port** this to a Rust `BitcoinLeg` (it must run on-device in the mesh core), **byte-validated
against `bitcoinjs-lib` 6.1.7** (already installed) — exactly how `@nimiq/core` validated the NIM leg.

## The HTLC redeem script (proven, matches our NIM leg)

```
OP_IF
  OP_SHA256 <hashRoot:32> OP_EQUALVERIFY <recipientPubkey:33> OP_CHECKSIG   ← claim w/ preimage
OP_ELSE
  <cltvLocktime> OP_CHECKLOCKTIMEVERIFY OP_DROP <senderPubkey:33> OP_CHECKSIG ← refund after timeout
OP_ENDIF
```

- **`OP_SHA256` single-pass** — the *same* 32-byte `hashRoot = SHA-256(preimage)` our Nimiq HTLC
  uses (`nimiq::htlc` `HashAlgorithm::Sha256`). **One `H` and one preimage open both legs.** ✓
- Pubkeys are **33-byte compressed secp256k1** (BTC), distinct from the NIM Ed25519 keys.
- Funding target = **P2WSH**: `scriptPubKey = OP_0 SHA256(redeemScript)` → a `tb1…` address.

## The spending txs

- **Claim** (recipient, before timeout): witness = `[recipientSig, preimage, 0x01, redeemScript]`
  (the `OP_IF` true branch). `nLockTime = 0`, sequence final. The **preimage is witness item [1]** →
  spending it on-chain reveals `S`; `extractPreimageFromWitness` is how the counterparty learns it.
- **Refund** (sender, after timeout): witness = `[senderSig, 0x00, redeemScript]` (the `OP_ELSE`
  branch). **`nLockTime = cltvLocktime`** and **sequence `< 0xffffffff`** (CLTV needs a non-final
  input sequence). Signature is **SIGHASH_ALL** over the **BIP143 (segwit v0)** sighash.

Key property (same as NIM): a valid `recipientSig` can be produced **before** the preimage is known
(SIGHASH_ALL commits to inputs+outputs, not other witness items) → the claim tx can be pre-built and
the observed preimage slotted in at [1].

## Cross-chain timelock ladder (the `Δ_safe` gate, now concrete)

Both legs use a **time-based** timeout (the NIM-leg fix carries over):
- **NIM** timeout = Unix-**ms** timestamp (`block_state.time`).
- **BTC** `cltvLocktime` = Unix-**seconds** timestamp (CLTV treats values `≥ 500_000_000` as Unix
  time). Convert: `cltv_secs = nim_timeout_ms / 1000`.
- The ladder `T_A(initiator) > T_B(responder) + Δ_safe` is evaluated in a common time base.
  `shared/htlc-window.ts` is the proven reference for the anchor + grace sizing.

## Goal ladder (B0–B4) — autonomous on signet, mainnet gated

| # | Goal | gate | status |
| --- | --- | --- | --- |
| **B0** | Spike — read hashmark's BTC HTLC, confirm the spec + signet plan (this doc) | no | ✅ done |
| **B1** | `BitcoinLeg` (Rust, `bitcoin-leg` feature): HTLC redeem script + P2WSH address + claim/refund txs (secp256k1), **byte-validated vs `bitcoinjs-lib`** | no (signet) | next |
| **B2** | BTC **gateway** — broadcast + confirmation via `mempool.space/signet/api` (no node) | no | todo |
| **B3** | **LIVE signet HTLC proof** — faucet → fund P2WSH → claim-with-preimage → refund (mirrors the NIM live test) | no | todo |
| **B4** | **Full cross-chain swap, LIVE** — one `H`: NIM HTLC + BTC HTLC, claim-reveals-`S`-on-one, `S`-claims-the-other, on-chain both sides | no | todo |

**Gated (`needs:owner`):** mainnet (BTC or NIM), real funds, on-device.

## Per-cycle gate (same as the NIM loop)
`cargo test` + `cargo clippy -D warnings` + `cargo fmt --check` + `scripts/size-guard.sh` green
locally; the `bitcoin` crate + `secp256k1` live behind the `bitcoin-leg` feature so the core stays
lean (like `gateway-rpc`). Byte-validate each BTC artifact against `bitcoinjs-lib`. Commit per goal
to `feat/mesh-swap`; never touch `main`.
