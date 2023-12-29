# Helix 🧬

## Goals
Keeping with the core principles of PBS, one of the key goals for the relay architecture is to minimise the impact on staking rewards, independent of the validator's geographical location or level of technical sophistication. Additionally, we want to keep the operational costs of running a relay minimal, which is essential for maintaining a varied and robust relay ecosystem. A crucial aspect of this objective is optimising relay performance. By achieving ultra-low latency for neutral relays, we can diminish the necessity for builders to vertically integrate.

## Optimising Relay Flows
The PBS relay operates with two distinct flows, each having unique requirements:

- **submit_block -> get_header Flow:** This is currently the only flow where latency is a critical factor. Our focus is on minimizing latency, with redundancy being a secondary concern. It's important to note that future enhancements may involve hyper-optimizing the `get_header` and `get_payload` flows for latency, particularly if timing games become more prevalent.
- **get_header -> get_payload Flow:** The ability to deliver the payload promptly after a `get_header` request is crucial. Failure to do so risks the proposer missing the slot, making high redundancy in this flow vital.

## Current Challenges and Solutions

### Geo-Distribution
The current Flashbots relay [implementation](https://github.com/flashbots/mev-boost-relay) only allows running on a single cluster. This has resulted in relays aggregating in the areas with the highest concentration of validators, namely AWS data centres in North Virginia and Europe. This presents a significant disadvantage to validators running in locations with high latency to either of these locations. To avoid missed slots, validators must modify the MEV-Boost client to call `get_header` earlier, reducing MEV rewards and incentivising co-location with relays. To counter this, we've designed our relay to support geo-distribution, allowing multiple clusters running in different geo-locations to serve the relay API as a single relay.
- Our design supports multiple geo-distributed clusters under a single relay URL.
- Automatic call routing ensures the lowest latency communication to relays.
- We've addressed potential non-determinism, such as differing routes for `get_header` and `get_payload`, by implementing the `GossipClientTrait` using gRPC. This ensures payload availability across all clusters.
- We're preparing for a mainnet launch with clusters in AWS North Virginia, Frankfurt, Tokyo, and Singapore, accessible via [Titan Relay](https://docs.titanrelay.xyz). Currently, we're live on the Holesky testnet with clusters in FR and NV.

### Censoring/ Non-Censoring
- Running censoring and non-censoring relays separately doubles operational costs. We've integrated both functionalities into a single infrastructure to reduce overhead and streamline operations.
- We have unified the censoring and non-censoring relays into a single relay by allowing validators to specify “preferences” on-registration.
- To ensure ease of adoption we allow validators to communicate this binary decision by using two different URLs ([titanrelay.xyz](titanrelay.xyz) and [censoring.titanrelay.xyz](censoring.titanrelay.xyz)), registrations to either of these URLs will automatically set the preferences. All other calls for headers, payloads, etc. will be routed in exactly the same way.
- On the builder side, there's a requirement to adapt to these changes as the `validators` API response will now contain an extra “preferences” field. Small changes are also required to the internal logic, as censoring moves from a per-relay basis to a per-validator basis.
- The “preferences” field lays the groundwork for future "PEPC" type developments. For example, relay-established inclusion lists and trusted builder lists.

### Modular Design
- Emphasizing generic design, Helix allows for flexible integration with various databases and libraries. 
- Key Traits include: `Database`, `Auctioneer`, `Simulator` and `BeaconClient`.
- The current `Auctioneer` implementation supports redis, due to the ease of implementation when synchronising multiple processes in the same cluster. 
- The `Simulator` is also purposely generic, allowing for implementations of all optimistic relaying implementations and different forms of simulation. For example, communicating with Geth via RPC or IPC.

### Handling Missed Slots
- Similar to the current mev-boost-relay implementation, we include a one-second delay before returning unblinded payloads to the proposer. This delay is required to mitigate attack vectors such as the recent [low-carb-crusader](https://collective.flashbots.net/t/disclosure-mitigation-of-block-equivocation-strategy-with-early-getpayload-calls-for-proposers/1705) attack. Consequently, the relay takes on the critical role of beacon-chain block propagation.
- To ensure efficient beacon-chain block propagation, without the need for sophisticated beacon client peering, we've integrated a `Broadcaster` Trait, allowing integration with network services like [Fiber](https://fiber.chainbound.io) and [BloXroute](https://bloxroute.com) for effective payload distribution. 

## Latency Analysis and Future Optimisations

TODO: add latency images

Analysing the latency metrics presented, we observe significant latency spikes in two distinct segments: `Receive -> Decode` and `Simulation -> Auctioneer`.

In the `Receive -> Decode` phase, the primary sources of latency can be attributed to the handling of incoming byte streams and their subsequent deserialization into the SignedBidSubmission structure. This latency is primarily incurred due to:

- Byte Stream Processing: The initial step involves reading the incoming byte stream into a memory buffer, specifically using `hyper::body::to_bytes(body)`. This step is crucial as it translates raw network data into a usable byte vector, and its latency is influenced by the size of the incoming data and the efficiency of the network I/O operations.
- GZIP Decompression: For compressed payloads, the GZIP decompression process can introduce significant computational overhead, especially for larger payloads.
- Deserialization Overhead: This is the final deserialization step, where the byte vector is converted into a `SignedBidSubmission` object using either SSZ or JSON.

In the `Simulation -> Auctioneer` section we store all necessary information about the payload, preparing it to be returned by `get_header` and `get_payload`. In the current implementation this is handled using redis which can introduce significant latency especially for larger payloads.

It is worth mentioning that all submissions during this period were simulated optimistically, if this wasn’t the case we would see the majority of the latency being taken up by `Bid checks -> Simulation`.

### OptimisticV2
[OptimisticV2](https://frontier.tech/optimistic-relays-and-where-to-find-them) introduces an architectural change where the lightweight header (always less than 1 [MTU](https://www.cloudflare.com/en-gb/learning/network-layer/what-is-mtu)) is decoupled from the much heavier payload. Due to the much smaller size, the header can be quickly downloaded, deserialized and saved, ready for `get_header` responses. Meanwhile, the much heavier full `SignedBidSubmission` is downloaded and verified asynchronously.

We plan to implement two distinct endpoints for builders: `submit_header` and `submit_payload`. Builders will be responsible for ensuring that they only use these endpoints if their collateral covers the block value and that they submit payloads in a timely manner to the relay. Builders that fail to submit payloads will have their collateral slashed in the same process as the current Optimistic V1 implementation.

Along with reducing the internal latency, separating the header and payload will drastically reduce the network latency. We should see end-to-end latencies reduce even further than the values shown in the graphs.

### In-Memory Auctioneer
In multi-relay cluster configurations, utilised for load balancing and redundancy, synchronising the best bid across all nodes is crucial to minimise redundant processing. Currently, this synchronisation relies on Redis. While the current `RedisCache` implementation could be optimised further, we plan on shifting to an in-memory model for our Auctioneer component, eliminating the reliance on Redis for critical-path functions.

Our approach will separate `Auctioneer` functionalities based on if they lie in the critical path. Non-critical path functions will continue to use Redis for synchronisation and redundancy. However, critical-path operations like `get_last_slot_delivered` and `check_if_bid_is_below_floor` will be moved in-memory. To ensure that we minimise redundant processing, each header update will be gossiped between local instances.

It is worth mentioning that the majority of the critical path auctioneer latency will be removed with OptimisticV2 as the majority of the Redis calls will be done in the `submit_payload` flow. 

## Credit to:
Flashbots:
- https://github.com/flashbots/mev-boost-relay

Alex Stokes. A lot of the types used are derived/ taken from these repos:
- https://github.com/ralexstokes/ethereum-consensus
- https://github.com/ralexstokes/mev-rs
- https://github.com/ralexstokes/beacon-api-client

## License
MIT OR Apache-2.0

## Contact
- https://twitter.com/titanbuilderxyz
