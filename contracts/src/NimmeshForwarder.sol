// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.24;

/// @title NimmeshForwarder — the minimal ERC-2771 trusted forwarder for relayer-sponsored
/// funding (G7, #78; ADR-0006 / ADR-0008)
/// @notice The gas-abstraction seam: a user who holds USDC but no POL signs an EIP-712
/// `ForwardRequest`; a relayer submits it here and pays the gas; this contract verifies the
/// signature, burns the nonce, appends the signer to the calldata (ERC-2771), and relays to the
/// target — so `NimmeshHtlc._msgSender()` resolves to the USER and `newSwapWithPermit` pulls the
/// USER's tokens. The relayer can grief (not relay) but cannot steal or forge: the signature
/// covers every field including the calldata, and a tampered request recovers a different signer.
///
/// The CLAIM path needs no forwarder at all — `NimmeshHtlc.withdraw`/`refund` are caller-open
/// with fixed payouts (ADR-0007), so any gas-paying party can land them directly. This contract
/// exists for FUNDING attribution only. Hand-rolled, no OZ dependency (the repo's no-framework
/// rule); the shape mirrors the classic minimal forwarder.
contract NimmeshForwarder {
    /// One relayable call, signed as EIP-712 typed data by `from`.
    struct ForwardRequest {
        address from; // the signer the target will see via _msgSender()
        address to; // the target contract (the HTLC)
        uint256 value; // native value to forward (0 for our calls)
        uint256 gas; // gas to forward to the inner call
        uint256 nonce; // must equal nonces[from] (strictly sequential)
        uint256 deadline; // Unix seconds; the request dies after this
        bytes data; // the target calldata (the signer is appended on relay)
    }

    bytes32 private constant REQUEST_TYPEHASH = keccak256(
        "ForwardRequest(address from,address to,uint256 value,uint256 gas,uint256 nonce,uint256 deadline,bytes data)"
    );
    bytes32 private constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");

    /// The EIP-712 domain (name "NimmeshForwarder", version "1") — read this LIVE when building
    /// request digests off-chain rather than reconstructing it.
    bytes32 public immutable DOMAIN_SEPARATOR;

    /// Strictly sequential per-signer request nonces (replay protection).
    mapping(address => uint256) public nonces;

    error ExpiredRequest(uint256 deadline, uint256 nowTs);
    error WrongNonce(uint256 given, uint256 expected);
    error BadSignature();
    error InsufficientRelayGas();

    /// One relayed call. `success` is the TARGET's outcome — the forwarder does not bubble the
    /// target's revert (a relayer should not burn its whole tx on a target-side failure), so
    /// observers must check effects (e.g. `getSwap`) or this flag, never assume.
    event Forwarded(address indexed from, address indexed to, uint256 nonce, bool success);

    constructor() {
        DOMAIN_SEPARATOR = keccak256(
            abi.encode(DOMAIN_TYPEHASH, keccak256("NimmeshForwarder"), keccak256("1"), block.chainid, address(this))
        );
    }

    /// The EIP-712 digest the user signs for `req` (dynamic `data` rides as `keccak256(data)`
    /// per EIP-712).
    function requestDigest(ForwardRequest calldata req) public view returns (bytes32) {
        return keccak256(
            abi.encodePacked(
                "\x19\x01",
                DOMAIN_SEPARATOR,
                keccak256(
                    abi.encode(
                        REQUEST_TYPEHASH,
                        req.from,
                        req.to,
                        req.value,
                        req.gas,
                        req.nonce,
                        req.deadline,
                        keccak256(req.data)
                    )
                )
            )
        );
    }

    /// Would `execute` accept this request right now? (Signature + nonce + deadline.)
    function verify(ForwardRequest calldata req, uint8 v, bytes32 r, bytes32 s) public view returns (bool) {
        if (block.timestamp > req.deadline || req.nonce != nonces[req.from]) {
            return false;
        }
        address signer = ecrecover(requestDigest(req), v, r, s);
        return signer != address(0) && signer == req.from;
    }

    /// Relay one signed request: checks-effects (nonce burn) before the call; the target sees
    /// `msg.sender == forwarder` and recovers the real sender from the appended 20 bytes.
    function execute(ForwardRequest calldata req, uint8 v, bytes32 r, bytes32 s)
        external
        payable
        returns (bool success, bytes memory ret)
    {
        if (block.timestamp > req.deadline) revert ExpiredRequest(req.deadline, block.timestamp);
        if (req.nonce != nonces[req.from]) revert WrongNonce(req.nonce, nonces[req.from]);
        address signer = ecrecover(requestDigest(req), v, r, s);
        if (signer == address(0) || signer != req.from) revert BadSignature();
        nonces[req.from] = req.nonce + 1;

        (success, ret) = req.to.call{gas: req.gas, value: req.value}(abi.encodePacked(req.data, req.from));

        // EIP-150 under-gas guard: a relayer that under-provisions the outer tx can make the
        // inner call fail with OOG while looking like a target-side failure. If we do not hold
        // at least 1/63rd of the requested gas AFTER the call, the relayer under-gassed us —
        // fail the whole tx loudly instead of reporting a false target failure.
        if (gasleft() < req.gas / 63) revert InsufficientRelayGas();

        emit Forwarded(req.from, req.to, req.nonce, success);
    }
}
