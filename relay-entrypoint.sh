#!/bin/sh
# Runs the relay and its data gatherer side by side.
#
# The gatherer attaches to the relay shared-memory queues, so it starts
# once the relay marks them ready (the epoch file), and a supervisor loop
# restarts it whenever it exits: a relay restart orphans its mappings and
# the gatherer exits loudly rather than reading deleted files.
# Without sinks configured the gatherer exits with status 3 and the loop
# stops.
set -u

/app/helix-relay "$@" &
RELAY_PID=$!

EPOCH_FILE="${XDG_DATA_HOME:-$HOME/.local/share}/helix/shmem/epoch"
i=0
while [ ! -f "$EPOCH_FILE" ] && kill -0 "$RELAY_PID" 2>/dev/null && [ "$i" -lt 300 ]; do
    sleep 0.2
    i=$((i + 1))
done

(
    # TERM reaches only this subshell: forward it so the gatherer drains.
    CHILD=
    trap 'kill -TERM "$CHILD" 2>/dev/null; wait "$CHILD"; exit 0' TERM INT
    while true; do
        /app/data-gatherer "$@" &
        CHILD=$!
        wait "$CHILD"
        [ $? -eq 3 ] && exit 0
        sleep 1
    done
) &
GATHER_LOOP_PID=$!

shutdown() {
    kill -TERM "$RELAY_PID" "$GATHER_LOOP_PID" 2>/dev/null
    wait
}
trap shutdown TERM INT

wait "$RELAY_PID"
STATUS=$?
kill -TERM "$GATHER_LOOP_PID" 2>/dev/null
wait
exit "$STATUS"
