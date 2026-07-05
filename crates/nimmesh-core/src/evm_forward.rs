//! # evm_forward — EIP-712 `ForwardRequest` pre-images + `execute` calldata (G7 #78, behind `polygon-leg`)
//!
//! The off-chain half of relayer-sponsored funding (ADR-0006/0008): the USER — who holds USDC
//! but no POL — signs a `NimmeshForwarder.ForwardRequest` wrapping the HTLC calldata (usually
//! [`crate::evm_abi::htlc_new_swap_with_permit`]); the RELAYER submits it with
//! [`forwarder_execute`] and pays the gas; `NimmeshHtlc._msgSender()` resolves to the user. The
//! claim path needs none of this — `withdraw`/`refund` are caller-open (ADR-0007).
//!
//! Sign the digest with any [`crate::evm_rlp::EvmSigner`]; the signature `v` is the same
//! EIP-712 27/28 rule as permits ([`crate::evm_permit::permit_sig_v`]). Build the forwarder's
//! domain separator with [`crate::evm_permit::eip712_domain_separator`] (name
//! "NimmeshForwarder", version "1") — or better, READ the deployed forwarder's
//! `DOMAIN_SEPARATOR()` ([`crate::evm_abi::erc20_domain_separator`] builds that selector, and
//! [`crate::evm_abi::erc20_nonces`] the `nonces(address)` read — both contracts use the same
//! canonical signatures). Vectors below are cast-derived; the Foundry suite proves the same
//! digests verify on-chain.

use crate::evm::{function_selector, keccak256};
use crate::evm_abi::{word_address, word_u256, WORD};
use crate::swap_usdc_leg::EvmAddress;

/// `keccak256` of the canonical `ForwardRequest` struct signature
/// (`0xca55ce03…1fa82` — asserted against the cast constant in the tests).
pub fn forward_request_typehash() -> [u8; 32] {
    keccak256(
        b"ForwardRequest(address from,address to,uint256 value,uint256 gas,uint256 nonce,uint256 deadline,bytes data)",
    )
}

/// The 32-byte EIP-712 digest the user signs for one relayed call:
/// `keccak256(0x1901 ‖ domainSeparator ‖ keccak256(abi.encode(TYPEHASH, from, to, value, gas,
/// nonce, deadline, keccak256(data))))` — dynamic `data` rides as its hash, per EIP-712.
/// `nonce` is the forwarder's `nonces(from)`; `deadline` is a Unix-seconds expiry.
#[allow(clippy::too_many_arguments)]
pub fn forward_request_digest(
    domain_separator: &[u8; 32],
    from: &EvmAddress,
    to: &EvmAddress,
    value: u64,
    gas: u64,
    nonce: u64,
    deadline: u64,
    data: &[u8],
) -> [u8; 32] {
    let mut enc = Vec::with_capacity(8 * WORD);
    enc.extend_from_slice(&forward_request_typehash());
    enc.extend_from_slice(&word_address(from));
    enc.extend_from_slice(&word_address(to));
    enc.extend_from_slice(&word_u256(value));
    enc.extend_from_slice(&word_u256(gas));
    enc.extend_from_slice(&word_u256(nonce));
    enc.extend_from_slice(&word_u256(deadline));
    enc.extend_from_slice(&keccak256(data));
    let struct_hash = keccak256(&enc);
    let mut pre = Vec::with_capacity(2 + 2 * 32);
    pre.extend_from_slice(&[0x19, 0x01]);
    pre.extend_from_slice(domain_separator);
    pre.extend_from_slice(&struct_hash);
    keccak256(&pre)
}

