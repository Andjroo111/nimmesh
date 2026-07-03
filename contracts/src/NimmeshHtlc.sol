// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.24;

/// Minimal ERC-20 surface the escrow needs. Hand-declared instead of importing a framework —
/// the repo vendors everything (see `crates/nimmesh-core/src/evm_abi.rs`, same rule). USDC
/// returns `bool` from both calls, so the checks below decode it; this contract is written
/// for USDC, not for non-standard tokens that return no data.
interface IERC20 {
    function transfer(address to, uint256 value) external returns (bool);
    function transferFrom(address from, address to, uint256 value) external returns (bool);
}

/// EIP-2612 `permit` surface (USDC on Polygon implements it): lets `newSwapWithPermit` fund the
/// escrow in a single transaction with no separate `approve` — closing S4's approve→transferFrom
/// race (see docs/swap/INTEGRATION-AGENDA.md, G5).
interface IERC20Permit {
    function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s)
        external;
}

/// @title NimmeshHtlc — the USDC hashed-timelock escrow for the NIM⇄USDC swap leg (G5, #76)
/// @notice One deliberate cross-chain choice: the hashlock is verified with **SHA-256** (the
/// `0x02` precompile via Solidity's `sha256`), NOT `keccak256`, so the lock `H = SHA-256(S)` is
/// byte-identical to the NIM and BTC legs and one secret `S` unlocks all three. Keccak is used
/// ONLY for the swap-id slot derivation, exactly mirroring the Rust model
/// (`crates/nimmesh-core/src/swap_usdc_leg.rs::usdc_swap_id`).
///
/// Timing semantics match the Rust model's code (the behavioral source of truth): `withdraw`
/// while `block.timestamp <= timelock`, `refund` strictly after (`block.timestamp > timelock`).
/// No overlap, no gap; the boundary second belongs to the claimer.
///
/// `withdraw` and `refund` are **caller-open with fixed payouts**: anyone may submit them, the
/// USDC always moves to the stored `receiver` / `sender`. That is ADR-0006's self-funded
/// fallback with no forwarder machinery on the claim path — a relayer (or any third party) can
/// land the recipient's claim, and a front-runner just pays the recipient's gas for them.
/// ERC-2771 therefore matters only where the caller's identity matters: `newSwap*`, where the
/// funder is `_msgSender()`. The trusted forwarder is immutable and set at deployment;
/// `address(0)` disables it (pure self-funded behavior). See ADR-0006 / ADR-0007.
contract NimmeshHtlc {
    /// A swap slot's lifecycle. Slots are single-use: once resolved they are never reused
    /// (`newSwap` requires a future timelock, so an identical re-creation of a resolved swap is
    /// impossible — its timelock is in the past). Kept in storage after resolution so the G6
    /// funding verifier can read the outcome.
    enum State {
        None,
        Live,
        Claimed,
        Refunded
    }

    struct Swap {
        address sender; // funder — the refund payout target
        address receiver; // the claim payout target
        uint256 amount; // micro-USDC (6 decimals)
        bytes32 hashlock; // SHA-256(S), shared with the NIM + BTC legs
        uint256 timelock; // Unix seconds: claim while <=, refund after >
        State state;
    }

    /// The escrowed token (USDC). Immutable — one deployment escrows exactly one token.
    IERC20 public immutable token;

    /// ERC-2771 trusted forwarder for meta-tx funding (ADR-0006). `address(0)` = disabled.
    address public immutable trustedForwarder;

    mapping(bytes32 => Swap) private swaps;

    event NewSwap(
        bytes32 indexed swapId,
        address indexed sender,
        address indexed receiver,
        uint256 amount,
        bytes32 hashlock,
        uint256 timelock
    );
    /// The claim publishes the secret on-chain — this is how the NIM funder learns `S` and
    /// settles the other leg. The secret also rides the calldata; the event makes it indexable.
    event Withdrawn(bytes32 indexed swapId, bytes32 secret);
    event Refunded(bytes32 indexed swapId);

    error ZeroReceiver();
    error ZeroAmount();
    error TimelockNotFuture(uint256 timelock, uint256 nowTs);
    error SwapAlreadyExists(bytes32 swapId);
    error UnknownSwap(bytes32 swapId);
    error NotLive(bytes32 swapId);
    error WrongSecret();
    error ClaimWindowClosed(uint256 timelock, uint256 nowTs);
    error TimeoutNotReached(uint256 timelock, uint256 nowTs);
    error TransferFailed();

    constructor(IERC20 usdc, address forwarder) {
        token = usdc;
        trustedForwarder = forwarder;
    }

    /// Escrow `amount` micro-USDC from the caller under `hashlock` for `receiver` until
    /// `timelock`. Requires a prior `approve` — byte-compatible with the calldata the Rust side
    /// builds (`evm_abi::htlc_new_swap`, selector of `newSwap(address,uint256,bytes32,uint256)`).
    function newSwap(address receiver, uint256 amount, bytes32 hashlock, uint256 timelock)
        external
        returns (bytes32 swapId)
    {
        return _newSwap(_msgSender(), receiver, amount, hashlock, timelock);
    }

    /// Single-transaction funding: EIP-2612 `permit` + escrow, no separate `approve` (closes
    /// S4's race). The `permit` call is wrapped in try/catch — if the signature was front-run
    /// and already spent, the allowance is in place and the escrow still succeeds (the standard
    /// permit-with-fallback pattern).
    function newSwapWithPermit(
        address receiver,
        uint256 amount,
        bytes32 hashlock,
        uint256 timelock,
        uint256 deadline,
        uint8 v,
        bytes32 r,
        bytes32 s
    ) external returns (bytes32 swapId) {
        address funder = _msgSender();
        try IERC20Permit(address(token)).permit(funder, address(this), amount, deadline, v, r, s) {} catch {}
        return _newSwap(funder, receiver, amount, hashlock, timelock);
    }

    /// The recipient's claim: reveal `secret`, receive the escrow. Verifies
    /// `sha256(secret) == hashlock` (the 0x02 precompile — the cross-chain lock) and
    /// `block.timestamp <= timelock`. Caller-open: payout is fixed to the stored receiver.
    function withdraw(bytes32 swapId, bytes32 secret) external {
        Swap storage s = swaps[swapId];
        if (s.state == State.None) revert UnknownSwap(swapId);
        if (s.state != State.Live) revert NotLive(swapId);
        if (sha256(abi.encodePacked(secret)) != s.hashlock) revert WrongSecret();
        if (block.timestamp > s.timelock) revert ClaimWindowClosed(s.timelock, block.timestamp);
        s.state = State.Claimed;
        emit Withdrawn(swapId, secret);
        if (!token.transfer(s.receiver, s.amount)) revert TransferFailed();
    }

    /// The funder's escape hatch after the timeout: `block.timestamp > timelock` (strictly —
    /// the boundary second belongs to the claimer). Caller-open: payout is fixed to the funder.
    function refund(bytes32 swapId) external {
        Swap storage s = swaps[swapId];
        if (s.state == State.None) revert UnknownSwap(swapId);
        if (s.state != State.Live) revert NotLive(swapId);
        if (block.timestamp <= s.timelock) revert TimeoutNotReached(s.timelock, block.timestamp);
        s.state = State.Refunded;
        emit Refunded(swapId);
        if (!token.transfer(s.sender, s.amount)) revert TransferFailed();
    }

    /// The slot derivation — MUST stay byte-identical to the Rust model's `usdc_swap_id`:
    /// `keccak256(abi.encodePacked(sender, receiver, amount, hashlock, timelock))`, a
    /// 20+20+32+32+32 = 136-byte pre-image. Deterministic and bound to every parameter.
    function swapIdFor(address sender, address receiver, uint256 amount, bytes32 hashlock, uint256 timelock)
        public
        pure
        returns (bytes32)
    {
        return keccak256(abi.encodePacked(sender, receiver, amount, hashlock, timelock));
    }

    /// Read a slot (fields + lifecycle state) — the G6 gateway funding verifier reads this to
    /// check hashlock/amount/timelock/recipient on-chain before its side ever funds or reveals.
    function getSwap(bytes32 swapId)
        external
        view
        returns (address sender, address receiver, uint256 amount, bytes32 hashlock, uint256 timelock, State state)
    {
        Swap storage s = swaps[swapId];
        return (s.sender, s.receiver, s.amount, s.hashlock, s.timelock, s.state);
    }

    /// ERC-2771 introspection.
    function isTrustedForwarder(address forwarder) external view returns (bool) {
        return forwarder != address(0) && forwarder == trustedForwarder;
    }

    function _newSwap(address funder, address receiver, uint256 amount, bytes32 hashlock, uint256 timelock)
        internal
        returns (bytes32 swapId)
    {
        if (receiver == address(0)) revert ZeroReceiver();
        if (amount == 0) revert ZeroAmount();
        if (timelock <= block.timestamp) revert TimelockNotFuture(timelock, block.timestamp);
        swapId = swapIdFor(funder, receiver, amount, hashlock, timelock);
        if (swaps[swapId].state != State.None) revert SwapAlreadyExists(swapId);
        swaps[swapId] = Swap(funder, receiver, amount, hashlock, timelock, State.Live);
        emit NewSwap(swapId, funder, receiver, amount, hashlock, timelock);
        // Effects before interaction (checks-effects-interactions); the pull is last.
        if (!token.transferFrom(funder, address(this), amount)) revert TransferFailed();
    }

    /// ERC-2771 `_msgSender()`: when the immutable trusted forwarder calls, the real sender is
    /// the last 20 bytes of calldata (appended by the forwarder); otherwise `msg.sender`.
    function _msgSender() internal view returns (address sender) {
        if (msg.sender == trustedForwarder && trustedForwarder != address(0) && msg.data.length >= 20) {
            assembly {
                sender := shr(96, calldataload(sub(calldatasize(), 20)))
            }
        } else {
            sender = msg.sender;
        }
    }
}
