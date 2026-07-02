# BTC key seam — on-device secp256k1 signing (`BtcEnclaveKey`)

The Bitcoin leg of a mesh swap must sign on-device, the same way the NIM leg does — the **secret
never crosses the FFI boundary**. This is the BTC mirror of `nimiq::signer::EnclaveKey`.

## The seam (`crates/nimmesh-core/src/btc.rs`)

```rust
#[uniffi::export(with_foreign)]
pub trait BtcEnclaveKey: Send + Sync {
    fn public_key(&self) -> Vec<u8>;          // 33-byte compressed secp256k1 (safe to leak)
    fn sign_sighash(&self, sighash: Vec<u8>) -> Vec<u8>; // DER ECDSA over the 32-byte BIP143 sighash
}
```

Only two things ever leave the key:

| out of the enclave | used for |
| --- | --- |
| the **33-byte compressed public key** | the HTLC redeem script (`recipient_pubkey` / `sender_pubkey`) + the P2WPKH payout address |
| a **DER ECDSA signature** over the 32-byte BIP143 sighash | the spend witness (`[sig, …branch…, redeemScript]`) |

The secp256k1 **secret stays behind the trait** — exactly like the Ed25519 seed behind
`EnclaveKey`. A native iOS impl signs via a non-exportable `SecKey`; Android via a hardware-backed
`KeyStore` entry. One wallet seed derives **both** keys (BIP32/BIP84 for BTC, the Nimiq HD path for
NIM); that derivation is the native layer's job — the core only ever holds the opaque handle.

## Why it's safe even if the native signer misbehaves

`BtcHtlcParams::sign_spend` **re-normalizes the returned signature to low-S** before assembling the
witness. So a native enclave that returns a high-S (BIP146-non-standard, relay-rejected) signature
still yields a consensus-canonical tx. For the in-memory reference key the signature is already
low-S, so the bytes stay **byte-identical to `bitcoinjs-lib`**.

## API

- `claim_tx_with_key(funded, preimage, dest_spk, payout_sat, &dyn BtcEnclaveKey)` — the on-device path.
- `refund_tx_with_key(funded, dest_spk, payout_sat, &dyn BtcEnclaveKey)` — the on-device refund path.
- `claim_tx` / `refund_tx` (raw `&[u8;32]`) are thin **dev/test** wrappers that build an
  `InMemoryBtcEnclaveKey` and call the `*_with_key` variants — so there is **one** signing path.

`InMemoryBtcEnclaveKey` is the in-process reference impl (RFC6979 deterministic + low-S). It is
**not** FFI-exported: a secret-bearing constructor must never appear on the UniFFI surface.

## Proof it's faithful

- `enclave_seam_signs_byte_identically_to_raw_key` — the seam path produces byte-identical claim +
  refund txs to the raw-key path, and `public_key()` matches the compressed pubkeys baked into the
  reference redeem script.
- Because `claim_tx`/`refund_tx` now delegate through the seam, the existing
  `signed_claim_tx_matches_bitcoinjs` / `signed_refund_tx_matches_bitcoinjs` tests prove the **seam**
  output equals `bitcoinjs-lib` byte-for-byte — the same bytes the live testnet3 swap already confirmed.

## Packaging note

The trait is `#[uniffi::export(with_foreign)]` inside the `bitcoin-leg`-gated module, so it only
appears in the generated Swift/Kotlin bindings when the binding build enables `bitcoin-leg`
(consistent with the rest of the BTC leg). The pure-protocol default build is unaffected.
