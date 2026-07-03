// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.24;

import {NimmeshHtlc, IERC20} from "../src/NimmeshHtlc.sol";
import {MockUsdc} from "./MockUsdc.sol";
import {Vm, Cheats} from "./Cheats.sol";

/// The G5 (#76) unit suite. The one test that matters most for the cross-chain story is
/// `test_SwapIdMatchesTheRustVector` — the byte-for-byte anchor against the Rust model's
/// `usdc_swap_id` (`crates/nimmesh-core/src/swap_usdc_leg.rs` asserts the SAME constant).
contract NimmeshHtlcTest {
    Vm internal constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    // Redeclared for `vm.expectEmit` reference emissions.
    event NewSwap(
        bytes32 indexed swapId,
        address indexed sender,
        address indexed receiver,
        uint256 amount,
        bytes32 hashlock,
        uint256 timelock
    );
    event Withdrawn(bytes32 indexed swapId, bytes32 secret);
    event Refunded(bytes32 indexed swapId);

    uint256 internal constant ALICE_KEY = 0xA11CE; // funder (signs permits)
    address internal ALICE;
    address internal constant BOB = address(0xB0B); // recipient
    address internal constant CARL = address(0xCA71); // uninvolved third party
    address internal constant FORWARDER = address(0xF0F0); // ERC-2771 trusted forwarder

    uint256 internal constant AMOUNT = 25_000_000; // 25 USDC in micro-USDC
    bytes32 internal constant SECRET = bytes32(uint256(0x5E5E5E));
    bytes32 internal HASHLOCK;
    uint256 internal constant T0 = 1_000_000; // test-epoch "now"
    uint256 internal constant TIMELOCK = T0 + 3_600;

    MockUsdc internal usdc;
    NimmeshHtlc internal htlc;

    function setUp() public {
        vm.warp(T0);
        usdc = new MockUsdc();
        htlc = new NimmeshHtlc(IERC20(address(usdc)), FORWARDER);
        ALICE = vm.addr(ALICE_KEY);
        usdc.mint(ALICE, 1_000_000_000); // 1,000 USDC
        HASHLOCK = sha256(abi.encodePacked(SECRET));
    }

    // ── helpers ────────────────────────────────────────────────────────────────────────────

    function _fund() internal returns (bytes32 id) {
        vm.prank(ALICE);
        usdc.approve(address(htlc), AMOUNT);
        vm.prank(ALICE);
        id = htlc.newSwap(BOB, AMOUNT, HASHLOCK, TIMELOCK);
    }

    function _permitSig(uint256 value, uint256 deadline) internal view returns (uint8 v, bytes32 r, bytes32 s) {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                usdc.DOMAIN_SEPARATOR(),
                keccak256(abi.encode(usdc.PERMIT_TYPEHASH(), ALICE, address(htlc), value, usdc.nonces(ALICE), deadline))
            )
        );
        return vm.sign(ALICE_KEY, digest);
    }

    function _assertState(bytes32 id, NimmeshHtlc.State want) internal view {
        (,,,,, NimmeshHtlc.State got) = htlc.getSwap(id);
        require(got == want, "unexpected swap state");
    }

    // ── swap-id derivation ─────────────────────────────────────────────────────────────────

    /// THE byte-match anchor (G5's "done when"): the same inputs must produce this exact id in
    /// the Rust model (`usdc_swap_id`, see `swap_usdc_leg.rs::usdc_swap_id_matches_the_contract_vector`)
    /// and in this contract. Pre-image = 20+20+32+32+32 = 136 packed bytes.
    function test_SwapIdMatchesTheRustVector() public view {
        bytes32 id = htlc.swapIdFor(
            address(0x1111111111111111111111111111111111111111),
            address(0x2222222222222222222222222222222222222222),
            25_000_000,
            bytes32(0xc7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7),
            5_000
        );
        require(
            id == bytes32(0x81137ded176c774f8dbc1b69583fa8232031e4a2810ba97231a69becf44131e0),
            "swap id diverged from the Rust vector"
        );
    }

    function test_NewSwapEscrowsAndDerivesIdOnChain() public {
        uint256 aliceBefore = usdc.balanceOf(ALICE);
        vm.prank(ALICE);
        usdc.approve(address(htlc), AMOUNT);

        bytes32 expectedId = htlc.swapIdFor(ALICE, BOB, AMOUNT, HASHLOCK, TIMELOCK);
        vm.prank(ALICE);
        vm.expectEmit(true, true, true, true);
        emit NewSwap(expectedId, ALICE, BOB, AMOUNT, HASHLOCK, TIMELOCK);
        bytes32 id = htlc.newSwap(BOB, AMOUNT, HASHLOCK, TIMELOCK);

        require(id == expectedId, "returned id != derived id");
        require(usdc.balanceOf(address(htlc)) == AMOUNT, "escrow did not receive the USDC");
        require(usdc.balanceOf(ALICE) == aliceBefore - AMOUNT, "funder balance unchanged");
        (address sender, address receiver, uint256 amount, bytes32 hashlock, uint256 timelock,) = htlc.getSwap(id);
        require(sender == ALICE && receiver == BOB, "parties not stored");
        require(amount == AMOUNT && hashlock == HASHLOCK && timelock == TIMELOCK, "terms not stored");
        _assertState(id, NimmeshHtlc.State.Live);
    }

    /// Single-occupancy: identical parameters map to the same slot; the second `newSwap` is
    /// rejected while the first is live (and forever after — resolved slots are never reused).
    function test_NewSwapRejectsADuplicateSlot() public {
        bytes32 id = _fund();
        vm.prank(ALICE);
        usdc.approve(address(htlc), AMOUNT);
        vm.prank(ALICE);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.SwapAlreadyExists.selector, id));
        htlc.newSwap(BOB, AMOUNT, HASHLOCK, TIMELOCK);
    }

    function test_NewSwapValidatesItsArguments() public {
        vm.prank(ALICE);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.ZeroReceiver.selector));
        htlc.newSwap(address(0), AMOUNT, HASHLOCK, TIMELOCK);

        vm.prank(ALICE);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.ZeroAmount.selector));
        htlc.newSwap(BOB, 0, HASHLOCK, TIMELOCK);

        // A timelock at or before "now" is expired at birth — the Rust engine would refuse the
        // swap anyway (ladder), but the contract must not accept it either.
        vm.prank(ALICE);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.TimelockNotFuture.selector, T0, T0));
        htlc.newSwap(BOB, AMOUNT, HASHLOCK, T0);
    }

    function test_NewSwapWithoutAllowanceReverts() public {
        vm.prank(ALICE);
        vm.expectRevert("MockUsdc: allowance");
        htlc.newSwap(BOB, AMOUNT, HASHLOCK, TIMELOCK);
    }

    // ── withdraw ───────────────────────────────────────────────────────────────────────────

    /// Caller-open with a fixed payout: an uninvolved third party submits the claim and the
    /// USDC still goes to BOB — this is ADR-0006's self-funded/anyone-can-submit fallback.
    function test_WithdrawPaysTheReceiverAndRevealsTheSecret() public {
        bytes32 id = _fund();
        vm.prank(CARL);
        vm.expectEmit(true, true, true, true);
        emit Withdrawn(id, SECRET);
        htlc.withdraw(id, SECRET);
        require(usdc.balanceOf(BOB) == AMOUNT, "receiver not paid");
        require(usdc.balanceOf(CARL) == 0, "submitter must get nothing");
        _assertState(id, NimmeshHtlc.State.Claimed);
    }

    function test_WithdrawRejectsAWrongSecret() public {
        bytes32 id = _fund();
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.WrongSecret.selector));
        htlc.withdraw(id, bytes32(uint256(0xBAD)));
        _assertState(id, NimmeshHtlc.State.Live); // unchanged — no theft
    }

    /// THE cross-chain choice, mirrored from the Rust suite: a lock built with keccak256(S)
    /// (a vanilla EVM HTLC) must NOT be claimable by S — the contract hashes with the SHA-256
    /// precompile, which is what makes H shared across the NIM/BTC/USDC legs.
    function test_AKeccakLockIsNotClaimableBySha256() public {
        bytes32 keccakLock = keccak256(abi.encodePacked(SECRET));
        require(keccakLock != HASHLOCK, "hashes must differ");
        vm.prank(ALICE);
        usdc.approve(address(htlc), AMOUNT);
        vm.prank(ALICE);
        bytes32 id = htlc.newSwap(BOB, AMOUNT, keccakLock, TIMELOCK);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.WrongSecret.selector));
        htlc.withdraw(id, SECRET);
        _assertState(id, NimmeshHtlc.State.Live); // safe — refundable by ALICE after the timeout
    }

    /// Boundary semantics match the Rust model's CODE (`swap_usdc_leg.rs`): claim allowed AT
    /// the timelock second, refused one past it. No overlap and no gap with refund.
    function test_WithdrawBoundaryIsInclusive() public {
        bytes32 id = _fund();
        vm.warp(TIMELOCK); // exactly the boundary — still the claimer's second
        htlc.withdraw(id, SECRET);
        _assertState(id, NimmeshHtlc.State.Claimed);
    }

    function test_WithdrawPastTheTimelockReverts() public {
        bytes32 id = _fund();
        vm.warp(TIMELOCK + 1);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.ClaimWindowClosed.selector, TIMELOCK, TIMELOCK + 1));
        htlc.withdraw(id, SECRET);
    }

    function test_WithdrawUnknownSwapReverts() public {
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.UnknownSwap.selector, bytes32(0)));
        htlc.withdraw(bytes32(0), SECRET);
    }

    // ── refund ─────────────────────────────────────────────────────────────────────────────

    function test_RefundOnlyStrictlyAfterTheTimelock() public {
        bytes32 id = _fund();
        // At the boundary the claimer still owns the second.
        vm.warp(TIMELOCK);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.TimeoutNotReached.selector, TIMELOCK, TIMELOCK));
        htlc.refund(id);
        // One past it, the funder gets the escrow back — submitted by a third party, paid to ALICE.
        uint256 aliceBefore = usdc.balanceOf(ALICE);
        vm.warp(TIMELOCK + 1);
        vm.prank(CARL);
        vm.expectEmit(true, true, true, true);
        emit Refunded(id);
        htlc.refund(id);
        require(usdc.balanceOf(ALICE) == aliceBefore + AMOUNT, "funder not refunded");
        _assertState(id, NimmeshHtlc.State.Refunded);
    }

    function test_NoDoubleResolve() public {
        // claimed → neither withdraw nor refund again
        bytes32 id = _fund();
        htlc.withdraw(id, SECRET);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.NotLive.selector, id));
        htlc.withdraw(id, SECRET);
        vm.warp(TIMELOCK + 1);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.NotLive.selector, id));
        htlc.refund(id);

        // refunded → no withdraw, no second refund (fresh swap, different hashlock → new slot)
        bytes32 lock2 = sha256(abi.encodePacked(bytes32(uint256(2))));
        vm.prank(ALICE);
        usdc.approve(address(htlc), AMOUNT);
        vm.prank(ALICE);
        bytes32 id2 = htlc.newSwap(BOB, AMOUNT, lock2, TIMELOCK + 7_200);
        vm.warp(TIMELOCK + 7_201);
        htlc.refund(id2);
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.NotLive.selector, id2));
        htlc.withdraw(id2, bytes32(uint256(2)));
        vm.expectRevert(abi.encodeWithSelector(NimmeshHtlc.NotLive.selector, id2));
        htlc.refund(id2);
    }

    // ── single-transaction funding (EIP-2612 permit) ───────────────────────────────────────

    function test_PermitFundsInASingleTransaction() public {
        uint256 deadline = T0 + 600;
        (uint8 v, bytes32 r, bytes32 s) = _permitSig(AMOUNT, deadline);
        // No prior approve anywhere — the permit inside newSwapWithPermit is the allowance.
        vm.prank(ALICE);
        bytes32 id = htlc.newSwapWithPermit(BOB, AMOUNT, HASHLOCK, TIMELOCK, deadline, v, r, s);
        require(usdc.balanceOf(address(htlc)) == AMOUNT, "escrow not funded");
        _assertState(id, NimmeshHtlc.State.Live);
    }

    /// A front-runner who watches the mempool can extract the permit signature and spend it
    /// first. The try/catch + allowance fallback makes that a no-op instead of a DoS.
    function test_PermitFrontRunIsTolerated() public {
        uint256 deadline = T0 + 600;
        (uint8 v, bytes32 r, bytes32 s) = _permitSig(AMOUNT, deadline);
        usdc.permit(ALICE, address(htlc), AMOUNT, deadline, v, r, s); // the front-run
        vm.prank(ALICE);
        bytes32 id = htlc.newSwapWithPermit(BOB, AMOUNT, HASHLOCK, TIMELOCK, deadline, v, r, s);
        require(usdc.balanceOf(address(htlc)) == AMOUNT, "escrow not funded after front-run");
        _assertState(id, NimmeshHtlc.State.Live);
    }

    // ── ERC-2771 (ADR-0006: relayer-sponsored funding) ─────────────────────────────────────

    /// A meta-tx `newSwap` through the trusted forwarder attributes the funder to the appended
    /// address, not to the forwarder — the swap id and the escrow pull both bind to ALICE.
    function test_ForwarderCallAttributesTheFunder() public {
        vm.prank(ALICE);
        usdc.approve(address(htlc), AMOUNT);
        bytes memory data =
            abi.encodePacked(abi.encodeWithSelector(htlc.newSwap.selector, BOB, AMOUNT, HASHLOCK, TIMELOCK), ALICE);
        vm.prank(FORWARDER);
        (bool ok, bytes memory ret) = address(htlc).call(data);
        require(ok, "forwarded newSwap failed");
        bytes32 id = abi.decode(ret, (bytes32));
        (address sender,,,,,) = htlc.getSwap(id);
        require(sender == ALICE, "funder must be the appended sender, not the forwarder");
        require(id == htlc.swapIdFor(ALICE, BOB, AMOUNT, HASHLOCK, TIMELOCK), "id must bind ALICE");
        require(htlc.isTrustedForwarder(FORWARDER), "forwarder introspection");
    }

    /// The same appended-suffix calldata from anyone else is NOT sender-forwarding: the funder
    /// stays msg.sender (here CARL, whose allowance is zero, so the pull reverts).
    function test_NonForwarderSuffixIsIgnored() public {
        bytes memory data =
            abi.encodePacked(abi.encodeWithSelector(htlc.newSwap.selector, BOB, AMOUNT, HASHLOCK, TIMELOCK), ALICE);
        vm.prank(CARL);
        (bool ok, bytes memory ret) = address(htlc).call(data);
        require(!ok, "non-forwarder suffix must not impersonate ALICE");
        // The revert is the mock's allowance check for CARL — proving the funder was CARL.
        require(
            keccak256(ret) == keccak256(abi.encodeWithSignature("Error(string)", "MockUsdc: allowance")), "wrong revert"
        );
    }
}
