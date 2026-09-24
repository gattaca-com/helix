// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.20;

import {Test, console2} from "forge-std/Test.sol";
import {HuffDeployer} from "foundry-huff/HuffDeployer.sol";

error AlreadyEntered();
error TimestampMismatch();
error TransferFailed(address recipient);

contract ReentrantRecipient {
    address public disperser;
    uint256 public grabAmount;
    bool public armed;
    bytes public nestedRevertData;

    constructor(address _disperser) {
        disperser = _disperser;
    }

    function arm(uint256 _grabAmount) external {
        armed = true;
        grabAmount = _grabAmount;
    }

    receive() external payable {
        if (armed) {
            armed = false;
            bytes memory data = abi.encodePacked(bytes4(uint32(block.timestamp)), address(this), grabAmount);
            (bool nestedOk, bytes memory retData) = disperser.call(data);
            if (!nestedOk) nestedRevertData = retData;
        }
    }
}

contract RevertingRecipient {
    receive() external payable {
        revert("nope");
    }
}

contract GasGuzzler {
    receive() external payable {
        uint256 i;
        while (true) {
            i++;
        }
    }
}

/// A caller with no payable receive/fallback -- e.g. any contract that never
/// expects to be sent plain ETH.
contract NonPayableCaller {
    function disperse(address disperser, bytes memory data) external returns (bool ok, bytes memory retData) {
        (ok, retData) = disperser.call(data);
    }
}

