# Local Helix Relay Development with Kurtosis

Run the full MEV pipeline (beacon nodes, EL clients, builder, mev-boost, postgres) in Kurtosis while running the helix relay on your host machine. This lets you iterate on relay code with full debug support in VS Code while the rest of the stack runs in the background.

## How it works

A lightweight socat proxy container (`helix-relay-proxy`) replaces the real relay inside the Kurtosis enclave. It:

- Forwards builder and mev-boost traffic (ports 4040/9060) from the enclave to your host
- Reverse-proxies beacon, blocksim, and postgres from the enclave back to your host relay

No changes to the ethereum-package starlark code are needed — the proxy image is a drop-in replacement.

```
┌─ Kurtosis Enclave ──────────────────────────────┐
│                                                  │
│  builder ──► helix-relay-proxy ──► host:4040     │
│  mev-boost ──► (proxy)         ──► host:4040     │
│                                                  │
│  beacon ◄── (proxy):4000 ◄── host relay          │
│  blocksim ◄── (proxy):8545 ◄── host relay        │
│  postgres ◄── (proxy):5432 ◄── host relay        │
│                                                  │
└──────────────────────────────────────────────────┘
```

## Prerequisites

- [Kurtosis](https://docs.kurtosis.com/install/)
- [Docker](https://docs.docker.com/engine/install/)
- Rust toolchain (for building helix)
- VS Code with [CodeLLDB](https://marketplace.visualstudio.com/items?itemName=vadimcn.vscode-lldb) extension (for debugging)
- `ufw` access (sudo) — the script opens firewall ports for Docker-to-host traffic

## Setup

### 1. Start the devnet

```bash
./restart_devnet.sh
```

This will:
1. Build the `helix-relay-proxy` Docker image
2. Start a Kurtosis enclave with the full MEV stack from [gattaca-com/ethereum-package](https://github.com/gattaca-com/ethereum-package) (minus the real relay)
3. Open UFW firewall ports for Docker-to-host relay traffic (requires sudo)
4. Extract the proxy container's IP
5. Download genesis data
6. Generate a local helix config at `local-helix/config.yaml` with all addresses rewritten to point through the proxy
7. Write an env file at `local-helix/env.sh`

### 2. Run the relay

From a terminal:
```bash
source local-helix/env.sh
cargo run --bin helix-relay -- --config local-helix/config.yaml
```

Or from VS Code — add this to `.vscode/launch.json` in the helix repo:

```json
{
    "version": "0.2.0",
    "configurations": [
        {
            "type": "lldb",
            "request": "launch",
            "name": "devnet",
            "cargo": {
                "args": ["build", "--bin=helix-relay", "--package=helix-relay"],
                "filter": {
                    "name": "helix-relay",
                    "kind": "bin"
                }
            },
            "args": ["--config", "${workspaceFolder}/scripts/devnet/local-helix/config.yaml"],
            "env": {
                "RELAY_KEY": "0x607a11b45a7219cc61a3d9c5fd08c7eebd602a6a19a977f8d3771d5711a550f2",
                "POSTGRES_PASSWORD": "postgres",
                "ADMIN_TOKEN": "test",
                "RUST_LOG": "TRACE"
            },
            "cwd": "${workspaceFolder}"
        }
    ]
}
```

Then hit **F5** to build and debug.

### 3. Iterate

The enclave stays running in the background. Stop and restart the relay as many times as you want — rebuild, set breakpoints, change log levels, etc. The builder and mev-boost will reconnect automatically.

To tear down the enclave:
```bash
kurtosis enclave stop devnet && kurtosis enclave rm devnet
```

## Troubleshooting

### Proxy can't reach host (`Operation timed out` in proxy logs)

Check `kurtosis service logs devnet helix-relay`. If you see timeouts to the host IP, your firewall is blocking Docker-to-host traffic. The restart script handles this automatically, but if the subnet changed you may need to re-run or manually allow:

```bash
# Find the kurtosis network subnet
docker network inspect kt-devnet --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}'

# Allow it
sudo ufw allow from <subnet> to any port 4040
sudo ufw allow from <subnet> to any port 9060
```

### Config parsing errors

If the relay panics on startup with a missing config field, your local helix build is newer than the config template in the ethereum-package. Manually add the missing field to `local-helix/config.yaml`.

### Relay not receiving builder submissions

Check that:
1. The proxy is running: `kurtosis service logs devnet helix-relay`
2. The relay is listening on 4040: `ss -tlnp | grep 4040`
3. Firewall is open (see above)

### Stale enclave

If you change `mev-helix.yaml` or the proxy image, re-run `./restart_devnet.sh` to rebuild everything.
