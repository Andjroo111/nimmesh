//! # evm_permit — EIP-2612 `permit` signing pre-images (single-tx escrow funding, behind `polygon-leg`)
//!
//! The off-chain half of single-transaction USDC funding: `NimmeshHtlc.newSwapWithPermit`
//! (`contracts/`, ADR-0007) takes an EIP-2612 permit signature so the funder needs no prior
//! `approve` transaction — closing S4's approve→transferFrom race on the live path exactly as the
//! Foundry suite proves it against the mock. This module builds the **EIP-712 digest** the
//! funder's secp256k1 key signs — hand-rolled like the rest of the EVM stack (no `ethers`).
//!
//! Sign the digest with any [`crate::evm_rlp::EvmSigner`] (e.g.
//! [`crate::evm_signer::LocalEvmKey::sign_hash`]); the permit signature's `v` is
//! [`permit_sig_v`] (27/28 — an EIP-712 signature, NOT the EIP-155 transaction `v`). The
//! calldata that carries it is [`crate::evm_abi::htlc_new_swap_with_permit`].
//!
//! On the LIVE path, prefer READING the token's `DOMAIN_SEPARATOR()`
//! ([`crate::evm_abi::erc20_domain_separator`] + `eth_call`) over rebuilding it: deployments
//! differ (Amoy USDC is name "USDC", version "2") and the on-chain value is the truth.
//! [`eip712_domain_separator`] exists for offline vectors and tokens whose fields are pinned
//! (the Foundry `MockUsdc`: name "Mock USDC", version "1").

use crate::evm::keccak256;
use crate::evm_abi::{word_address, word_u256, WORD};
use crate::swap_usdc_leg::EvmAddress;

/// `keccak256` of the canonical EIP-2612 `Permit` struct signature
/// (`0x6e71edae…26c9` — asserted against the published constant in the tests).
pub fn permit_typehash() -> [u8; 32] {
    keccak256(b"Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")
}

