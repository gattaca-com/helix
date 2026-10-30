#!/usr/bin/env bash
# Checks that the MultiPayment Huff sources compile to the runtime the tests and
# the deploy script carry, which are maintained by hand. Needs huff2's `huff`
# CLI (https://github.com/huff-language/huff2), on PATH or as $HUFF.
set -euo pipefail
cd "$(dirname "$0")/.."
huff=${HUFF:-huff}
status=0
for name in MultiPaymentForwarder MultiPaymentLeg; do
    runtime=$("$huff" "src/$name.huff" MAIN | tail -n 1)
    runtime=${runtime#0x}
    if ! grep -q "hex\"$runtime\"" test/MultiPaymentForwarder.t.sol; then
        echo "$name: test runtime differs from src/$name.huff" >&2
        status=1
    fi
    if ! grep -q "$runtime" script/DeployMultiPayment.s.sol; then
        echo "$name: deploy init code does not carry src/$name.huff" >&2
        status=1
    fi
done
exit $status
