//! # evm_abi — hand-built EVM ABI calldata for the USDC HTLC + ERC-20 calls (P3, behind `polygon-leg`)
//!
//! The calldata the Polygon HTLC leg ([`crate::swap_usdc_leg`]) and its USDC token approvals will
//! eventually broadcast — built by hand, **no `ethers`/`web3` dependency**. This is the standard ABI
//! **head encoding** (32-byte words), which is DIFFERENT from the `abi.encodePacked` layout
//! `usdc_swap_id` uses: here every argument occupies a full 32-byte word (a `uint256` is a
//! big-endian word, an `address` is the 20-byte value right-aligned in a word, a `bytes32` is itself).
//!
//! `calldata = function_selector(sig) ++ word(arg0) ++ word(arg1) ++ …`. Every type we need is
//! "static" (fixed-size, no dynamic offset/length tail), so the encoding is a simple concatenation.
//!
//! **Money-path stays gated.** This module only BUILDS bytes — it never signs or broadcasts (that's
//! P4, gated behind a key seam + testnet-only). The selectors are pinned to public ERC-20 constants
//! (`approve` = `0x095ea7b3`, `transferFrom` = `0x23b872dd`, validated in [`crate::evm`] too); the
//! famous Uniswap-V2-router `approve` calldata anchors the full byte layout in the tests.

use crate::evm::function_selector;
use crate::swap_leg::HASH_LEN;
use crate::swap_usdc_leg::{EvmAddress, UsdcSwapId};

/// One ABI word — 32 bytes.
pub const WORD: usize = 32;

/// Encode a `u64` as an ABI `uint256` word: 32 bytes big-endian, high bytes zero. (USDC amounts +
/// timelocks fit in `u64`; the full 256-bit range is never needed for our calls.)
pub fn word_u256(v: u64) -> [u8; WORD] {
    let mut w = [0u8; WORD];
    w[WORD - 8..].copy_from_slice(&v.to_be_bytes());
    w
}

/// Encode a 20-byte `address` as an ABI word: the address right-aligned in 32 bytes (12 leading
/// zero bytes), exactly how Solidity ABI-encodes an `address` argument.
pub fn word_address(addr: &EvmAddress) -> [u8; WORD] {
    let mut w = [0u8; WORD];
    w[WORD - 20..].copy_from_slice(addr);
    w
}

/// Start a calldata buffer with a function's 4-byte selector.
fn calldata(signature: &str) -> Vec<u8> {
    function_selector(signature).to_vec()
}

// ── ERC-20 (USDC) ──────────────────────────────────────────────────────────────────────────────

/// ERC-20 `approve(address spender, uint256 amount)` calldata (selector `0x095ea7b3`). The HTLC
/// contract needs an allowance before it can `transferFrom` the funder's USDC into escrow.
pub fn erc20_approve(spender: &EvmAddress, amount: u64) -> Vec<u8> {
    let mut cd = calldata("approve(address,uint256)");
    cd.extend_from_slice(&word_address(spender));
    cd.extend_from_slice(&word_u256(amount));
    cd
}

/// ERC-20 `transferFrom(address from, address to, uint256 amount)` calldata (selector `0x23b872dd`).
/// The HTLC's `newSwap` pulls the funder's USDC into escrow with this.
pub fn erc20_transfer_from(from: &EvmAddress, to: &EvmAddress, amount: u64) -> Vec<u8> {
    let mut cd = calldata("transferFrom(address,address,uint256)");
    cd.extend_from_slice(&word_address(from));
    cd.extend_from_slice(&word_address(to));
    cd.extend_from_slice(&word_u256(amount));
    cd
}

// ── HTLC contract ──────────────────────────────────────────────────────────────────────────────

/// HTLC `newSwap(address receiver, uint256 amount, bytes32 hashlock, uint256 timelock)` calldata —
/// escrows `amount` micro-USDC under the SHA-256 `hashlock` for `receiver` until `timelock`. The
/// contract derives the swap id on-chain (see [`crate::swap_usdc_leg::usdc_swap_id`]).
pub fn htlc_new_swap(
    receiver: &EvmAddress,
    amount: u64,
    hashlock: &[u8; HASH_LEN],
    timelock: u64,
) -> Vec<u8> {
    let mut cd = calldata("newSwap(address,uint256,bytes32,uint256)");
    cd.extend_from_slice(&word_address(receiver));
    cd.extend_from_slice(&word_u256(amount));
    cd.extend_from_slice(hashlock); // bytes32 — passthrough
    cd.extend_from_slice(&word_u256(timelock));
    cd
}

/// HTLC `withdraw(bytes32 swapId, bytes32 secret)` calldata — the recipient claims by revealing the
/// secret; the contract checks `sha256(secret) == hashlock` (the `0x02` precompile) before the timeout.
pub fn htlc_withdraw(swap_id: &UsdcSwapId, secret: &[u8; HASH_LEN]) -> Vec<u8> {
    let mut cd = calldata("withdraw(bytes32,bytes32)");
    cd.extend_from_slice(swap_id); // bytes32
    cd.extend_from_slice(secret); // bytes32
    cd
}