contract SlotLockedDisperserSecurityTest is Test {
    address disperser;

    function setUp() public {
        disperser = HuffDeployer.config().with_evm_version("osaka").deploy("SlotLockedDisperser");
    }

    /// A malicious recipient reenters mid-CALL (which forwards all remaining gas)
    /// and asks the disperser to pay it again, out of the same live balance.
    function test_Reentrancy_StealsRefundMeantForOriginalCaller() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 10 ether);

        ReentrantRecipient attacker = new ReentrantRecipient(disperser);
        attacker.arm(5 ether);

        address payable legitCaller = payable(address(0xCAFE));
        uint256 attackerAllocation = 1 ether;

        bytes memory data =
            abi.encodePacked(bytes4(uint32(block.timestamp)), address(attacker), attackerAllocation);

        vm.prank(legitCaller);
        (bool ok,) = disperser.call(data);

        assertTrue(ok, "outer call should still succeed for a non-reentrant recipient's own payment");
        assertEq(address(attacker).balance, attackerAllocation, "reentrancy guard should cap attacker at their own allocation");
        assertEq(legitCaller.balance, 10 ether - attackerAllocation, "legit caller should still get the full leftover refund");
        assertEq(disperser.balance, 0);
        assertEq(
            bytes4(attacker.nestedRevertData()),
            AlreadyEntered.selector,
            "the blocked nested call should decode as AlreadyEntered()"
        );
    }

    /// OPEN DESIGN QUESTION, not fixed here: nothing in the contract checks msg.sender
    /// or restricts who the recipients/amounts can be. Anyone who can land a tx in the
    /// matching-timestamp block can call this directly and route the entire live balance
    /// to themselves. This is only safe if the contract is guaranteed to hold zero balance
    /// except atomically within a single funding+dispersal bundle. If it can ever hold a
    /// resting balance across blocks, this drains it.
    function test_AnyoneCanDrainRestingBalance_NoAccessControl() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 10 ether);

        address payable randomStranger = payable(address(0xD00D));
        bytes memory data = abi.encodePacked(bytes4(uint32(block.timestamp)), randomStranger, uint256(10 ether));

        vm.prank(randomStranger);
        (bool ok,) = disperser.call(data);

        assertTrue(ok, "documents current behavior: unauthenticated drain succeeds");
        assertEq(randomStranger.balance, 10 ether);
    }

    /// Requesting more than the live balance for one recipient must not partially
    /// pay earlier recipients and then silently stop — the whole tx should revert.
    function test_InsufficientBalanceMidLoop_RevertsEverything() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 5 ether);

        address r1 = address(0xBEEF1);
        address r2 = address(0xBEEF2);

        bytes memory data = abi.encodePacked(
            bytes4(uint32(block.timestamp)),
            r1,
            uint256(3 ether),
            r2,
            uint256(3 ether) // 3 + 3 > 5 ether available
        );

        (bool ok, bytes memory retData) = disperser.call(data);

        assertFalse(ok, "should revert rather than partially disperse");
        assertEq(r1.balance, 0, "first recipient must not keep a partial payment from a reverted tx");
        assertEq(disperser.balance, 5 ether, "balance must be untouched after revert");
        assertEq(bytes4(retData), TransferFailed.selector, "revert reason should decode as TransferFailed(address)");
        (address failedRecipient) = abi.decode(_stripSelector(retData), (address));
        assertEq(failedRecipient, r2, "revert data should name r2 as the recipient whose transfer failed");
    }

    function _stripSelector(bytes memory data) internal pure returns (bytes memory) {
        bytes memory out = new bytes(data.length - 4);
        for (uint256 i = 0; i < out.length; i++) {
            out[i] = data[i + 4];
        }
        return out;
    }

    /// One recipient reverting on receipt must fail the whole batch atomically,
    /// not silently skip that recipient and continue.
    function test_RevertingRecipient_FailsWholeBatchAtomically() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 5 ether);

        RevertingRecipient bad = new RevertingRecipient();
        address good = address(0xBEEF);

        bytes memory data = abi.encodePacked(
            bytes4(uint32(block.timestamp)), good, uint256(1 ether), address(bad), uint256(1 ether)
        );

        (bool ok, bytes memory retData) = disperser.call(data);

        assertFalse(ok);
        assertEq(good.balance, 0, "the legitimate recipient must not be paid out of an atomically-reverted tx");
        assertEq(disperser.balance, 5 ether);
        assertEq(bytes4(retData), TransferFailed.selector);
        (address failedRecipient) = abi.decode(_stripSelector(retData), (address));
        assertEq(failedRecipient, address(bad), "revert data should name the reverting recipient");
    }

    /// Known trade-off of atomic push-based dispersal, not fixed here: CALL forwards
    /// all remaining gas with no cap, so a single adversarial or merely gas-hungry
    /// recipient can burn the entire tx's gas and force revert_fail, blocking payment
    /// to every other (well-behaved) recipient in the same batch.
    function test_OneGasGuzzlingRecipient_BlocksPaymentToEveryoneElse() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 5 ether);

        GasGuzzler guzzler = new GasGuzzler();
        address goodRecipient = address(0xBEEF);

        bytes memory data = abi.encodePacked(
            bytes4(uint32(block.timestamp)),
            goodRecipient,
            uint256(1 ether),
            address(guzzler),
            uint256(1 ether)
        );

        (bool ok, bytes memory retData) = disperser.call{gas: 5_000_000}(data);

        assertFalse(ok, "documents current behavior: the guzzler forces the whole batch to revert");
        assertEq(goodRecipient.balance, 0, "well-behaved recipient gets nothing because of one bad actor");

        // The guzzler's own CALL frame burns to zero, but EIP-150 only forwards 63/64ths
        // of the available gas, so the outer contract retains a 1/64th sliver -- easily
        // enough to still run `transfer_failed` and name the culprit before reverting.
        assertEq(bytes4(retData), TransferFailed.selector, "the diagnostic should survive a gas-griefing recipient too");
        (address failedRecipient) = abi.decode(_stripSelector(retData), (address));
        assertEq(failedRecipient, address(guzzler), "revert data should name the gas guzzler as the culprit");
    }

    /// Sanity check: calldata whose trailing entry is short (not an exact multiple of
    /// 52 bytes past the 4-byte timestamp) never satisfies `offset == calldatasize`,
    /// so the loop keeps reading past the end (calldataload zero-pads) and advancing
    /// until it runs out of gas, rather than reverting cleanly up front.
    function test_MisalignedCalldata_BurnsGasInsteadOfCleanRevert() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 1 ether);

        bytes memory data = abi.encodePacked(
            bytes4(uint32(block.timestamp)),
            address(0xBEEF),
            uint256(1 ether),
            uint8(0xFF) // one stray trailing byte
        );

        (bool ok,) = disperser.call{gas: 2_000_000}(data);

        assertFalse(ok, "malformed calldata should not succeed, but note it burns gas rather than failing fast");
    }

    /// Regression test for a bug Halmos found that no hand-picked concrete test here
    /// ever hit: the refund-to-caller CALL's success was discarded (`pop`), so a caller
    /// that can't accept plain ETH used to get a silent success with the leftover
    /// balance permanently stranded in the contract instead of a revert.
    function test_NonPayableCaller_RefundFailureRevertsInsteadOfStrandingFunds() public {
        vm.warp(1_000_000);
        vm.deal(disperser, 5 ether);

        NonPayableCaller caller = new NonPayableCaller();
        bytes memory data = abi.encodePacked(bytes4(uint32(block.timestamp)));

        (bool ok, bytes memory retData) = caller.disperse(disperser, data);

        assertFalse(ok, "a refund that can't be delivered must revert the whole call");
        assertEq(disperser.balance, 5 ether, "balance must stay put, not get stranded");
        assertEq(bytes4(retData), TransferFailed.selector);
        (address failedRecipient) = abi.decode(_stripSelector(retData), (address));
        assertEq(failedRecipient, address(caller), "revert data should name the caller whose refund failed");
    }
}