/// `keccak256` of the canonical 4-field EIP-712 domain signature USDC (and `MockUsdc`) use
/// (`0x8b73c3c6…400f`).
pub fn eip712_domain_typehash() -> [u8; 32] {
    keccak256(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")
}

/// Rebuild a token's EIP-712 domain separator from its fields:
/// `keccak256(abi.encode(DOMAIN_TYPEHASH, keccak256(name), keccak256(version), chainId, token))`.
pub fn eip712_domain_separator(
    name: &str,
    version: &str,
    chain_id: u64,
    verifying_contract: &EvmAddress,
) -> [u8; 32] {
    let mut enc = Vec::with_capacity(5 * WORD);
    enc.extend_from_slice(&eip712_domain_typehash());
    enc.extend_from_slice(&keccak256(name.as_bytes()));
    enc.extend_from_slice(&keccak256(version.as_bytes()));
    enc.extend_from_slice(&word_u256(chain_id));
    enc.extend_from_slice(&word_address(verifying_contract));
    keccak256(&enc)
}

/// The 32-byte EIP-712 digest the funder signs to permit `spender` to pull `value` micro-USDC:
/// `keccak256(0x1901 ‖ domainSeparator ‖ keccak256(abi.encode(PERMIT_TYPEHASH, owner, spender,
/// value, nonce, deadline)))`. `nonce` is the token's `nonces(owner)`
/// ([`crate::evm_abi::erc20_nonces`] + `eth_call`); `deadline` is a Unix-seconds expiry.
pub fn permit_digest(
    domain_separator: &[u8; 32],
    owner: &EvmAddress,
    spender: &EvmAddress,
    value: u64,
    nonce: u64,
    deadline: u64,
) -> [u8; 32] {
    let mut enc = Vec::with_capacity(6 * WORD);
    enc.extend_from_slice(&permit_typehash());
    enc.extend_from_slice(&word_address(owner));
    enc.extend_from_slice(&word_address(spender));
    enc.extend_from_slice(&word_u256(value));
    enc.extend_from_slice(&word_u256(nonce));
    enc.extend_from_slice(&word_u256(deadline));
    let struct_hash = keccak256(&enc);
    let mut pre = Vec::with_capacity(2 + 2 * 32);
    pre.extend_from_slice(&[0x19, 0x01]);
    pre.extend_from_slice(domain_separator);
    pre.extend_from_slice(&struct_hash);
    keccak256(&pre)
}

/// The EIP-712/EIP-2612 signature `v` from a recoverable-ECDSA recovery id: `27 + recovery_id`
/// (NOT the EIP-155 transaction `v`, which folds in the chain id).
pub fn permit_sig_v(recovery_id: u8) -> u8 {
    27 + recovery_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm_rlp::EvmSigner;
    use crate::evm_signer::LocalEvmKey;
    use crate::nimiq::hex::bytes_to_hex;

    #[test]
    fn typehashes_match_the_published_constants() {
        // The canonical EIP-2612 + EIP-712 constants (independently: `cast keccak "<sig>"`).
        assert_eq!(
            bytes_to_hex(&permit_typehash()),
            "6e71edae12b1b97f4d1f60370fef10105fa2faae0126114a169c64845d6126c9"
        );
        assert_eq!(
            bytes_to_hex(&eip712_domain_typehash()),
            "8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f"
        );
    }

    #[test]
    fn domain_separator_matches_the_cast_vector() {
        // name "Mock USDC" · version "1" · chainId 31337 (Foundry's default) · token 0x22…22 —
        // the exact fields the Foundry MockUsdc pins; vector derived with cast keccak.
        let ds = eip712_domain_separator("Mock USDC", "1", 31_337, &[0x22; 20]);
        assert_eq!(
            bytes_to_hex(&ds),
            "a0cdf1e2cd86205e216305bbf8790600c96df5d3b23d1f79338283ef7185a2ff"
        );
    }

    #[test]
    fn permit_digest_matches_the_cast_vector_and_binds_every_field() {
        let ds = eip712_domain_separator("Mock USDC", "1", 31_337, &[0x22; 20]);
        let owner = [0x11; 20];
        let spender = [0x33; 20];
        let digest = permit_digest(&ds, &owner, &spender, 25_000_000, 0, 5_000);
        assert_eq!(
            bytes_to_hex(&digest),
            "a391306a95c6553efda178a65e1b4c314226ec0fa1d6f1d52d2398060e1a0d32"
        );
        // Bound to every parameter — any change moves the digest (no cross-signature reuse).
        assert_ne!(
            digest,
            permit_digest(&ds, &spender, &spender, 25_000_000, 0, 5_000)
        );
        assert_ne!(
            digest,
            permit_digest(&ds, &owner, &owner, 25_000_000, 0, 5_000)
        );
        assert_ne!(
            digest,
            permit_digest(&ds, &owner, &spender, 25_000_001, 0, 5_000)
        );
        assert_ne!(
            digest,
            permit_digest(&ds, &owner, &spender, 25_000_000, 1, 5_000)
        );
        assert_ne!(
            digest,
            permit_digest(&ds, &owner, &spender, 25_000_000, 0, 5_001)
        );
        let other_ds = eip712_domain_separator("USDC", "2", 80_002, &[0x22; 20]);
        assert_ne!(
            digest,
            permit_digest(&other_ds, &owner, &spender, 25_000_000, 0, 5_000)
        );
    }

    #[test]
    fn a_local_key_signs_the_digest_with_an_eip712_v() {
        let key = LocalEvmKey::from_secret(&[0x42; 32]).unwrap();
        let ds = eip712_domain_separator("Mock USDC", "1", 31_337, &[0x22; 20]);
        let digest = permit_digest(&ds, &key.address(), &[0x33; 20], 1_000_000, 0, 9_999);
        let (r, s, recovery_id) = key.sign_hash(digest);
        assert!(recovery_id <= 1); // low-s normalized recoverable ECDSA
        let v = permit_sig_v(recovery_id);
        assert!(v == 27 || v == 28);
        assert_ne!(r, [0u8; 32]);
        assert_ne!(s, [0u8; 32]);
    }
}
