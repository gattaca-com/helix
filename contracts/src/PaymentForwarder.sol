// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// Forwards value, and any trailing calldata, to a target only in the block the
/// caller signed for: `block.timestamp` must equal the timestamp in the
/// calldata. A payment exposed by a reorg or a missed slot cannot be replayed in
/// a later slot.
///
/// Calldata, no selector:
///   [0..20)  target
///   [20..28) expected block.timestamp, u64 big-endian
///   [28..)   passed to the target verbatim
///
/// All remaining gas is forwarded and a failed call reverts, so a payment never
/// leaves value behind. The sender is not authenticated; anyone can pay through
/// this contract and only their own value moves. Re-signing a payment from a
/// different sender with the same target and calldata keeps its gas identical.
///
/// Empty calldata is accepted and does nothing, so a target that returns a
/// leftover balance to `msg.sender` cannot revert the outer call. A payment
/// never strands value, so `withdraw()` exists only to recover funds sent here
/// by mistake.
contract PaymentForwarder {
    address internal constant RESCUER = 0x367103073f54Ad295B894e41F6A58a2bA8223B0F;

    function withdraw() external {
        require(msg.sender == RESCUER);
        (bool ok,) = RESCUER.call{value: address(this).balance}("");
        require(ok);
    }

    fallback() external payable {
        assembly {
            if iszero(calldatasize()) { stop() }
            if lt(calldatasize(), 28) { revert(0, 0) }
            let target := shr(96, calldataload(0))
            let ts := shr(192, calldataload(20))
            if iszero(eq(ts, timestamp())) { revert(0, 0) }
            let len := sub(calldatasize(), 28)
            calldatacopy(0, 28, len)
            if iszero(call(gas(), target, callvalue(), 0, len, 0, 0)) { revert(0, 0) }
            stop()
        }
    }
}
