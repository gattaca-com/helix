# Beacon Client

## Overview

This crate provides a modular interface for interacting with the consensus layer through Ethereum 2.0 beacon nodes and other 3rd party providers (BloXroute/ Fiber). In the context of the relay, the beacon-client keeps the current state up to date. For example, it is used to fetch the next set of proposer duties and keep list of all known validators up to date. The crate also includes multi-client support.

## Features

- **BeaconClient**
- **MultiBeaconClient**
- **BlockBroadcasterTrait**

## License

MIT OR Apache-2.0
