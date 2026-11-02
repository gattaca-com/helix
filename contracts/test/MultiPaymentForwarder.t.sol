// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

// Minimal cheatcode surface so the repo carries no forge-std submodule.
// Assertions are plain require(); forge fails a test iff it reverts.
interface Vm {
    struct Gas {
        uint64 gasLimit;
        uint64 gasTotalUsed;
        uint64 gasMemoryUsed;
        int64 gasRefunded;
        uint64 gasRemaining;
    }

    function warp(uint256) external;
    function prank(address sender, address txOrigin) external;
    function deal(address, uint256) external;
    function etch(address, bytes calldata) external;
    function assume(bool) external pure;
    function cool(address) external;
    function lastCallGas() external view returns (Gas memory);
}

contract Rejecter {
    receive() external payable {
        revert("no");
    }
}

contract MultiPaymentForwarderTest {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);

    address constant CREATE2_PROXY = 0x4e59b44847b379578588920cA78FbF26c0B4956C;

    /// MultiPaymentLeg.huff and MultiPaymentForwarder.huff, the code deployed at
    /// the addresses in README.md, with the init code and salts that put it there.
    address constant LEG = 0xFeEeEEE0e9C10f0484111144Ba82788E0B550D91;
    bytes constant LEG_RUNTIME = hex"5f3560601cff";
    bytes constant LEG_INIT = hex"655f3560601cff5f526006601af3";
    bytes32 constant LEG_SALT = 0xbe7118254507ff4648493e173ea0790cd064480fe40ebf342500b748b42f7d65;

    address constant MPF = 0xFeEeEEEAEA9b3cE71Ce8A252aAa11D2dC439CC69;
    bytes constant MPF_RUNTIME =
        hex"5f358060e01c4218605f5760401c601836811080601a575050ff5b5f60145f8435805f526bffffffffffffffffffffffff1673feeeeee0e9c10f0484111144ba82788e0b550d915af190602001908036831016601a579036141615605f57ff5b5f5ffd";
    bytes constant MPF_INIT =
        hex"60638060095f395ff35f358060e01c4218605f5760401c601836811080601a575050ff5b5f60145f8435805f526bffffffffffffffffffffffff1673feeeeee0e9c10f0484111144ba82788e0b550d915af190602001908036831016601a579036141615605f57ff5b5f5ffd";
    bytes32 constant MPF_SALT = 0xb5315133f201e55fb71dbd10a07e056154f12e04c2c7d5d7bc2664ffd66e6712;

    uint32 constant TS = 1_900_000_000;
    address constant PAYER = address(0xba5e01);

    /// Deploys both contracts the way script/DeployMultiPayment.s.sol does, so
    /// every test runs against what the init code actually leaves on chain.
    function setUp() public {
        deploy(LEG_SALT, LEG_INIT, LEG, LEG_RUNTIME);
        deploy(MPF_SALT, MPF_INIT, MPF, MPF_RUNTIME);
        vm.deal(PAYER, 1e30);
        vm.warp(TS);
    }

    function deploy(bytes32 salt, bytes memory init, address want, bytes memory runtime) internal {
        (bool ok,) = CREATE2_PROXY.call(abi.encodePacked(salt, init));
        require(ok, "create2 deploy failed");
        require(keccak256(want.code) == keccak256(runtime), "unexpected address or runtime");
    }

    function entry(address recipient, uint96 amount) internal pure returns (bytes memory) {
        return abi.encodePacked(recipient, amount);
    }

    function pay(uint32 ts, address finalRecipient, bytes memory entries, uint256 value) internal returns (bool ok) {
        vm.prank(PAYER, PAYER);
        (ok,) = MPF.call{value: value}(abi.encodePacked(ts, finalRecipient, entries));
    }

    function test_paysEntriesAndRestToFinalRecipient() public {
        address r1 = address(0xe01);
        address r2 = address(0xe02);
        address f = address(0xe0f);
        vm.deal(r1, 1 ether);
        vm.deal(r2, 1 ether);
        vm.deal(f, 1 ether);
        bytes memory entries = bytes.concat(entry(r1, 1 ether), entry(r2, 2 ether));
        require(pay(TS, f, entries, 3.5 ether), "pay failed");
        require(r1.balance == 2 ether, "wrong r1 balance");
        require(r2.balance == 3 ether, "wrong r2 balance");
        require(f.balance == 1.5 ether, "wrong final balance");
        require(MPF.balance == 0 && LEG.balance == 0, "value left behind");
    }

    /// The header is PaymentForwarder's calldata, and with no entries the
    /// payment is the same.
    function test_noEntries_paysLikePaymentForwarder() public {
        address f = address(0xe0f);
        require(pay(TS, f, "", 3 ether), "pay failed");
        require(f.balance == 3 ether, "wrong final balance");
        require(MPF.balance == 0, "value left behind");
    }

    /// No recipient's code runs, so recipients that would reject a transfer
    /// are still paid.
    function test_paysRecipientsWithoutExecutingThem() public {
        Rejecter r = new Rejecter();
        Rejecter f = new Rejecter();
        require(pay(TS, address(f), entry(address(r), 1 ether), 3 ether), "pay failed");
        require(address(r).balance == 1 ether, "wrong entry balance");
        require(address(f).balance == 2 ether, "wrong final balance");
    }

    function test_paysEmptyAccounts() public {
        address r = address(0xdead01);
        address f = address(0xdead02);
        require(pay(TS, f, entry(r, 1 ether), 5 ether), "pay failed");
        require(r.balance == 1 ether, "wrong entry balance");
        require(f.balance == 4 ether, "wrong final balance");
    }

    function test_paysSameRecipientMoreThanOnce() public {
        address r = address(0xe01);
        bytes memory entries = bytes.concat(entry(r, 1 ether), entry(r, 2 ether));
        require(pay(TS, r, entries, 4 ether), "pay failed");
        require(r.balance == 4 ether, "wrong balance");
    }

    function test_zeroAmountEntry() public {
        address r = address(0xe01);
        address f = address(0xe0f);
        require(pay(TS, f, entry(r, 0), 1 ether), "pay failed");
        require(r.balance == 0, "zero entry paid");
        require(f.balance == 1 ether, "wrong final balance");
    }

    function test_amountsAboveValue_revert() public {
        address r1 = address(0xe01);
        address r2 = address(0xe02);
        bytes memory entries = bytes.concat(entry(r1, 1 ether), entry(r2, 1 ether));
        require(!pay(TS, address(0xe0f), entries, 1.5 ether), "should revert");
        require(r1.balance == 0 && r2.balance == 0, "partially paid");
    }

    function test_wrongTimestamp_reverts() public {
        require(!pay(TS - 12, address(0xe0f), entry(address(0xe01), 1), 1 ether), "should revert");
        require(!pay(TS + 12, address(0xe0f), entry(address(0xe01), 1), 1 ether), "should revert");
    }

    function testFuzz_wrongTimestamp_reverts(uint32 ts) public {
        vm.assume(ts != TS);
        require(!pay(ts, address(0xe0f), entry(address(0xe01), 1), 1 ether), "should revert");
    }

    function test_replayNextSlot_reverts() public {
        address r = address(0xe01);
        address f = address(0xe0f);
        bytes memory calldata_ = abi.encodePacked(TS, f, entry(r, 1 ether));
        vm.prank(PAYER, PAYER);
        (bool ok,) = MPF.call{value: 2 ether}(calldata_);
        require(ok, "original inclusion failed");

        vm.warp(uint256(TS) + 12);
        vm.prank(PAYER, PAYER);
        (ok,) = MPF.call{value: 2 ether}(calldata_);
        require(!ok, "replay must revert");
        require(r.balance == 1 ether && f.balance == 1 ether, "paid more than once");
    }

    /// Anything sent to either contract beforehand is paid out with the next
    /// payment rather than stranded: the leg's with the first entry, the
    /// forwarder's to the final recipient.
    function test_strayBalancesArePaidOut() public {
        vm.deal(LEG, 7 wei);
        vm.deal(MPF, 5 wei);
        address r1 = address(0xe01);
        address r2 = address(0xe02);
        address f = address(0xe0f);
        bytes memory entries = bytes.concat(entry(r1, 1 ether), entry(r2, 1 ether));
        require(pay(TS, f, entries, 2 ether), "pay failed");
        require(r1.balance == 1 ether + 7, "leg leftover not paid with the first entry");
        require(r2.balance == 1 ether, "wrong r2 balance");
        require(f.balance == 5, "forwarder leftover not paid to the final recipient");
        require(MPF.balance == 0 && LEG.balance == 0, "value left behind");
    }

    /// With entries, the calldata must be the header plus whole 32-byte
    /// entries: a cut-short or overlong tail reverts rather than being read
    /// zero-padded.
    function test_partialEntry_reverts() public {
        address r = address(0xe01);
        address f = address(0xe0f);
        require(!pay(TS, f, abi.encodePacked(r), 1 ether), "cut-short entry must revert");
        require(!pay(TS, f, bytes.concat(entry(r, 1), hex"00"), 1 ether), "trailing byte must revert");
        require(r.balance == 0 && f.balance == 0, "paid");
    }

    function testFuzz_paysExactAmounts(uint96[4] memory amounts, uint96 rest, uint8 n) public {
        n %= 5;
        bytes memory entries;
        uint256 total = rest;
        for (uint256 i; i < n; i++) {
            entries = bytes.concat(entries, entry(address(uint160(0x1000 + i)), amounts[i]));
            total += amounts[i];
        }
        address f = address(0xf1a1);
        require(pay(TS, f, entries, total), "pay failed");
        for (uint256 i; i < n; i++) {
            require(address(uint160(0x1000 + i)).balance == amounts[i], "wrong entry balance");
        }
        require(f.balance == rest, "wrong final balance");
    }

    /// The leg pays its whole balance to whoever the calldata names, for any
    /// caller, without running the recipient.
    function test_leg_paysBalanceToRecipient() public {
        vm.deal(LEG, 1 wei);
        Rejecter r = new Rejecter();
        address caller = address(0x07e5);
        vm.deal(caller, 2 ether);
        vm.prank(caller, caller);
        (bool ok,) = LEG.call{value: 2 ether}(abi.encodePacked(address(r)));
        require(ok, "leg call failed");
        require(address(r).balance == 2 ether + 1, "wrong recipient balance");
        require(LEG.balance == 0, "value left behind");
    }

    // Execution gas, the README's formula: a payment's gas is these constants,
    // the transaction's 21,000 and its calldata, whatever the amounts.
    uint256 constant NO_ENTRIES = 7_669;
    uint256 constant BASE = 10_195;
    uint256 constant PER_ENTRY = 14_483;
    uint256 constant WARM_RECIPIENT = 2_600; // less, per recipient already accessed
    uint256 constant NEW_ACCOUNT = 25_000; // more, per payment > 0 to an empty account
    uint256 constant ZERO_AMOUNT = 6_700; // less, per entry with amount 0

    address constant F = address(0xf1a1);

    /// Gas of a payment transaction: forge measures a call from the test as a
    /// transaction of its own, intrinsic and calldata gas included, with every
    /// account cold. The forwarder is pre-funded so the call carries no value.
    function txGas(address finalRecipient, bytes memory entries) internal returns (uint256) {
        vm.deal(MPF, 1e27);
        vm.prank(PAYER, PAYER);
        (bool ok,) = MPF.call(abi.encodePacked(TS, finalRecipient, entries));
        require(ok, "pay failed");
        return vm.lastCallGas().gasTotalUsed;
    }

    /// 21,000 plus 4 gas per zero byte and 16 per non-zero byte of calldata. The
    /// EIP-7623 floor never binds: execution outweighs it many times over.
    function expected(address finalRecipient, bytes memory entries, uint256 exec) internal pure returns (uint256 gas) {
        bytes memory data = abi.encodePacked(TS, finalRecipient, entries);
        gas = 21_000 + exec;
        for (uint256 i; i < data.length; i++) {
            gas += data[i] == 0 ? 4 : 16;
        }
    }

    function funded(uint160 seed, uint256 n) internal returns (address[] memory accounts) {
        accounts = new address[](n);
        for (uint256 i; i < n; i++) {
            accounts[i] = address(seed + uint160(i));
            vm.deal(accounts[i], 1);
        }
    }

    function test_gas_noEntries() public {
        vm.deal(F, 1);
        require(txGas(F, "") == expected(F, "", NO_ENTRIES), "no entries");
    }

    function test_gas_perEntry() public {
        vm.deal(F, 1);
        for (uint256 n = 1; n <= 4; n++) {
            address[] memory r = funded(uint160(0x1000 * n), n);
            bytes memory entries;
            for (uint256 i; i < n; i++) {
                entries = bytes.concat(entries, entry(r[i], uint96(1e14 * (i + 1))));
            }
            require(txGas(F, entries) == expected(F, entries, BASE + n * PER_ENTRY), "n entries");
        }
    }

    function test_gas_independentOfAmountAndRecipientCode() public {
        vm.deal(F, 1);
        address[] memory r = funded(0x1000, 1);
        address code = address(0x2000);
        vm.etch(code, type(Rejecter).runtimeCode);
        vm.deal(code, 1);
        uint256 exec = BASE + PER_ENTRY;
        bytes memory small = entry(r[0], 1);
        bytes memory large = entry(r[0], 1e26);
        bytes memory toCode = entry(code, 1e18);
        require(txGas(F, small) == expected(F, small, exec), "small amount");
        require(txGas(F, large) == expected(F, large, exec), "large amount");
        require(txGas(F, toCode) == expected(F, toCode, exec), "recipient with code");
    }

    function test_gas_adjustments() public {
        vm.deal(F, 1);
        address[] memory r = funded(0x1000, 2);

        // A recipient paid twice is warm the second time.
        bytes memory twice = bytes.concat(entry(r[0], 1), entry(r[0], 1));
        require(txGas(F, twice) == expected(F, twice, BASE + 2 * PER_ENTRY - WARM_RECIPIENT), "repeat recipient");
        // So is a final recipient that was an entry's.
        bytes memory one = entry(r[1], 1);
        require(txGas(r[1], one) == expected(r[1], one, BASE + PER_ENTRY - WARM_RECIPIENT), "final was an entry");
        // Paying an empty account creates it.
        bytes memory toEmpty = entry(address(0xdead01), 1);
        require(txGas(F, toEmpty) == expected(F, toEmpty, BASE + PER_ENTRY + NEW_ACCOUNT), "empty recipient");
        // An entry of 0 carries no value: no transfer cost, nothing created.
        bytes memory zero = entry(address(0xdead02), 0);
        require(txGas(F, zero) == expected(F, zero, BASE + PER_ENTRY - ZERO_AMOUNT), "zero amount");
        // The final recipient too is created when paid.
        require(
            txGas(address(0xdead03), "") == expected(address(0xdead03), "", NO_ENTRIES + NEW_ACCOUNT),
            "empty final recipient"
        );
    }

    /// A gas limit of exactly the formula is enough: the 1/64 a call keeps back
    /// never starves a leg, since the final payment always comes after it.
    /// One gas less runs out on a real transaction (checked on anvil), but how
    /// forge applies a call's gas to an isolated transaction differs between
    /// versions, so only the sufficient side is asserted here.
    function test_gas_exactLimitSuffices() public {
        vm.deal(F, 1);
        address[] memory r = funded(0x1000, 3);
        address empty = address(0xdead04);
        bytes memory entries = bytes.concat(entry(r[0], 1e18), entry(empty, 1e18), entry(r[1], 1e18), entry(r[2], 1e18));
        bytes memory data = abi.encodePacked(TS, F, entries);
        uint256 limit = expected(F, entries, BASE + 4 * PER_ENTRY + NEW_ACCOUNT);
        vm.deal(MPF, 1e27);

        (bool ok,) = MPF.call{gas: limit}(data);
        require(ok, "exact limit must succeed");
        require(empty.balance == 1e18, "not paid");
    }
}
