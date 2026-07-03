// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.24;

/// The slice of Foundry's cheatcode surface these tests actually use — hand-vendored instead of
/// pulling the forge-std submodule (the repo vendors everything; see the note in foundry.toml
/// and `crates/nimmesh-core/src/evm_abi.rs`). The address is the canonical cheatcode account:
/// `address(uint160(uint256(keccak256("hevm cheat code"))))`.
interface Vm {
    function warp(uint256 newTimestamp) external;
    function prank(address msgSender) external;
    function expectRevert(bytes calldata revertData) external;
    function expectEmit(bool checkTopic1, bool checkTopic2, bool checkTopic3, bool checkData) external;
    function sign(uint256 privateKey, bytes32 digest) external pure returns (uint8 v, bytes32 r, bytes32 s);
    function addr(uint256 privateKey) external pure returns (address keyAddr);
}

library Cheats {
    Vm internal constant VM = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
}
