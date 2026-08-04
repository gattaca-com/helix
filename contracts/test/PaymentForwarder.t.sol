// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {PaymentForwarder} from "../src/PaymentForwarder.sol";

// Minimal cheatcode surface so the repo carries no forge-std submodule.
// Assertions are plain require(); forge fails a test iff it reverts.
interface Vm {
    function warp(uint256) external;
    function prank(address sender, address txOrigin) external;
    function deal(address, uint256) external;
    function assume(bool) external pure;
}

contract Sink {
    receive() external payable {}
}

// SSTORE 0->1 costs ~22.1k, more than the 2300 stipend.
contract GasHeavySink {
    uint256 public s;

    receive() external payable {
        s += 1;
    }
}

contract Revertor {
    receive() external payable {
        revert("no");
    }
}

contract NoReceive {
    function x() external {}
}

contract SenderRecorder {
    address public lastSender;

    receive() external payable {
        lastSender = msg.sender;
    }
}

contract Reenter {
    address payable immutable fwd;

    constructor(address payable f) {
        fwd = f;
    }

    receive() external payable {
        // Reentrant call must revert inside the forwarder without affecting
        // the outer payment.
        (bool ok,) = fwd.call(abi.encodePacked(address(this), uint64(0)));
        require(!ok, "reentrant call unexpectedly succeeded");
    }
}

contract InnerRecorder {
    address public lastSender;
    uint256 public lastValue;
    uint256 public lastArg;

    function record(uint256 arg) external payable {
        lastSender = msg.sender;
        lastValue = msg.value;
        lastArg = arg;
    }

    function die() external payable {
        revert("no");
    }
}

// Splits value between recipients and returns any leftover to msg.sender, the
// pattern that makes the empty-calldata path load bearing.
contract Splitter {
    function split(address payable[] calldata recipients, uint256[] calldata values)
        external
        payable
    {
        for (uint256 i = 0; i < recipients.length; i++) {
            recipients[i].transfer(values[i]);
        }
        uint256 balance = address(this).balance;
        if (balance > 0) {
            payable(msg.sender).transfer(balance);
        }
    }
}

contract ForceSend {
    constructor(address payable to) payable {
        selfdestruct(to);
    }
}