/// HTLC `refund(bytes32 swapId)` calldata — the funder reclaims the escrow after the timeout.
pub fn htlc_refund(swap_id: &UsdcSwapId) -> Vec<u8> {
    let mut cd = calldata("refund(bytes32)");
    cd.extend_from_slice(swap_id); // bytes32
    cd
}

/// HTLC `newSwapWithPermit(address,uint256,bytes32,uint256,uint256,uint8,bytes32,bytes32)`
/// calldata (selector `0x0dc15831`) — **single-transaction funding**: the EIP-2612 permit and the
/// escrow ride one tx, no prior `approve` (closes S4's approve→transferFrom race; ADR-0007).
/// Build the digest with [`crate::evm_permit::permit_digest`]; `v` is
/// [`crate::evm_permit::permit_sig_v`] (27/28), `r`/`s` are the signature words.
#[allow(clippy::too_many_arguments)]
pub fn htlc_new_swap_with_permit(
    receiver: &EvmAddress,
    amount: u64,
    hashlock: &[u8; HASH_LEN],
    timelock: u64,
    deadline: u64,
    v: u8,
    r: &[u8; 32],
    s: &[u8; 32],
) -> Vec<u8> {
    let mut cd = calldata(
        "newSwapWithPermit(address,uint256,bytes32,uint256,uint256,uint8,bytes32,bytes32)",
    );
    cd.extend_from_slice(&word_address(receiver));
    cd.extend_from_slice(&word_u256(amount));
    cd.extend_from_slice(hashlock); // bytes32 — passthrough
    cd.extend_from_slice(&word_u256(timelock));
    cd.extend_from_slice(&word_u256(deadline));
    cd.extend_from_slice(&word_u256(u64::from(v))); // uint8 — right-aligned in a word
    cd.extend_from_slice(r); // bytes32
    cd.extend_from_slice(s); // bytes32
    cd
}

/// ERC-20 `DOMAIN_SEPARATOR()` calldata (selector `0x3644e515`) — read the token's LIVE EIP-712
/// domain for [`crate::evm_permit::permit_digest`] (deployments differ; on-chain is the truth).
pub fn erc20_domain_separator() -> Vec<u8> {
    calldata("DOMAIN_SEPARATOR()")
}

