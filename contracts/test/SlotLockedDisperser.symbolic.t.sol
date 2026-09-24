// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SymTest} from "halmos-cheatcodes/SymTest.sol";
import {HuffDeployer} from "foundry-huff/HuffDeployer.sol";

error AlreadyEntered();
error TimestampMismatch();
error TransferFailed(address recipient);

/// A recipient that always attempts to reenter the disperser on receiving ETH,
/// with fully arbitrary (symbolic) calldata and value. Used to check that
/// conservation holds even against an adversarial recipient, not just an inert
/// address -- see check_AtomicityAndConservation_AdversarialRecipientCannotExploitReentrancy.
contract MaliciousRecipient is SymTest {
    address public disperser;

    constructor(address _disperser) {
        disperser = _disperser;
    }

    receive() external payable {
        bytes memory reentrantData = svm.createBytes(56, "reentrantCalldata");
        uint256 reentrantValue = svm.createUint256("reentrantValue");
        disperser.call{value: reentrantValue}(reentrantData);
    }
}

/// Symbolic (Halmos) properties for SlotLockedDisperser. Each `check_*` function
/// is proven, not sampled: Halmos explores every input satisfying the `vm.assume`
/// constraints via an SMT solver, rather than running a fixed set of concrete cases.
contract SlotLockedDisperserSymbolicTest is SymTest, Test {
    address disperser;

    function setUp() public {
        disperser = HuffDeployer.config().with_evm_version("osaka").deploy("SlotLockedDisperser");
    }

    // vm.toString isn't in Halmos's supported cheatcode set; build decimal strings
    // (0-99) by hand just to get unique symbolic-variable names per index.
    function _idxStr(uint256 i) internal pure returns (string memory) {
        if (i < 10) {
            return string(abi.encodePacked(bytes1(uint8(48 + i))));
        }
        return string(abi.encodePacked(bytes1(uint8(48 + i / 10)), bytes1(uint8(48 + i % 10))));
    }

    function _symbolicAddresses(uint256 n, string memory tag) internal returns (address[] memory out) {
        out = new address[](n);
        for (uint256 i = 0; i < n; i++) {
            out[i] = svm.createAddress(string.concat(tag, "_r", _idxStr(i)));
        }
    }

    function _symbolicAmounts(uint256 n, string memory tag) internal returns (uint256[] memory out) {
        out = new uint256[](n);
        for (uint256 i = 0; i < n; i++) {
            out[i] = svm.createUint256(string.concat(tag, "_a", _idxStr(i)));
        }
    }

    /// Shared core of the atomicity/conservation property, for N recipients built
    /// from arbitrary (symbolic) addresses and amounts: either the call reverts and
    /// every balance is exactly unchanged, or it succeeds, each recipient gets
    /// exactly its own amount, and the caller gets exactly the leftover.
    function _assertAtomicityAndConservation(address[] memory recipients, uint256[] memory amounts, uint256 startBalance)
        internal
    {
        uint256 n = recipients.length;
        for (uint256 i = 0; i < n; i++) {
            vm.assume(recipients[i] != address(this));
            vm.assume(recipients[i] != disperser);
            for (uint256 j = i + 1; j < n; j++) {
                vm.assume(recipients[i] != recipients[j]);
            }
        }
        vm.deal(disperser, startBalance);

        uint256[] memory before = new uint256[](n);
        for (uint256 i = 0; i < n; i++) {
            before[i] = recipients[i].balance;
        }
        uint256 callerBefore = address(this).balance;

        bytes memory data = abi.encodePacked(bytes4(uint32(block.timestamp)));
        for (uint256 i = 0; i < n; i++) {
            data = abi.encodePacked(data, recipients[i], amounts[i]);
        }

        (bool ok,) = disperser.call(data);

        if (ok) {
            uint256 total;
            for (uint256 i = 0; i < n; i++) {
                total += amounts[i];
                assertEq(recipients[i].balance, before[i] + amounts[i], "recipient must receive exactly its amount");
            }
            assertEq(disperser.balance, 0, "contract must be fully drained on success");
            assertEq(
                address(this).balance,
                callerBefore + (startBalance - total),
                "caller must receive the exact leftover"
            );
        } else {
            for (uint256 i = 0; i < n; i++) {
                assertEq(recipients[i].balance, before[i], "failed call must not pay any recipient");
            }
            assertEq(disperser.balance, startBalance, "failed call must leave contract balance untouched");
            assertEq(address(this).balance, callerBefore, "failed call must not refund the caller anything");
        }
    }

    /// Reentrancy lock: once the lock slot (storage slot 0) is set, every call
    /// must be rejected before any calldata is even read, regardless of value
    /// attached or calldata contents -- and a rejected call must move no value
    /// at all.
    function check_AlreadyEnteredBlocksAnyCallRegardlessOfCalldata(uint256 attachedValue) public {
        vm.store(disperser, bytes32(0), bytes32(uint256(1)));
        vm.deal(address(this), attachedValue);

        bytes memory data = svm.createBytes(32, "calldata");

        uint256 balBefore = disperser.balance;
        uint256 callerBalBefore = address(this).balance;

        (bool ok, bytes memory ret) = disperser.call{value: attachedValue}(data);

        assertFalse(ok, "a locked contract must reject any call");
        assertEq(disperser.balance, balBefore, "value must not move onto a rejected call");
        assertEq(address(this).balance, callerBalBefore, "caller must keep its value on a rejected call");
        assertEq(bytes4(ret), AlreadyEntered.selector, "must decode as AlreadyEntered()");
    }

    /// A timestamp that doesn't match block.timestamp must revert before touching
    /// any balance, no matter what recipient/amount/starting-balance accompany it.
    function check_TimestampMismatchNeverMutatesState(address recipient, uint256 amount, uint256 startBalance)
        public
    {
        vm.warp(1_700_000_000);
        uint32 tsArg = uint32(svm.createUint(32, "tsArg"));
        vm.assume(uint256(tsArg) != block.timestamp);
        vm.deal(disperser, startBalance);

        uint256 recipientBefore = recipient.balance;

        bytes memory data = abi.encodePacked(bytes4(tsArg), recipient, amount);
        (bool ok, bytes memory ret) = disperser.call(data);

        assertFalse(ok, "mismatched timestamp must revert");
        assertEq(disperser.balance, startBalance, "balance must be untouched");
        assertEq(recipient.balance, recipientBefore, "recipient must not be paid");
        assertEq(bytes4(ret), TimestampMismatch.selector, "must decode as TimestampMismatch()");
    }

    /// One recipient: either the call reverts and every balance is exactly
    /// unchanged, or it succeeds and value is conserved exactly -- the recipient
    /// gets precisely `amount`, the caller gets precisely the leftover, and the
    /// contract ends at zero. No third outcome is possible.
    function check_AtomicityAndConservation_OneRecipient(
        address recipient,
        uint256 amount,
        uint256 startBalance
    ) public {
        vm.warp(1_700_000_000);
        vm.assume(recipient != address(this));
        vm.assume(recipient != disperser);
        vm.deal(disperser, startBalance);

        uint256 recipientBefore = recipient.balance;
        uint256 callerBefore = address(this).balance;

        bytes memory data = abi.encodePacked(bytes4(uint32(block.timestamp)), recipient, amount);
        (bool ok,) = disperser.call(data);

        if (ok) {
            assertEq(disperser.balance, 0, "contract must be fully drained on success");
            assertEq(recipient.balance, recipientBefore + amount, "recipient must receive exactly amount");
            assertEq(
                address(this).balance,
                callerBefore + (startBalance - amount),
                "caller must receive the exact leftover"
            );
        } else {
            assertEq(disperser.balance, startBalance, "failed call must leave contract balance untouched");
            assertEq(recipient.balance, recipientBefore, "failed call must not pay the recipient anything");
            assertEq(address(this).balance, callerBefore, "failed call must not refund the caller anything");
        }
    }

    /// Same property, two recipients: exercises the loop running more than once.
    function check_AtomicityAndConservation_TwoRecipients(
        address r1,
        uint256 a1,
        address r2,
        uint256 a2,
        uint256 startBalance
    ) public {
        vm.warp(1_700_000_000);
        vm.assume(r1 != address(this) && r2 != address(this));
        vm.assume(r1 != disperser && r2 != disperser);
        vm.assume(r1 != r2);
        vm.deal(disperser, startBalance);

        uint256 r1Before = r1.balance;
        uint256 r2Before = r2.balance;
        uint256 callerBefore = address(this).balance;

        bytes memory data =
            abi.encodePacked(bytes4(uint32(block.timestamp)), r1, a1, r2, a2);
        (bool ok,) = disperser.call(data);

        if (ok) {
            assertEq(disperser.balance, 0, "contract must be fully drained on success");
            assertEq(r1.balance, r1Before + a1);
            assertEq(r2.balance, r2Before + a2);
            assertEq(
                address(this).balance,
                callerBefore + (startBalance - a1 - a2),
                "caller must receive the exact leftover"
            );
        } else {
            assertEq(disperser.balance, startBalance, "failed call must leave contract balance untouched");
            assertEq(r1.balance, r1Before);
            assertEq(r2.balance, r2Before);
            assertEq(address(this).balance, callerBefore);
        }
    }

    /// Same conservation property, generalized to N=3 recipients via the shared helper.
    function check_AtomicityAndConservation_ThreeRecipients(uint256 startBalance) public {
        vm.warp(1_700_000_000);
        address[] memory recipients = _symbolicAddresses(3, "three");
        uint256[] memory amounts = _symbolicAmounts(3, "three");
        _assertAtomicityAndConservation(recipients, amounts, startBalance);
    }

    /// Same property again at N=4: fully arbitrary, mutually-distinct addresses.
    /// This is the practical ceiling for that formulation -- N=5 already takes ~40s
    /// and N=6 exceeds a 60s solver timeout, an exponential wall driven by reasoning
    /// about N mutually-distinct *symbolic* call targets, not by the loop itself (see
    /// the fixed-address versions below, which scale to 25 in ~3s once addresses are
    /// concrete). Real usage never exceeds ~20, so this ceiling isn't a practical gap:
    /// it just means "any address" and "any N up to real usage" are proven separately
    /// rather than in one combined property.
    function check_AtomicityAndConservation_FourRecipients(uint256 startBalance) public {
        vm.warp(1_700_000_000);
        address[] memory recipients = _symbolicAddresses(4, "four");
        uint256[] memory amounts = _symbolicAmounts(4, "four");
        _assertAtomicityAndConservation(recipients, amounts, startBalance);
    }

    // The loop's logic never branches on *which* address it is -- only on whether
    // amounts/balances line up -- so fixing concrete (but still distinct) addresses
    // and leaving only the amounts symbolic exercises the same loop/arithmetic path
    // at real-world N without paying for N mutually-distinct symbolic call targets.
    function _concreteAddresses(uint256 n, uint256 seed) internal pure returns (address[] memory out) {
        out = new address[](n);
        for (uint256 i = 0; i < n; i++) {
            out[i] = address(uint160(0xC0FFEE0000 + seed * 1000 + i));
        }
    }

    /// Matches Owen's stated typical real usage: relay + proposer + up to ~8 builders.
    /// Fixed distinct addresses, arbitrary amounts -- see note above on why.
    function check_AtomicityAndConservation_EightRecipients_FixedAddresses(uint256 startBalance) public {
        vm.warp(1_700_000_000);
        address[] memory recipients = _concreteAddresses(8, 8);
        uint256[] memory amounts = _symbolicAmounts(8, "eightC");
        _assertAtomicityAndConservation(recipients, amounts, startBalance);
    }

    /// Matches the stated extreme upper bound (20), with headroom to 25.
    function check_AtomicityAndConservation_TwentyFiveRecipients_FixedAddresses(uint256 startBalance) public {
        vm.warp(1_700_000_000);
        address[] memory recipients = _concreteAddresses(25, 25);
        uint256[] memory amounts = _symbolicAmounts(25, "twentyFiveC");
        _assertAtomicityAndConservation(recipients, amounts, startBalance);
    }

    /// The property that actually answers "what should not be possible": conservation
    /// must hold even when the recipient is an adversarial contract that always tries
    /// to reenter on receiving payment, with fully arbitrary calldata and value. This
    /// doesn't enumerate attacker strategies -- Halmos can't search over the space of
    /// possible programs -- it empirically checks the one channel an attacker actually
    /// has (calling back into this exact contract), which the reentrancy-guard proof
    /// above already showed is rejected regardless of content. A reverted call has no
    /// state effect as a basic EVM guarantee, so those two facts together mean no
    /// recipient contract, however it's written, can use reentrancy to break this.
    function check_AtomicityAndConservation_AdversarialRecipientCannotExploitReentrancy(
        uint256 amount,
        uint256 startBalance
    ) public {
        vm.warp(1_700_000_000);
        MaliciousRecipient recipient = new MaliciousRecipient(disperser);
        vm.deal(disperser, startBalance);

        uint256 recipientBefore = address(recipient).balance;
        uint256 callerBefore = address(this).balance;

        bytes memory data = abi.encodePacked(bytes4(uint32(block.timestamp)), address(recipient), amount);
        (bool ok,) = disperser.call(data);

        if (ok) {
            assertEq(disperser.balance, 0, "contract must be fully drained on success");
            assertEq(
                address(recipient).balance,
                recipientBefore + amount,
                "recipient must receive exactly amount, even though it tried to reenter"
            );
            assertEq(
                address(this).balance,
                callerBefore + (startBalance - amount),
                "caller must receive the exact leftover"
            );
        } else {
            assertEq(disperser.balance, startBalance, "failed call must leave contract balance untouched");
            assertEq(address(recipient).balance, recipientBefore, "failed call must not pay the recipient anything");
            assertEq(address(this).balance, callerBefore, "failed call must not refund the caller anything");
        }
    }
}
