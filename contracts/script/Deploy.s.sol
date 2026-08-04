// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {PaymentForwarder} from "../src/PaymentForwarder.sol";

interface Vm {
    function startBroadcast() external;
    function stopBroadcast() external;
}

/// Deploys through the canonical deterministic-deployment proxy, so the
/// forwarder lands at the same address on every chain:
///
///   forge script script/Deploy.s.sol --rpc-url $RPC --broadcast \
///       --private-key $DEPLOYER_KEY
///
/// The address depends only on the salt, the init code and the proxy, not on
/// who sends the deployment.
contract Deploy {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);
    address constant CREATE2_PROXY = 0x4e59b44847b379578588920cA78FbF26c0B4956C;
    bytes32 constant SALT =
        0xf1bf8c685a6f0b052ea171d451a7f10138754ca328a6b2bf2c4b2e6dcb593529;

    function run() external returns (address deployed) {
        bytes memory initCode = type(PaymentForwarder).creationCode;
        deployed = address(
            uint160(
                uint256(
                    keccak256(
                        abi.encodePacked(bytes1(0xff), CREATE2_PROXY, SALT, keccak256(initCode))
                    )
                )
            )
        );
        vm.startBroadcast();
        (bool ok,) = CREATE2_PROXY.call(abi.encodePacked(SALT, initCode));
        vm.stopBroadcast();
        require(ok && deployed.code.length > 0, "create2 deploy failed");
    }
}
