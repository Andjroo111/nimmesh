// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.24;

import {NimmeshHtlc, IERC20} from "../src/NimmeshHtlc.sol";
import {NimmeshForwarder} from "../src/NimmeshForwarder.sol";
import {MockUsdc} from "./MockUsdc.sol";
import {Vm} from "./Cheats.sol";

/// The G7 (#78) suite: gasless funding through the forwarder into the REAL HTLC + the
/// forwarder's own guarantees (replay, expiry, forgery, tamper, attribution).
contract NimmeshForwarderTest {
    Vm internal constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    uint256 internal constant USER_KEY = 0x0A11CE; // holds USDC, holds NO POL, never sends a tx
    address internal USER;
    address internal constant RELAYER = address(0x4E1A); // pays all the gas
    address internal constant BOB = address(0xB0B);

    uint256 internal constant AMOUNT = 1_000_000; // 1 USDC
    bytes32 internal constant SECRET = bytes32(uint256(0x757));
    bytes32 internal HASHLOCK;
    uint256 internal constant T0 = 1_000_000;
    uint256 internal constant TIMELOCK = T0 + 3_600;
    uint256 internal constant FWD_GAS = 400_000;

    MockUsdc internal usdc;
    NimmeshForwarder internal fwd;
    NimmeshHtlc internal htlc;

    function setUp() public {
        vm.warp(T0);
        usdc = new MockUsdc();
        fwd = new NimmeshForwarder();
        htlc = new NimmeshHtlc(IERC20(address(usdc)), address(fwd));
        USER = vm.addr(USER_KEY);
        usdc.mint(USER, 10_000_000);
        HASHLOCK = sha256(abi.encodePacked(SECRET));
    }

    // ── helpers ────────────────────────────────────────────────────────────────────────────

    function _permitSig(uint256 value, uint256 deadline) internal view returns (uint8 v, bytes32 r, bytes32 s) {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                usdc.DOMAIN_SEPARATOR(),
                keccak256(abi.encode(usdc.PERMIT_TYPEHASH(), USER, address(htlc), value, usdc.nonces(USER), deadline))
            )
        );
        return vm.sign(USER_KEY, digest);
    }

    /// The user's whole intent in one signed blob: newSwapWithPermit calldata for the HTLC.
    function _fundingCalldata(uint256 deadline) internal view returns (bytes memory) {
        (uint8 pv, bytes32 pr, bytes32 ps) = _permitSig(AMOUNT, deadline);
        return
            abi.encodeWithSelector(
                htlc.newSwapWithPermit.selector, BOB, AMOUNT, HASHLOCK, TIMELOCK, deadline, pv, pr, ps
            );
    }

    function _request(bytes memory data, uint256 nonce, uint256 deadline)
        internal
        view
        returns (NimmeshForwarder.ForwardRequest memory)
    {
        return NimmeshForwarder.ForwardRequest({
            from: USER, to: address(htlc), value: 0, gas: FWD_GAS, nonce: nonce, deadline: deadline, data: data
        });
    }

    function _requestSig(NimmeshForwarder.ForwardRequest memory req)
        internal
        view
        returns (uint8 v, bytes32 r, bytes32 s)
    {
        return vm.sign(USER_KEY, this.digestOf(req));
    }

    /// External hop so the struct crosses as calldata (requestDigest takes calldata).
    function digestOf(NimmeshForwarder.ForwardRequest calldata req) external view returns (bytes32) {
        return fwd.requestDigest(req);
    }

    // ── the headline: gasless funding, funder attributed to the signer ────────────────────

    function test_GaslessFundingAttributesTheUserAsFunder() public {
        uint256 deadline = T0 + 600;
        NimmeshForwarder.ForwardRequest memory req = _request(_fundingCalldata(deadline), 0, deadline);
        (uint8 v, bytes32 r, bytes32 s) = _requestSig(req);

        require(fwd.verify(req, v, r, s), "verify");
        vm.prank(RELAYER);
        (bool ok,) = fwd.execute(req, v, r, s);
        require(ok, "relayed call failed");

        bytes32 id = htlc.swapIdFor(USER, BOB, AMOUNT, HASHLOCK, TIMELOCK);
        (address sender, address receiver, uint256 amount,,, NimmeshHtlc.State state) = htlc.getSwap(id);
        require(sender == USER, "funder must be the SIGNER, not the relayer/forwarder");
        require(receiver == BOB && amount == AMOUNT, "terms");
        require(state == NimmeshHtlc.State.Live, "escrow not live");
        require(usdc.balanceOf(address(htlc)) == AMOUNT, "escrow did not pull the USER's USDC");
        require(usdc.balanceOf(USER) == 9_000_000, "user balance");
        require(fwd.nonces(USER) == 1, "nonce burned");
    }

    function test_TheGaslessEscrowSettlesViaCallerOpenWithdraw() public {
        uint256 deadline = T0 + 600;
        NimmeshForwarder.ForwardRequest memory req = _request(_fundingCalldata(deadline), 0, deadline);
        (uint8 v, bytes32 r, bytes32 s) = _requestSig(req);
        vm.prank(RELAYER);
        fwd.execute(req, v, r, s);

        // The user hands S to the relayer (ADR-0006's sharp edge: that IS a reveal); the relayer
        // lands the caller-open claim directly — no forwarder machinery on the claim path.
        bytes32 id = htlc.swapIdFor(USER, BOB, AMOUNT, HASHLOCK, TIMELOCK);
        vm.prank(RELAYER);
        htlc.withdraw(id, SECRET);
        require(usdc.balanceOf(BOB) == AMOUNT, "payout is fixed to the stored receiver");
    }

    // ── the forwarder's own guarantees ─────────────────────────────────────────────────────

    function test_ReplayIsRejected() public {
        uint256 deadline = T0 + 600;
        NimmeshForwarder.ForwardRequest memory req = _request(_fundingCalldata(deadline), 0, deadline);
        (uint8 v, bytes32 r, bytes32 s) = _requestSig(req);
        vm.prank(RELAYER);
        fwd.execute(req, v, r, s);
        vm.prank(RELAYER);
        vm.expectRevert(abi.encodeWithSelector(NimmeshForwarder.WrongNonce.selector, 0, 1));
        fwd.execute(req, v, r, s);
    }

    function test_ExpiredRequestIsRejected() public {
        uint256 deadline = T0 + 600;
        NimmeshForwarder.ForwardRequest memory req = _request(_fundingCalldata(deadline), 0, deadline);
        (uint8 v, bytes32 r, bytes32 s) = _requestSig(req);
        vm.warp(deadline + 1);
        vm.prank(RELAYER);
        vm.expectRevert(abi.encodeWithSelector(NimmeshForwarder.ExpiredRequest.selector, deadline, deadline + 1));
        fwd.execute(req, v, r, s);
    }

    function test_TamperedCalldataIsRejected() public {
        uint256 deadline = T0 + 600;
        NimmeshForwarder.ForwardRequest memory req = _request(_fundingCalldata(deadline), 0, deadline);
        (uint8 v, bytes32 r, bytes32 s) = _requestSig(req);
        // The relayer swaps in different calldata (a bigger amount) under the same signature.
        req.data = abi.encodeWithSelector(
            htlc.newSwapWithPermit.selector,
            BOB,
            AMOUNT * 2,
            HASHLOCK,
            TIMELOCK,
            deadline,
            uint8(27),
            bytes32(0),
            bytes32(0)
        );
        vm.prank(RELAYER);
        vm.expectRevert(abi.encodeWithSelector(NimmeshForwarder.BadSignature.selector));
        fwd.execute(req, v, r, s);
    }

    function test_AForgedFromIsRejected() public {
        uint256 deadline = T0 + 600;
        NimmeshForwarder.ForwardRequest memory req = _request(_fundingCalldata(deadline), 0, deadline);
        (uint8 v, bytes32 r, bytes32 s) = _requestSig(req);
        // The relayer claims the request came from itself — recovery does not match `from`.
        req.from = RELAYER;
        vm.prank(RELAYER);
        vm.expectRevert(abi.encodeWithSelector(NimmeshForwarder.BadSignature.selector));
        fwd.execute(req, v, r, s);
    }

    function test_ATargetFailureIsReportedNotBubbled() public {
        // A request whose inner call fails (no USDC behind the permit → transferFrom reverts):
        // execute itself SUCCEEDS, returns success=false, and the nonce is still burned.
        uint256 deadline = T0 + 600;
        vm.prank(USER);
        usdc.transfer(BOB, 10_000_000); // drain the user first
        NimmeshForwarder.ForwardRequest memory req = _request(_fundingCalldata(deadline), 0, deadline);
        (uint8 v, bytes32 r, bytes32 s) = _requestSig(req);
        vm.prank(RELAYER);
        (bool ok,) = fwd.execute(req, v, r, s);
        require(!ok, "target failure must be reported");
        require(fwd.nonces(USER) == 1, "nonce burned even on target failure");
        bytes32 id = htlc.swapIdFor(USER, BOB, AMOUNT, HASHLOCK, TIMELOCK);
        (,,,,, NimmeshHtlc.State state) = htlc.getSwap(id);
        require(state == NimmeshHtlc.State.None, "no escrow was created");
    }

    function test_VerifyIsAnHonestPreview() public {
        uint256 deadline = T0 + 600;
        NimmeshForwarder.ForwardRequest memory req = _request(_fundingCalldata(deadline), 0, deadline);
        (uint8 v, bytes32 r, bytes32 s) = _requestSig(req);
        require(fwd.verify(req, v, r, s), "fresh request verifies");
        req.nonce = 1;
        require(!fwd.verify(req, v, r, s), "wrong nonce fails");
        req.nonce = 0;
        req.from = RELAYER;
        require(!fwd.verify(req, v, r, s), "wrong from fails");
        req.from = USER;
        vm.warp(deadline + 1);
        require(!fwd.verify(req, v, r, s), "expired fails");
    }
}