/// ERC-20 `nonces(address owner)` calldata (selector `0x7ecebe00`) — the owner's next permit nonce.
pub fn erc20_nonces(owner: &EvmAddress) -> Vec<u8> {
    let mut cd = calldata("nonces(address)");
    cd.extend_from_slice(&word_address(owner));
    cd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn word_u256_is_big_endian_right_aligned() {
        assert_eq!(word_u256(0), [0u8; 32]);
        let one = word_u256(1);
        assert_eq!(one[31], 1);
        assert_eq!(&one[..31], &[0u8; 31]);
        // u64::MAX fills exactly the low 8 bytes; the high 24 stay zero.
        let max = word_u256(u64::MAX);
        assert_eq!(&max[..24], &[0u8; 24]);
        assert_eq!(&max[24..], &[0xffu8; 8]);
        // 256 = 0x0100 lands in the last two bytes.
        assert_eq!(word_u256(256)[30..], [0x01, 0x00]);
    }

    #[test]
    fn word_address_is_left_padded_to_32_bytes() {
        let w = word_address(&[0xAA; 20]);
        assert_eq!(&w[..12], &[0u8; 12]); // 12 leading zero bytes
        assert_eq!(&w[12..], &[0xAAu8; 20]); // then the 20-byte address
    }

    // The famous Uniswap-V2-router address `0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D` — a public,
    // widely-seen constant; its ABI word is the anchor for `approve`'s byte layout. (Bytes written
    // lowercase; the EIP-55 checksum casing is only a string-display convention.)
    const UNIV2_ROUTER: [u8; 20] = [
        0x7a, 0x25, 0x0d, 0x56, 0x30, 0xb4, 0xcf, 0x53, 0x97, 0x39, 0xdf, 0x2c, 0x5d, 0xac, 0xb4,
        0xc6, 0x59, 0xf2, 0x48, 0x8d,
    ];

    #[test]
    fn approve_calldata_matches_the_known_byte_layout() {
        // approve(0x7a25…488D, 1) — selector 095ea7b3 ++ left-padded router ++ uint256(1).
        let expected = hex(concat!(
            "095ea7b3",
            "0000000000000000000000007a250d5630b4cf539739df2c5dacb4c659f2488d",
            "0000000000000000000000000000000000000000000000000000000000000001",
        ));
        assert_eq!(erc20_approve(&UNIV2_ROUTER, 1), expected);
        assert_eq!(erc20_approve(&UNIV2_ROUTER, 1).len(), 4 + 2 * WORD); // 68 bytes
                                                                         // Selector is the public ERC-20 constant.
        assert_eq!(&erc20_approve(&UNIV2_ROUTER, 1)[..4], &hex("095ea7b3")[..]);
    }

    #[test]
    fn transfer_from_calldata_matches_the_known_byte_layout() {
        let from = [0x11u8; 20];
        let to = [0x22u8; 20];
        let cd = erc20_transfer_from(&from, &to, 5_000_000); // 5 USDC
        assert_eq!(&cd[..4], &hex("23b872dd")[..]); // public ERC-20 selector
        assert_eq!(cd.len(), 4 + 3 * WORD); // 100 bytes
        assert_eq!(&cd[4..36], &word_address(&from)[..]); // from word
        assert_eq!(&cd[36..68], &word_address(&to)[..]); // to word
        assert_eq!(&cd[68..100], &word_u256(5_000_000)[..]); // amount word
    }

    #[test]
    fn new_swap_calldata_lays_out_every_arg_in_order() {
        let receiver = [0xAB; 20];
        let hashlock = [0xC7; 32];
        let cd = htlc_new_swap(&receiver, 25_000_000, &hashlock, 5_000);
        // selector ++ receiver ++ amount ++ hashlock(bytes32 verbatim) ++ timelock
        assert_eq!(
            &cd[..4],
            &function_selector("newSwap(address,uint256,bytes32,uint256)")[..]
        );
        assert_eq!(cd.len(), 4 + 4 * WORD); // 132 bytes
        assert_eq!(&cd[4..36], &word_address(&receiver)[..]);
        assert_eq!(&cd[36..68], &word_u256(25_000_000)[..]);
        assert_eq!(&cd[68..100], &hashlock[..]); // bytes32 passthrough, not re-padded
        assert_eq!(&cd[100..132], &word_u256(5_000)[..]);
    }

    #[test]
    fn withdraw_calldata_carries_swap_id_then_secret_verbatim() {
        let swap_id = [0x1D; 32];
        let secret = [0x5E; 32];
        let cd = htlc_withdraw(&swap_id, &secret);
        assert_eq!(
            &cd[..4],
            &function_selector("withdraw(bytes32,bytes32)")[..]
        );
        assert_eq!(cd.len(), 4 + 2 * WORD); // 68 bytes
        assert_eq!(&cd[4..36], &swap_id[..]); // bytes32 verbatim
        assert_eq!(&cd[36..68], &secret[..]); // bytes32 verbatim
    }

    #[test]
    fn refund_calldata_is_selector_plus_swap_id() {
        let swap_id = [0x2F; 32];
        let cd = htlc_refund(&swap_id);
        assert_eq!(&cd[..4], &function_selector("refund(bytes32)")[..]);
        assert_eq!(cd.len(), 4 + WORD); // 36 bytes
        assert_eq!(&cd[4..36], &swap_id[..]);
    }

    #[test]
    fn new_swap_with_permit_calldata_lays_out_all_eight_words() {
        let receiver = [0xAB; 20];
        let hashlock = [0xC7; 32];
        let r = [0x04; 32];
        let sig_s = [0x05; 32];
        let cd = htlc_new_swap_with_permit(
            &receiver, 25_000_000, &hashlock, 5_000, 6_000, 27, &r, &sig_s,
        );
        assert_eq!(&cd[..4], &hex("0dc15831")[..]); // cast sig of the canonical signature
        assert_eq!(cd.len(), 4 + 8 * WORD); // 260 bytes
        assert_eq!(&cd[4..36], &word_address(&receiver)[..]);
        assert_eq!(&cd[36..68], &word_u256(25_000_000)[..]);
        assert_eq!(&cd[68..100], &hashlock[..]);
        assert_eq!(&cd[100..132], &word_u256(5_000)[..]);
        assert_eq!(&cd[132..164], &word_u256(6_000)[..]);
        assert_eq!(&cd[164..196], &word_u256(27)[..]); // uint8 v, right-aligned
        assert_eq!(&cd[196..228], &r[..]);
        assert_eq!(&cd[228..260], &sig_s[..]);
    }

    #[test]
    fn the_permit_read_selectors_match_their_public_constants() {
        assert_eq!(&erc20_domain_separator()[..], &hex("3644e515")[..]); // DOMAIN_SEPARATOR()
        let cd = erc20_nonces(&[0x44; 20]);
        assert_eq!(&cd[..4], &hex("7ecebe00")[..]); // nonces(address)
        assert_eq!(cd.len(), 4 + WORD);
        assert_eq!(&cd[4..36], &word_address(&[0x44; 20])[..]);
    }

    #[test]
    fn the_two_erc20_selectors_match_their_public_constants() {
        // Independent of any builder: the canonical signatures hash to the documented selectors.
        assert_eq!(&erc20_approve(&[0; 20], 0)[..4], &hex("095ea7b3")[..]);
        assert_eq!(
            &erc20_transfer_from(&[0; 20], &[0; 20], 0)[..4],
            &hex("23b872dd")[..]
        );
    }
}
