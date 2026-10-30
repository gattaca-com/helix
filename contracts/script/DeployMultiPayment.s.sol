// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

interface Vm {
    function startBroadcast() external;
    function stopBroadcast() external;
}

/// Deploys MultiPaymentLeg.huff and MultiPaymentForwarder.huff through the
/// canonical deterministic-deployment proxy, so both land at the same address on
/// every chain:
///
///   forge script script/DeployMultiPayment.s.sol --rpc-url $RPC --broadcast \
///       --private-key $DEPLOYER_KEY
///
/// The forwarder's runtime hardcodes the leg's address. The leg goes first: a
/// call to a leg without code would succeed and keep the entry's amount.
contract DeployMultiPayment {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);
    address constant CREATE2_PROXY = 0x4e59b44847b379578588920cA78FbF26c0B4956C;

    address constant LEG = 0xFeEeEEE0e9C10f0484111144Ba82788E0B550D91;
    bytes32 constant LEG_SALT = 0xbe7118254507ff4648493e173ea0790cd064480fe40ebf342500b748b42f7d65;
    /// PUSH6 <runtime>, MSTORE, RETURN(26, 6).
    bytes constant LEG_INIT = hex"655f3560601cff5f526006601af3";

    address constant FORWARDER = 0xFeEeEEEAEA9b3cE71Ce8A252aAa11D2dC439CC69;
    bytes32 constant FORWARDER_SALT = 0xb5315133f201e55fb71dbd10a07e056154f12e04c2c7d5d7bc2664ffd66e6712;
    /// CODECOPY the 99-byte runtime that follows this 9-byte prefix, RETURN it.
    bytes constant FORWARDER_INIT =
        hex"60638060095f395ff35f358060e01c4218605f5760401c601836811080601a575050ff5b5f60145f8435805f526bffffffffffffffffffffffff1673feeeeee0e9c10f0484111144ba82788e0b550d915af190602001908036831016601a579036141615605f57ff5b5f5ffd";

    /// Where FORWARDER_INIT holds the leg address: the 9-byte prefix, then the
    /// runtime's PUSH20 at 0x32.
    uint256 constant LEG_PUSH = 9 + 0x32;

    function run() external {
        bytes memory init = FORWARDER_INIT;
        address embedded;
        assembly {
            embedded := shr(96, mload(add(add(init, 0x20), add(LEG_PUSH, 1))))
        }
        require(init[LEG_PUSH] == 0x73 && embedded == LEG, "forwarder does not pay through LEG");

        vm.startBroadcast();
        deploy(LEG_SALT, LEG_INIT, LEG);
        deploy(FORWARDER_SALT, FORWARDER_INIT, FORWARDER);
        vm.stopBroadcast();
    }

    function deploy(bytes32 salt, bytes memory init, address expected) internal {
        address predicted =
            address(uint160(uint256(keccak256(abi.encodePacked(bytes1(0xff), CREATE2_PROXY, salt, keccak256(init))))));
        require(predicted == expected, "salt or init code changed");
        if (expected.code.length > 0) return;
        (bool ok,) = CREATE2_PROXY.call(abi.encodePacked(salt, init));
        require(ok && expected.code.length > 0, "create2 deploy failed");
    }
}
