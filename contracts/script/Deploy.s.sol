// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

interface Vm {
    function startBroadcast() external;
    function stopBroadcast() external;
}

/// Deploys PaymentForwarder.huff through the canonical deterministic-deployment
/// proxy, so the forwarder lands at the same address on every chain:
///
///   forge script script/Deploy.s.sol --rpc-url $RPC --broadcast \
///       --private-key $DEPLOYER_KEY
///
/// The address depends only on the salt, the init code and the proxy, not on who
/// sends the deployment.
contract Deploy {
    Vm constant vm = Vm(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D);
    address constant CREATE2_PROXY = 0x4e59b44847b379578588920cA78FbF26c0B4956C;
    bytes32 constant SALT =
        0x7a607ae0a692287e530cda81144a4d223ebb50453f102b830ce657ef26b99453;

    /// PUSH20 <runtime>, MSTORE, RETURN(12, 20).
    bytes constant INIT_CODE = hex"735f358060e01c4218600f5760401cff5b5f5ffd005f526014600cf3";

    function run() external returns (address deployed) {
        deployed = address(
            uint160(
                uint256(
                    keccak256(
                        abi.encodePacked(bytes1(0xff), CREATE2_PROXY, SALT, keccak256(INIT_CODE))
                    )
                )
            )
        );
        vm.startBroadcast();
        (bool ok,) = CREATE2_PROXY.call(abi.encodePacked(SALT, INIT_CODE));
        vm.stopBroadcast();
        require(ok && deployed.code.length > 0, "create2 deploy failed");
    }
}
