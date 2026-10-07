// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.20;

import {Test, console2} from "forge-std/Test.sol";
import {HuffDeployer} from "foundry-huff/HuffDeployer.sol";

error AlreadyEntered();
error TimestampMismatch();
error TransferFailed(address recipient);

contract SlotLockedDisperserTest is Test {
    address disperser;

    function setUp() public {
        disperser = HuffDeployer.config().with_evm_version("osaka").deploy("SlotLockedDisperser");
    }

    function _tsCalldata(uint256 ts) internal pure returns (bytes memory) {
        return abi.encodePacked(bytes4(uint32(ts)));
    }

    function test_RevertsOnTimestampMismatch() public {
        vm.warp(1_000_000);
        bytes memory data = _tsCalldata(block.timestamp - 1);
        (bool ok, bytes memory retData) = disperser.call(data);
        assertFalse(ok, "should revert on mismatched timestamp");
        assertEq(bytes4(retData), TimestampMismatch.selector, "revert reason should decode as TimestampMismatch()");
    }

    function test_NoRecipients_RefundsCallerFullBalance() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 5 ether);

        address payable caller = payable(address(0xCAFE));
        bytes memory data = _tsCalldata(block.timestamp);

        vm.prank(caller);
        (bool ok,) = disperser.call(data);

        assertTrue(ok, "call should succeed");
        assertEq(disperser.balance, 0, "contract should be drained");
        assertEq(caller.balance, 5 ether, "caller should receive full refund");
    }

    function test_Dispersal_SingleRecipient() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 10 ether);

        address recipient = address(0xBEEF);
        uint256 amount = 3 ether;
        address payable caller = payable(address(0xCAFE));

        bytes memory data = abi.encodePacked(
            bytes4(uint32(block.timestamp)),
            recipient,
            amount
        );

        vm.prank(caller);
        (bool ok, bytes memory retData) = disperser.call(data);

        console2.log("call success:", ok);
        console2.log("recipient balance:", recipient.balance);
        console2.log("caller balance:", caller.balance);
        console2.log("contract balance:", disperser.balance);
        if (!ok) {
            console2.log("revert data length:", retData.length);
        }

        assertTrue(ok, "dispersal call should succeed");
        assertEq(recipient.balance, amount, "recipient should receive amount");
        assertEq(caller.balance, 10 ether - amount, "caller should receive leftover refund");
        assertEq(disperser.balance, 0, "contract should be fully drained");
    }

    function test_Dispersal_TwoRecipients() public {
        vm.warp(2_000_000);
        vm.deal(disperser, 10 ether);

        address r1 = address(0xBEEF1);
        address r2 = address(0xBEEF2);
        uint256 a1 = 1 ether;
        uint256 a2 = 2 ether;
        address payable caller = payable(address(0xCAFE));

        bytes memory data = abi.encodePacked(
            bytes4(uint32(block.timestamp)),
            r1, a1,
            r2, a2
        );

        vm.prank(caller);
        (bool ok,) = disperser.call(data);

        assertTrue(ok, "dispersal call should succeed");
        assertEq(r1.balance, a1);
        assertEq(r2.balance, a2);
        assertEq(caller.balance, 10 ether - a1 - a2);
    }

    function test_Dispersal_RealisticAddressCanRunOutOfGas() public {
        vm.warp(3_000_000);
        vm.deal(disperser, 10 ether);

        address recipient = 0x1234567890AbcdEF1234567890aBcdef12345678;
        uint256 amount = 1 ether;
        address payable caller = payable(address(0xCAFE));

        bytes memory data = abi.encodePacked(
            bytes4(uint32(block.timestamp)),
            recipient,
            amount
        );

        vm.prank(caller);
        (bool ok, ) = disperser.call{gas: 30_000_000}(data);
        console2.log("realistic-address call success:", ok);
        console2.log("recipient balance:", recipient.balance);
        console2.log("caller balance:", caller.balance);
    }
}
