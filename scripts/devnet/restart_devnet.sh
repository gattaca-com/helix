#!/usr/bin/env bash
set -euo pipefail

ENCLAVE="devnet"
ARGS_FILE="mev-helix.yaml"
ETH_PKG="github.com/gattaca-com/ethereum-package"
LOCAL_DIR="$(cd "$(dirname "$0")" && pwd)"
OUTPUT_DIR="${LOCAL_DIR}/local-helix"
mkdir -p "$OUTPUT_DIR"

# Build proxy image
docker build -t helix-relay-proxy "$LOCAL_DIR"

# Restart enclave
kurtosis enclave stop "$ENCLAVE" 2>/dev/null || true
kurtosis enclave rm "$ENCLAVE" 2>/dev/null || true
kurtosis run --enclave "$ENCLAVE" "$ETH_PKG" --args-file "$ARGS_FILE"

# Allow kurtosis containers to reach the host relay
NETWORK_SUBNET=$(docker network inspect "kt-${ENCLAVE}" --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}')
sudo ufw allow from "$NETWORK_SUBNET" to any port 4040
sudo ufw allow from "$NETWORK_SUBNET" to any port 9060

# Get helix-relay proxy container ID
PROXY_CONTAINER=$(docker ps --filter "name=helix-relay" --format '{{.ID}}' | head -n 1)

if [ -z "$PROXY_CONTAINER" ]; then
    echo "ERROR: helix-relay proxy container not found or not running" >&2
    exit 1
fi

# Get IP in the kurtosis enclave network.
# Avoids IP concatenation when container is attached to multiple networks.
PROXY_IP=$(docker inspect "$PROXY_CONTAINER" \
    --format "{{(index .NetworkSettings.Networks \"kt-${ENCLAVE}\").IPAddress}}" 2>/dev/null || true)

if [ -z "$PROXY_IP" ]; then
    echo "ERROR: could not resolve IP for container $PROXY_CONTAINER" >&2
    exit 1
fi

# Extract env vars from the service
RELAY_KEY=$(kurtosis service inspect "$ENCLAVE" helix-relay 2>&1 | awk '/RELAY_KEY:/{print $2}')

# Download genesis data
rm -rf "${OUTPUT_DIR}/network-configs"
kurtosis files download "$ENCLAVE" el_cl_genesis_data "${OUTPUT_DIR}/network-configs"

# Pull rendered config and rewrite for local use
RAW_CONFIG=$(kurtosis service exec "$ENCLAVE" helix-relay "cat /config/config.yaml" 2>&1)
echo "$RAW_CONFIG" \
  | tail -n +2 \
  | sed \
    -e "s|dir_path: \"/network-configs/|dir_path: \"${OUTPUT_DIR}/network-configs/|" \
    -e "s|hostname: \"helix-relay-postgres\"|hostname: \"${PROXY_IP}\"|" \
    -e "s|http://cl-[^\"]*|http://${PROXY_IP}:4000|" \
    -e "s|http://el-[^\"]*|http://${PROXY_IP}:8545|" \
  > "${OUTPUT_DIR}/config.yaml"

# Write env file for convenience
cat > "${OUTPUT_DIR}/env.sh" <<EOF
export RELAY_KEY=${RELAY_KEY}
export POSTGRES_PASSWORD=postgres
export ADMIN_TOKEN=test
EOF

echo ""
echo "=== Devnet ready ==="
echo "Config: ${OUTPUT_DIR}/config.yaml"
echo "Env:    ${OUTPUT_DIR}/env.sh"
echo ""
echo "To run the relay:"
echo "  source ${OUTPUT_DIR}/env.sh"
echo "  cargo run --bin helix-relay -- --config ${OUTPUT_DIR}/config.yaml"