contract PaymentForwarderTest {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);
    address constant CONSOLE = 0x000000000000000000636F6e736F6c652e6c6f67;

    uint64 constant TS = 1_900_000_000;
    address constant PAYER = address(0xba5e01);
    address constant OTHER = address(0x07e5);

    PaymentForwarder fwd;

    function setUp() public {
        fwd = new PaymentForwarder();
        vm.deal(PAYER, 1e30);
        vm.warp(TS);
    }

    function pay(address to, uint64 ts, uint256 value) internal returns (bool ok) {
        vm.prank(PAYER, PAYER);
        (ok,) = address(fwd).call{value: value}(abi.encodePacked(to, ts));
    }

    // -- happy paths --------------------------------------------------------

    function test_paysEoaRecipient() public {
        address r = address(0xe0a);
        vm.deal(r, 1 ether);
        require(pay(r, TS, 3 ether), "pay failed");
        require(r.balance == 4 ether, "wrong recipient balance");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    function test_paysEmptyAccount() public {
        address r = address(0xdead0);
        require(r.balance == 0 && r.code.length == 0, "not empty");
        require(pay(r, TS, 5 ether), "pay failed");
        require(r.balance == 5 ether, "wrong recipient balance");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    function test_paysContractRecipient() public {
        Sink r = new Sink();
        require(pay(address(r), TS, 2 ether), "pay failed");
        require(address(r).balance == 2 ether, "wrong recipient balance");
    }

    function test_paysGasHeavyRecipient() public {
        GasHeavySink r = new GasHeavySink();
        require(pay(address(r), TS, 1 ether), "pay failed");
        require(address(r).balance == 1 ether, "wrong recipient balance");
        require(r.s() == 1, "receive() did not run");
    }

    function test_recipientSeesForwarderAsSender() public {
        SenderRecorder r = new SenderRecorder();
        require(pay(address(r), TS, 1 ether), "pay failed");
        require(r.lastSender() == address(fwd), "unexpected msg.sender");
    }

    function test_innerCalldataToEoaStillPays() public {
        address r = address(0xe0a);
        vm.prank(PAYER, PAYER);
        (bool ok,) = address(fwd).call{value: 1 ether}(abi.encodePacked(r, TS, "junk"));
        require(ok, "pay failed");
        require(r.balance == 1 ether, "wrong recipient balance");
    }

    function test_innerCalldataForwardedToContract() public {
        InnerRecorder r = new InnerRecorder();
        bytes memory inner = abi.encodeWithSignature("record(uint256)", 42);
        vm.prank(PAYER, PAYER);
        (bool ok,) = address(fwd).call{value: 3 ether}(abi.encodePacked(address(r), TS, inner));
        require(ok, "pay failed");
        require(address(r).balance == 3 ether, "wrong recipient balance");
        require(r.lastValue() == 3 ether, "value not seen by inner call");
        require(r.lastArg() == 42, "inner calldata not forwarded");
        require(r.lastSender() == address(fwd), "unexpected msg.sender");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    function test_innerCallRevert_revertsWhole() public {
        InnerRecorder r = new InnerRecorder();
        bytes memory inner = abi.encodeWithSignature("die()");
        vm.prank(PAYER, PAYER);
        (bool ok,) = address(fwd).call{value: 1 ether}(abi.encodePacked(address(r), TS, inner));
        require(!ok, "should revert");
        require(address(r).balance == 0, "recipient must not be paid");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    function test_forwardsWrappedCall() public {
        Splitter splitter = new Splitter();
        address payable[] memory recipients = new address payable[](2);
        recipients[0] = payable(address(0xe0a1));
        recipients[1] = payable(address(0xe0a2));
        uint256[] memory values = new uint256[](2);
        values[0] = 3 ether;
        values[1] = 4 ether;
        bytes memory inner =
            abi.encodeWithSignature("split(address[],uint256[])", recipients, values);

        vm.prank(PAYER, PAYER);
        (bool ok,) =
            address(fwd).call{value: 7 ether}(abi.encodePacked(address(splitter), TS, inner));
        require(ok, "wrapped call failed");
        require(recipients[0].balance == 3 ether, "recipient 0 not paid");
        require(recipients[1].balance == 4 ether, "recipient 1 not paid");
        require(address(splitter).balance == 0, "value stranded in splitter");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    /// Force-sent dust makes the target return a leftover balance to the
    /// forwarder with empty calldata and the 2300 stipend. Rejecting that would
    /// revert every wrapped call to such a target.
    function test_wrappedCallSurvivesLeftoverReturn() public {
        Splitter splitter = new Splitter();
        new ForceSend{value: 5 wei}(payable(address(splitter)));

        address payable[] memory recipients = new address payable[](1);
        recipients[0] = payable(address(0xe0a1));
        uint256[] memory values = new uint256[](1);
        values[0] = 1 ether;
        bytes memory inner =
            abi.encodeWithSignature("split(address[],uint256[])", recipients, values);

        vm.prank(PAYER, PAYER);
        (bool ok,) =
            address(fwd).call{value: 1 ether}(abi.encodePacked(address(splitter), TS, inner));
        require(ok, "wrapped call with dust failed");
        require(recipients[0].balance == 1 ether, "recipient not paid");
        require(address(splitter).balance == 0, "dust stuck in splitter");
        require(address(fwd).balance == 5, "dust must strand in forwarder");
    }

    function test_calldata27Bytes_reverts() public {
        // One byte short of the header.
        bytes memory data = new bytes(27);
        vm.prank(PAYER, PAYER);
        (bool ok,) = address(fwd).call{value: 1 ether}(data);
        require(!ok, "should revert");
    }

    // -- guard reverts ------------------------------------------------------

    function test_timestampAhead_reverts() public {
        require(!pay(address(0xe0a), TS + 12, 1 ether), "should revert");
    }

    function test_timestampBehind_reverts() public {
        require(!pay(address(0xe0a), TS - 12, 1 ether), "should revert");
    }

    function testFuzz_wrongTimestamp_reverts(uint64 ts) public {
        vm.assume(ts != TS);
        require(!pay(address(0xe0a), ts, 1 ether), "should revert");
    }

    /// A payment can be re-signed by a different sender, keeping the same
    /// target and calldata.
    function test_anySenderMayPay() public {
        vm.deal(OTHER, 1 ether);
        address r = address(0xe0a);
        vm.prank(OTHER, OTHER);
        (bool ok,) = address(fwd).call{value: 1 ether}(abi.encodePacked(r, TS));
        require(ok, "pay failed");
        require(r.balance == 1 ether, "wrong recipient balance");
    }

    function test_replayNextSlot_reverts() public {
        address r = address(0xe0a);
        bytes memory calldata_ = abi.encodePacked(r, TS);
        vm.prank(PAYER, PAYER);
        (bool ok,) = address(fwd).call{value: 1 ether}(calldata_);
        require(ok, "original inclusion failed");

        vm.warp(TS + 12);
        vm.prank(PAYER, PAYER);
        (ok,) = address(fwd).call{value: 1 ether}(calldata_);
        require(!ok, "replay must revert");
        require(r.balance == 1 ether, "paid more than once");
    }

    /// Bare receives are accepted within the 2300 stipend so a target returning
    /// a leftover balance to msg.sender does not revert the outer call.
    function test_emptyCalldataAcceptedAndStranded() public {
        vm.prank(PAYER, PAYER);
        bool ok = payable(address(fwd)).send(1 ether);
        require(ok, "bare send must be accepted");
        require(address(fwd).balance == 1 ether, "value must strand");
    }

    function test_shortCalldata_reverts() public {
        // 20 bytes: ts loads as 0 != block.timestamp.
        vm.prank(PAYER, PAYER);
        (bool ok,) = address(fwd).call{value: 1 ether}(abi.encodePacked(address(0xe0a)));
        require(!ok, "should revert");
    }

    // -- recipient failure containment --------------------------------------

    function test_revertingRecipient_revertsWhole() public {
        Revertor r = new Revertor();
        require(!pay(address(r), TS, 1 ether), "should revert");
        require(address(r).balance == 0, "recipient must not be paid");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    function test_noReceiveRecipient_revertsWhole() public {
        NoReceive r = new NoReceive();
        require(!pay(address(r), TS, 1 ether), "should revert");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    // -- statelessness ------------------------------------------------------

    function test_rescuerWithdraws() public {
        address rescuer = 0x367103073f54Ad295B894e41F6A58a2bA8223B0F;
        new ForceSend{value: 5 wei}(payable(address(fwd)));
        vm.prank(rescuer, rescuer);
        fwd.withdraw();
        require(address(fwd).balance == 0, "not swept");
        require(rescuer.balance == 5, "rescuer not credited");
    }

    function test_nonRescuerWithdraw_reverts() public {
        new ForceSend{value: 5 wei}(payable(address(fwd)));
        vm.prank(PAYER, PAYER);
        (bool ok,) = address(fwd).call(abi.encodeWithSignature("withdraw()"));
        require(!ok, "must revert for non-rescuer");
        require(address(fwd).balance == 5, "balance must be untouched");
    }

    /// A recipient whose leading bytes are the withdraw() selector dispatches
    /// there instead of the fallback, so the payment reverts rather than
    /// misroutes.
    function test_selectorCollidingRecipient_reverts() public {
        address r = address(uint160(uint32(PaymentForwarder.withdraw.selector)) << 128);
        vm.deal(r, 0);
        require(!pay(r, TS, 1 ether), "must revert, not misroute");
    }

    function test_forceSentDustStaysStuck() public {
        new ForceSend{value: 3 wei}(payable(address(fwd)));
        require(address(fwd).balance == 3, "dust not delivered");

        address r = address(0xe0a);
        require(pay(r, TS, 1 ether), "pay failed");
        // Exactly callvalue forwarded: dust must not leak into a payment.
        require(r.balance == 1 ether, "dust swept into payment");
        require(address(fwd).balance == 3, "dust balance changed");
    }

    function test_reentrancyHarmless() public {
        Reenter r = new Reenter(payable(address(fwd)));
        require(pay(address(r), TS, 1 ether), "pay failed");
        require(address(r).balance == 1 ether, "wrong recipient balance");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    // -- gas properties ------------------------------------------------------

    /// Gas must not depend on the value forwarded, so a caller can rewrite the
    /// value of a signed payment without re-estimating gas.
    function test_gasValueInvariant() public {
        // Warm the forwarder so both measured calls see it warm.
        require(pay(address(0xaaa1), TS, 1 wei), "warmup failed");

        // Fresh cold recipients: identical gas profiles.
        uint256 g1 = payGas(address(0xaaa2), 1 wei);
        uint256 g2 = payGas(address(0xaaa3), 1e24);
        require(g1 == g2, "gas depends on value");
    }

    function testFuzz_paysExactValue(address r, uint96 value) public {
        vm.assume(value > 0);
        vm.assume(r != PAYER && r != address(fwd) && r != address(this));
        vm.assume(r != address(vm) && r != CONSOLE);
        vm.assume(uint160(r) > 0xffff); // precompiles
        // selector-colliding recipients dispatch to withdraw() and revert;
        // pinned by test_selectorCollidingRecipient_reverts
        vm.assume(uint160(r) >> 128 != uint32(PaymentForwarder.withdraw.selector));
        vm.assume(r.code.length == 0 && r.balance == 0);
        require(pay(r, TS, value), "pay failed");
        require(r.balance == value, "wrong recipient balance");
        require(address(fwd).balance == 0, "value stranded in forwarder");
    }

    // -- bytecode pin --------------------------------------------------------

    /// The runtime deployed at the address in README.md. Different source or
    /// solc settings break this on purpose: they change the CREATE2 address.
    function test_runtimeBytecodeMatchesCommitted() public view {
        bytes memory expected =
            hex"60806040526004361061001d575f3560e01c80633ccfd60b1461007f575b3661002457005b601c361015610031575f5ffd5b4260143560c01c14610041575f5ffd5b7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe4360180601c5f375f5f825f345f3560601c5af161007d575f5ffd5b005b34801561008a575f5ffd5b5061007d3373367103073f54ad295b894e41f6a58a2ba8223b0f146100ad575f5ffd5b6040515f9073367103073f54ad295b894e41f6a58a2ba8223b0f9047908381818185875af1925050503d805f8114610100576040519150601f19603f3d011682016040523d82523d5f602084013e610105565b606091505b5050905080610112575f5ffd5b5056";
        require(keccak256(address(fwd).code) == keccak256(expected), "runtime bytecode drifted");
    }

    function payGas(address to, uint256 value) internal returns (uint256 used) {
        bytes memory data = abi.encodePacked(to, TS);
        vm.prank(PAYER, PAYER);
        uint256 g0 = gasleft();
        (bool ok,) = address(fwd).call{value: value}(data);
        used = g0 - gasleft();
        require(ok, "pay failed");
    }
}
