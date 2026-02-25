#!/bin/sh
set -e

# Parse --config arg
CONFIG_FILE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --config) CONFIG_FILE="$2"; shift 2 ;;
        *) shift ;;
    esac
done

if [ -z "$CONFIG_FILE" ]; then
    echo "error: --config <path> required"
    exit 1
fi

echo "config: $CONFIG_FILE"

# Resolve host IP: default gateway inside Docker is the host
HOST_IP="${HOST_IP:-$(ip route | awk '/default/ { print $3 }')}"
echo "host IP: $HOST_IP"

# Extract URLs from config
BEACON_URL=$(awk '/^beacon_clients:/{found=1; next} found && /url:/{gsub(/.*"http:\/\/|".*/, ""); print; exit}' "$CONFIG_FILE")
BLOCKSIM_URL=$(awk '/^simulators:/{found=1; next} found && /url:/{gsub(/.*"http:\/\/|".*/, ""); print; exit}' "$CONFIG_FILE")
PG_HOST=$(awk '/^postgres:/{found=1; next} found && /hostname:/{gsub(/.*hostname: *"|"$/, ""); print; exit}' "$CONFIG_FILE")
PG_PORT=$(awk '/^postgres:/{found=1; next} found && /port:/{gsub(/[^0-9]/, ""); print; exit}' "$CONFIG_FILE")

BEACON_HOST="${BEACON_URL%:*}"
BEACON_PORT="${BEACON_URL##*:}"
BLOCKSIM_HOST="${BLOCKSIM_URL%:*}"
BLOCKSIM_PORT="${BLOCKSIM_URL##*:}"

RELAY_PORT="${RELAY_PORT:-4040}"
WEBSITE_PORT="${WEBSITE_PORT:-9060}"

# Enclave -> Host: forward relay API and website to host
echo "relay:    :${RELAY_PORT} -> ${HOST_IP}:${RELAY_PORT}"
socat TCP-LISTEN:${RELAY_PORT},fork,reuseaddr TCP:${HOST_IP}:${RELAY_PORT} &

echo "website:  :${WEBSITE_PORT} -> ${HOST_IP}:${WEBSITE_PORT}"
socat TCP-LISTEN:${WEBSITE_PORT},fork,reuseaddr TCP:${HOST_IP}:${WEBSITE_PORT} &

# Host -> Enclave: reverse proxy beacon, blocksim, and postgres
echo "beacon:   :${BEACON_PORT} -> ${BEACON_HOST}:${BEACON_PORT}"
socat TCP-LISTEN:${BEACON_PORT},fork,reuseaddr TCP:${BEACON_HOST}:${BEACON_PORT} &

echo "blocksim: :${BLOCKSIM_PORT} -> ${BLOCKSIM_HOST}:${BLOCKSIM_PORT}"
socat TCP-LISTEN:${BLOCKSIM_PORT},fork,reuseaddr TCP:${BLOCKSIM_HOST}:${BLOCKSIM_PORT} &

echo "postgres: :${PG_PORT} -> ${PG_HOST}:${PG_PORT}"
socat TCP-LISTEN:${PG_PORT},fork,reuseaddr TCP:${PG_HOST}:${PG_PORT} &

echo "proxy ready"
wait
