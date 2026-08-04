// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

// Minimal cheatcode surface so the repo carries no forge-std submodule.
// Assertions are plain require(); forge fails a test iff it reverts.
interface Vm {
    function warp(uint256) external;
    function prank(address sender, address txOrigin) external;
    function deal(address, uint256) external;
    function etch(address, bytes calldata) external;
    function assume(bool) external pure;
}

contract Reverter {
    receive() external payable {
        revert("no");
    }
}

contract PaymentForwarderTest {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);

    /// PaymentForwarder.huff, the code deployed at the address in README.md.
    bytes constant RUNTIME = hex"5f358060e01c4218600f5760401cff5b5f5ffd00";

    uint32 constant TS = 1_900_000_000;
    address constant FWD = address(0xfd0);
    address constant PAYER = address(0xba5e01);

    function setUp() public {
        vm.etch(FWD, RUNTIME);
        vm.deal(PAYER, 1e30);
        vm.warp(TS);
    }

    function pay(address recipient, uint32 ts, uint256 value) internal returns (bool ok) {
        vm.prank(PAYER, PAYER);
        (ok,) = FWD.call{value: value}(abi.encodePacked(ts, recipient));
    }

    function test_paysRecipient() public {
        address r = address(0xe0a);
        vm.deal(r, 1 ether);
        require(pay(r, TS, 3 ether), "pay failed");
        require(r.balance == 4 ether, "wrong recipient balance");
        require(FWD.balance == 0, "value left behind");
    }

    function test_paysEmptyAccount() public {
        address r = address(0xdead0);
        require(pay(r, TS, 5 ether), "pay failed");
        require(r.balance == 5 ether, "wrong recipient balance");
    }

    /// The recipient's code never runs, so a recipient that would reject a
    /// transfer is still paid.
    function test_paysRecipientWithoutExecutingIt() public {
        Reverter r = new Reverter();
        require(pay(address(r), TS, 2 ether), "pay failed");
        require(address(r).balance == 2 ether, "wrong recipient balance");
    }

    /// The whole balance is forwarded, so anything sent here beforehand is paid
    /// out with the next payment rather than stranded.
    function test_forwardsEntireBalance() public {
        vm.deal(FWD, 7 wei);
        address r = address(0xe0a);
        require(pay(r, TS, 1 ether), "pay failed");
        require(r.balance == 1 ether + 7, "leftover not forwarded");
        require(FWD.balance == 0, "value left behind");
    }

    function test_wrongTimestamp_reverts() public {
        require(!pay(address(0xe0a), TS - 12, 1 ether), "should revert");
        require(!pay(address(0xe0a), TS + 12, 1 ether), "should revert");
    }

    function testFuzz_wrongTimestamp_reverts(uint32 ts) public {
        vm.assume(ts != TS);
        require(!pay(address(0xe0a), ts, 1 ether), "should revert");
    }

    function test_replayNextSlot_reverts() public {
        address r = address(0xe0a);
        bytes memory calldata_ = abi.encodePacked(TS, r);
        vm.prank(PAYER, PAYER);
        (bool ok,) = FWD.call{value: 1 ether}(calldata_);
        require(ok, "original inclusion failed");

        vm.warp(uint256(TS) + 12);
        vm.prank(PAYER, PAYER);
        (ok,) = FWD.call{value: 1 ether}(calldata_);
        require(!ok, "replay must revert");
        require(r.balance == 1 ether, "paid more than once");
    }

    /// There is no length check: with the recipient omitted it reads as zero and
    /// the balance is burned, so callers must encode all 24 bytes.
    function test_missingRecipient_burnsBalance() public {
        uint256 burnedBefore = address(0).balance;
        vm.prank(PAYER, PAYER);
        (bool ok,) = FWD.call{value: 1 ether}(abi.encodePacked(TS));
        require(ok, "call failed");
        require(address(0).balance == burnedBefore + 1 ether, "not burned");
    }

    /// A payment can be re-signed by a different sender, keeping the same
    /// calldata, which is what a relay adjusting a bid does.
    function test_anySenderMayPay() public {
        address other = address(0x07e5);
        vm.deal(other, 1 ether);
        address r = address(0xe0a);
        vm.prank(other, other);
        (bool ok,) = FWD.call{value: 1 ether}(abi.encodePacked(TS, r));
        require(ok, "pay failed");
        require(r.balance == 1 ether, "wrong recipient balance");
    }

    /// Gas depends on neither the value nor the recipient's code, so a caller
    /// can rewrite the value of a signed payment without re-estimating.
    function test_gasIsIndependentOfValueAndRecipient() public {
        Reverter contractRecipient = new Reverter();
        require(pay(address(0xaaa0), TS, 1 wei), "warmup failed");
        vm.deal(address(0xaaa1), 1);
        vm.deal(address(0xaaa2), 1);
        vm.deal(address(contractRecipient), 1);

        uint256 small = payGas(address(0xaaa1), 1 wei);
        uint256 large = payGas(address(0xaaa2), 1e24);
        uint256 toContract = payGas(address(contractRecipient), 1e24);
        require(small == large, "gas depends on value");
        require(small == toContract, "gas depends on recipient code");
    }

    function payGas(address recipient, uint256 value) internal returns (uint256 used) {
        bytes memory data = abi.encodePacked(TS, recipient);
        vm.prank(PAYER, PAYER);
        uint256 before = gasleft();
        (bool ok,) = FWD.call{value: value}(data);
        used = before - gasleft();
        require(ok, "pay failed");
    }
}
