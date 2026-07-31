# Data API

HTTP API exposing relay data for consumers such as dashboards and analytics. Routes are
served both by the standalone `data-api` binary and mounted directly on the relay under
`PATH_DATA_API` (`/relay/v1/data`).

## `GET /relay/v1/data/merged_blocks?slot=<slot>`

Returns every merged block delivered for a given slot.

### Response fields

| Field | Type | Meaning |
| --- | --- | --- |
| `slot` | `u64` (quoted) | Slot the merged block was delivered for. |
| `block_number` | `u64` (quoted) | Execution block number. |
| `original_block_hash` | hash | Hash of the original (unmerged) block the proposer would otherwise have received. |
| `block_hash` | hash | Hash of the final merged block delivered to the proposer. |
| `original_value` | `U256` (quoted) | Value of the original bid, before merging. |
| `proposer_value` | `U256` (quoted) | Total value paid to the proposer for the merged block (`original_value` plus the proposer's share of the merge uplift). **Not** the total value of the merged block. |
| `total_merged_value` | `U256` (quoted) | Total value added by the merged orders — sum of `contribution` across all `builder_inclusions` entries. |
| `base_builder_revenue` | `U256` (quoted) | Value paid to the base block builder for its share of the merge split. |
| `relay_revenue` | `U256` (quoted) | Value paid to the relay for its share of the merge split. |
| `original_tx_count` / `merged_tx_count` | `u64` (quoted) | Transaction counts before/after merging. |
| `original_blob_count` / `merged_blob_count` | `u64` (quoted) | Blob counts before/after merging. |
| `original_gas_used` / `merged_gas_used` | `u64` | Gas used before/after merging. |
| `builder_inclusions` | map, keyed by builder fee-recipient address | Per-builder breakdown of the merge. See below. |

### `builder_inclusions` entries

| Field | Type | Meaning |
| --- | --- | --- |
| `contribution` | `U256` (quoted) | Value merged in from this builder's block. |
| `revenue` | `U256` (quoted) | What this builder actually earned for its contribution, net of payout-tx gas — matches the on-chain split. |
| `txs` | list of tx hashes | Transactions merged in from this builder's block. |

### Working out the full split

`total_merged_value` gives you the gross value merged in without needing to sum
`builder_inclusions` yourself. To see who actually received what:

- **Proposer's share** = `proposer_value - original_value` (not a separate field —
  trivial to compute from the two).
- **Full split** across parties = proposer's share + `base_builder_revenue` +
  `relay_revenue` + sum of `revenue` across all `builder_inclusions` entries.

This avoids needing to track the relay's bps split configuration over time to work out
who received what — the dollar amounts are already broken out per party.

### Notes on rollout

`base_builder_revenue` and `relay_revenue` depend on the connected merge builder
supporting block-merging protocol v3 (`helix_tcp_types::merging::MERGING_PROTOCOL_VERSION`).
Against a builder still on v2, they'll read as zero. `total_merged_value` is always
correct — it's built from `contribution`, which was already present (under the name
`revenue`) before this protocol bump.

### A note on the `merged_value` DB column

The underlying Postgres column backing `proposer_value` is still named `merged_value`
(kept as-is to avoid a disruptive rename); the API and internal Rust types use the
clearer `proposer_value` name and the mapping is handled in the database layer.