/// `NimmeshForwarder.execute((address,address,uint256,uint256,uint256,uint256,bytes),uint8,bytes32,bytes32)`
/// calldata (selector `0x3f7f44c8`) — the ONE dynamic-tail shape in the EVM stack, hand-built:
/// head = `[tuple-offset(0x80), v, r, s]`; the tuple's 7 words end in `data`'s inner offset
/// (`0xE0`); the tail is `len(data)` + the bytes zero-padded to a word. Byte-anchored against
/// `cast calldata` in the tests.
#[allow(clippy::too_many_arguments)]
pub fn forwarder_execute(
    from: &EvmAddress,
    to: &EvmAddress,
    value: u64,
    gas: u64,
    nonce: u64,
    deadline: u64,
    data: &[u8],
    v: u8,
    r: &[u8; 32],
    s: &[u8; 32],
) -> Vec<u8> {
    let padded = data.len().div_ceil(WORD) * WORD;
    let mut cd = function_selector(
        "execute((address,address,uint256,uint256,uint256,uint256,bytes),uint8,bytes32,bytes32)",
    )
    .to_vec();
    // head: the tuple is dynamic (it contains `bytes`), so its head slot is an offset.
    cd.extend_from_slice(&word_u256(4 * WORD as u64)); // 0x80 — the tuple starts after 4 head words
    cd.extend_from_slice(&word_u256(u64::from(v)));
    cd.extend_from_slice(r);
    cd.extend_from_slice(s);
    // the tuple: 6 static fields + data's offset WITHIN the tuple (7 words → 0xE0).
    cd.extend_from_slice(&word_address(from));
    cd.extend_from_slice(&word_address(to));
    cd.extend_from_slice(&word_u256(value));
    cd.extend_from_slice(&word_u256(gas));
    cd.extend_from_slice(&word_u256(nonce));
    cd.extend_from_slice(&word_u256(deadline));
    cd.extend_from_slice(&word_u256(7 * WORD as u64)); // 0xE0
                                                       // the tail: length + zero-padded bytes.
    cd.extend_from_slice(&word_u256(data.len() as u64));
    cd.extend_from_slice(data);
    cd.resize(cd.len() + (padded - data.len()), 0);
    cd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm_permit::eip712_domain_separator;
    use crate::nimiq::hex::{bytes_to_hex, hex_to_bytes};

    const FROM: EvmAddress = [0x11; 20];
    const TO: EvmAddress = [0x22; 20];

    #[test]
    fn typehash_matches_the_cast_constant() {
        assert_eq!(
            bytes_to_hex(&forward_request_typehash()),
            "ca55ce0307ac53917d02c1387bc157c21729fef42093fa6ec5e3cb506dd1fa82"
        );
    }

    #[test]
    fn request_digest_matches_the_cast_vector_and_binds_the_calldata() {
        // Domain: NimmeshForwarder/"1"/31337 (Foundry's chain id)/forwarder 0x22…22 — the same
        // generic EIP-712 domain builder permits use.
        let ds = eip712_domain_separator("NimmeshForwarder", "1", 31_337, &TO);
        assert_eq!(
            bytes_to_hex(&ds),
            "0691b3a357457d7a8fc50d3fe3903138ca386f2dd13e5535b48e5d1ebffed369"
        );
        let data = hex_to_bytes("deadbeef").unwrap();
        let digest = forward_request_digest(&ds, &FROM, &TO, 0, 400_000, 0, 600, &data);
        assert_eq!(
            bytes_to_hex(&digest),
            "525a70a5ce936be85cee901f740cd811f735768102a6d86da9fbfb72779ec8c2"
        );
        // The signature covers the CALLDATA — a relayer swapping in different bytes under the
        // same signature recovers a different signer (the Foundry tamper test's off-chain twin).
        let tampered = hex_to_bytes("deadbeee").unwrap();
        assert_ne!(
            digest,
            forward_request_digest(&ds, &FROM, &TO, 0, 400_000, 0, 600, &tampered)
        );
        assert_ne!(
            digest,
            forward_request_digest(&ds, &FROM, &TO, 0, 400_000, 1, 600, &data)
        );
        assert_ne!(
            digest,
            forward_request_digest(&ds, &TO, &TO, 0, 400_000, 0, 600, &data)
        );
    }

    #[test]
    fn execute_calldata_byte_matches_cast() {
        // The full `cast calldata` reference for (FROM, TO, 0, 400000, 0, 600, 0xdeadbeef),
        // v=27, r=0x33…33, s=0x44…44 — selector 0x3f7f44c8, tuple offset 0x80, inner data
        // offset 0xE0, 4-byte payload padded to a word.
        let expected = concat!(
            "3f7f44c8",
            "0000000000000000000000000000000000000000000000000000000000000080",
            "000000000000000000000000000000000000000000000000000000000000001b",
            "3333333333333333333333333333333333333333333333333333333333333333",
            "4444444444444444444444444444444444444444444444444444444444444444",
            "0000000000000000000000001111111111111111111111111111111111111111",
            "0000000000000000000000002222222222222222222222222222222222222222",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000061a80",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000258",
            "00000000000000000000000000000000000000000000000000000000000000e0",
            "0000000000000000000000000000000000000000000000000000000000000004",
            "deadbeef00000000000000000000000000000000000000000000000000000000",
        );
        let data = hex_to_bytes("deadbeef").unwrap();
        let cd = forwarder_execute(
            &FROM,
            &TO,
            0,
            400_000,
            0,
            600,
            &data,
            27,
            &[0x33; 32],
            &[0x44; 32],
        );
        assert_eq!(bytes_to_hex(&cd), expected);
        // An exact-multiple payload needs no padding — length math stays exact.
        let data32 = vec![0xAB; 32];
        let cd32 = forwarder_execute(&FROM, &TO, 0, 1, 0, 1, &data32, 28, &[0; 32], &[0; 32]);
        assert_eq!(cd32.len(), 4 + 11 * WORD + WORD + 32);
    }
}
