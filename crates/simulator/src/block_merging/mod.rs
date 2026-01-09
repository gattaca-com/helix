use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use alloy_consensus::{SignableTransaction, Transaction, TxEip1559};
use alloy_eips::{eip7685::RequestsOrHash, eip7840::BlobParams};
use alloy_primitives::{Address, B256, TxHash, U256, U512};
use alloy_rpc_types::{
    beacon::{relay::BidTrace, requests::ExecutionRequestsV4},
    engine::{
        CancunPayloadFields, ExecutionData, ExecutionPayload, ExecutionPayloadSidecar,
        ExecutionPayloadV3, PraguePayloadFields,
    },
};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use reth_ethereum::{
    Block, EthPrimitives,
    chainspec::EthChainSpec,
    evm::{
        EthEvmConfig,
        primitives::{
            Evm, EvmEnvFor, EvmError,
            block::{BlockExecutionError, BlockExecutor},
            execute::BlockBuilder as RethBlockBuilder,
        },
        revm::{cached::CachedReads, database::StateProviderDatabase},
    },
    provider::ChainSpecProvider,
    storage::{StateProvider, StateProviderFactory},
    trie::iter::{IntoParallelIterator, ParallelIterator},
};
use reth_node_builder::{
    Block as _, ConfigureEvm, NewPayloadError, NextBlockEnvAttributes, PayloadValidator,
};
use reth_primitives::{GotExpected, Recovered};
use revm::{
    DatabaseCommit, DatabaseRef, database::{CacheDB, State}
};
use tracing::{debug, info, warn};

pub(crate) use crate::block_merging::api::{BlockMergingApi, BlockMergingApiServer};
use crate::{
    block_merging::{
        error::BlockMergingApiError,
        types::{
            BlockMergeRequestV1, BlockMergeResponseV1, BuilderInclusionResult, DistributionConfig,
            MergeableOrderBytes, MergeableOrderRecovered, RecoveredTx, SignedTx, SimulatedOrder,
            SimulationError,
        },
    },
    common::CachedRethDb,
};

mod api;
mod error;
pub(crate) mod types;

impl BlockMergingApi {
    /// Core logic for appending additional transactions to a block.
    async fn _merge_block_v1(
        &self,
        request: BlockMergeRequestV1,
    ) -> Result<BlockMergeResponseV1, BlockMergingApiError> {
        let start_time = Instant::now();
        let base_block_hash = request.execution_payload.payload_inner.payload_inner.block_hash;
        info!(
            target: "rpc::relay::block_merging",
            block_hash=%base_block_hash,
            tx_count=%request.execution_payload.payload_inner.payload_inner.transactions.len(),
            proposer_value=%request.original_value,
            merging_data_count=%request.merging_data.len(),
            "Merging block v1",
        );
        let block: Block =
            request.execution_payload.try_into_block().map_err(NewPayloadError::Eth)?;

        let proposer_fee_recipient = request.proposer_fee_recipient;
        let gas_limit = block.gas_limit;
        let parent_beacon_block_root = request.parent_beacon_block_root;

        // The `merge_block` function is to avoid a lifetime leak that causes this
        // async fn to not be Send, which is required for spawning it.
        let (response, blob_versioned_hashes, request_cache) = self
            .merge_block(
                base_block_hash,
                request.original_value,
                proposer_fee_recipient,
                block,
                parent_beacon_block_root,
                request.merging_data,
                start_time,
            )
            .await?;

        debug!(
            target: "rpc::relay::block_merging",
            block_hash=%response.execution_payload.payload_inner.payload_inner.block_hash,
            tx_count=%response.execution_payload.payload_inner.payload_inner.transactions.len(),
            proposer_value=%response.proposer_value,
            appended_blob_count=%response.appended_blobs.len(),
            "Finished block merging",
        );

        let parent_hash = response.execution_payload.payload_inner.payload_inner.parent_hash;

        self.validation.update_cached_reads(parent_hash, request_cache).await;

        if self.validate_merged_blocks {
            let block_hash = response.execution_payload.payload_inner.payload_inner.block_hash;
            let gas_used = response.execution_payload.payload_inner.payload_inner.gas_used;

            let message = BidTrace {
                slot: 0, // unused
                parent_hash,
                block_hash,
                builder_pubkey: Default::default(),  // unused
                proposer_pubkey: Default::default(), // unused
                proposer_fee_recipient,
                gas_limit,
                gas_used,
                value: response.proposer_value,
            };
            let block = self
                .validation
                .payload_validator
                .ensure_well_formed_payload(ExecutionData {
                    payload: ExecutionPayload::V3(response.execution_payload.clone()),
                    sidecar: ExecutionPayloadSidecar::v4(
                        CancunPayloadFields {
                            parent_beacon_block_root,
                            versioned_hashes: blob_versioned_hashes,
                        },
                        PraguePayloadFields {
                            requests: RequestsOrHash::Requests(
                                response.execution_requests.to_requests(),
                            ),
                        },
                    ),
                })
                .inspect_err(|e| warn!(%e, "payload is not well formed"))?;

            self.validation
                .validate_message_against_block(block, message, 0, false, None)
                .await
                .inspect_err(|e| warn!(%e, "message is not valid against block"))?;
        }

        Ok(response)
    }

    /// Merge a block by appending mergeable orders.
    /// Returns the response with the block, the versioned hashes of the appended blobs,
    /// and the cached reads used during execution.
    async fn merge_block(
        &self,
        base_block_hash: B256,
        original_value: U256,
        proposer_fee_recipient: Address,
        base_block: Block,
        parent_beacon_block_root: B256,
        merging_data: Vec<MergeableOrderBytes>,
        start_time: Instant,
    ) -> Result<(BlockMergeResponseV1, Vec<B256>, CachedReads), BlockMergingApiError> {
        let validation = &self.validation;

        // Recover the base block transactions in parallel
        let (base_block, senders) = base_block
            .try_into_recovered()
            .map_err(|_| BlockMergingApiError::InvalidSignatureInBaseBlock)?
            .split();

        let (header, body) = base_block.split();

        let (withdrawals, transactions) = (body.withdrawals, body.transactions);

        let block_base_fee_per_gas = header.base_fee_per_gas.unwrap_or_default();

        let relay_fee_recipient = self.relay_fee_recipient;
        let beneficiary = header.beneficiary;

        let evm_config = validation.evm_config.clone().with_extra_data(header.extra_data);

        debug!(
            target: "rpc::relay::block_merging",
            parent=%header.parent_hash,
            %beneficiary,
            gas_limit=%header.gas_limit,
            gas_used=%header.gas_used,
            txs=%transactions.len(),
            "Started block merging",
        );

        // Check we have collateral for this builder
        let Some(types::PrivateKeySigner(signer)) =
            self.builder_collateral_map.get(&beneficiary).as_ref()
        else {
            return Err(BlockMergingApiError::NoSignerForBuilder(beneficiary));
        };

        // Check that block has proposer payment, otherwise reject it.
        // We don't remove it from the block, but add another payment transaction at the end.
        let Some(payment_tx) = transactions.last() else {
            return Err(BlockMergingApiError::MissingProposerPayment);
        };
        debug!(
            target: "rpc::relay::block_merging",
            to=?payment_tx.to(),
            value=%payment_tx.value(),
            tx_hash=%payment_tx.hash(),
            gas_limit=%payment_tx.gas_limit(),
            "Got proposer payment",
        );
        if payment_tx.value() != original_value || payment_tx.to() != Some(proposer_fee_recipient) {
            return Err(BlockMergingApiError::InvalidProposerPayment);
        }

        let payment_tx_gas_limit = payment_tx.gas_limit();

        // TODO: compute dynamically by keeping track of gas cost
        // Leave some gas for the final revenue distribution call
        // and the proposer payment.
        // The gas cost should be 10k per target, but could jump
        // to 35k if the targets are new accounts.
        // This number leaves us space for ~9 non-empty targets, or ~2 new accounts.
        // We also leave some extra gas for the proposer payment, ignoring the
        // intrinsic tx gas cost, since that's already covered.
        let distribution_gas_limit = 100000 + payment_tx_gas_limit - 21000;

        // Compute gas left in the base block
        let base_gas_left = header.gas_limit.saturating_sub(header.gas_used);
        if base_gas_left <= distribution_gas_limit {
            return Err(BlockMergingApiError::NotEnoughGasForPayment(base_gas_left));
        }
        // We already checked this doesn't underflow
        let gas_limit = header.gas_limit - distribution_gas_limit;

        let new_block_attrs = NextBlockEnvAttributes {
            timestamp: header.timestamp,
            suggested_fee_recipient: beneficiary,
            // mix_hash == prev_randao (source: https://eips.ethereum.org/EIPS/eip-4399)
            prev_randao: header.mix_hash,
            gas_limit: header.gas_limit,
            parent_beacon_block_root: Some(parent_beacon_block_root),
            withdrawals,
        };

        let parent_hash = header.parent_hash;

        let state_provider = validation.provider.state_by_block_hash(parent_hash)?;

        let mut request_cache = validation.cached_reads(parent_hash).await;

        let cached_db = request_cache.as_db(StateProviderDatabase::new(&state_provider));

        let mut state_db =
            State::builder().with_database_ref(&cached_db).with_bundle_update().build();

        let parent_header = validation.get_parent_header(parent_hash)?;

        // Execute the base block
        let evm_env = evm_config
            .next_evm_env(&parent_header, &new_block_attrs)
            .or(Err(BlockMergingApiError::NextEvmEnvFail))?;

        let evm = evm_config.evm_with_env(&mut state_db, evm_env.clone());
        let ctx = evm_config
            .context_for_next_block(&parent_header, new_block_attrs.clone())
            .or(Err(BlockMergingApiError::BlockContext))?;

        let block_builder = evm_config.create_block_builder(evm, &parent_header, ctx);

        let mut builder = BlockBuilder::new(evm_config.clone(), evm_env, block_builder, gas_limit);

        // Pair the transactions with the precomputed senders
        let recovered_txs = transactions
            .into_iter()
            .zip(senders)
            .map(|(tx, sender)| RecoveredTx::new_unchecked(tx, sender));

        self.merging_metrics.prep_to_execute_us.record(start_time.elapsed());

        builder.execute_base_block(recovered_txs)?;
        let start_time = Instant::now();

        let base_block_tx_count = builder.tx_hashes.len();
        debug!(
            target: "rpc::relay::block_merging",
            tx_count=%base_block_tx_count,
            gas_used=%builder.gas_used,
            "Finished executing base block",
        );

        self.merging_metrics.execute_base_block.record(start_time.elapsed());
        let start_time = Instant::now();

        let recovered_orders: Vec<MergeableOrderRecovered> =
            merging_data.into_par_iter().filter_map(|order| {
                match order.recover() {
                    Ok(tx) => Some(tx),
                    Err(e) => {
                        debug!(target: "rpc::relay::block_merging", %e, "Error recovering mergeable order");
                        None
                    },
                }
            }).collect();

        debug!(
            target: "rpc::relay::block_merging",
            count=%recovered_orders.len(),
            "Finished recovering mergeable orders",
        );

        // TODO: parallelize simulation
        // For this we need to consolidate `State` and wrap our database in a thread-safe cache.
        let mut simulated_orders: Vec<SimulatedOrder> = recovered_orders
            .into_iter()
            .filter_map(|order| match builder.simulate_order(order) {
                Ok(simulated_order) => Some(simulated_order),
                Err(e) => {
                    debug!(target: "rpc::relay::block_merging", %e, "Error simulating order");
                    None
                }
            })
            .collect();

        debug!(
            target: "rpc::relay::block_merging",
            count=%simulated_orders.len(),
            "Finished simulating orders",
        );

        self.merging_metrics.execute_merge_orders.record(start_time.elapsed());
        let start_time = Instant::now();

        // Sort orders by revenue, in descending order
        simulated_orders.sort_unstable_by(|o1, o2| o2.builder_payment.cmp(&o1.builder_payment));
        debug!(target: "rpc::relay::block_merging", "Finished sorting orders");

        let initial_builder_balance = get_balance_or_zero(builder.get_state(), beneficiary)?;

        // Simulate orders until we run out of block gas
        let revenues = append_greedily_until_gas_limit(&mut builder, simulated_orders)?;

        let number_of_appended_txs = builder.tx_hashes.len() - base_block_tx_count;
        debug!(target: "rpc::relay::block_merging", %number_of_appended_txs, "Finished appending orders");

        let final_builder_balance = get_balance_or_zero(builder.get_state(), beneficiary)?;

        let total_revenue: U256 = revenues.values().map(|v| v.revenue).sum();
        let builder_balance_delta = final_builder_balance.saturating_sub(initial_builder_balance);

        // Sanity check the sum of revenues is equal to the builder balance delta
        if total_revenue != builder_balance_delta {
            return Err(BlockMergingApiError::BuilderBalanceDeltaMismatch(GotExpected {
                expected: total_revenue,
                got: builder_balance_delta,
            }));
        }

        let estimated_payment_cost =
            U256::from(block_base_fee_per_gas).saturating_mul(U256::from(distribution_gas_limit));

        if total_revenue <= estimated_payment_cost {
            return Err(BlockMergingApiError::ZeroMergedBlockRevenue);
        }

        let updated_revenues = prepare_revenues(
            &self.distribution_config,
            &revenues,
            estimated_payment_cost,
            proposer_fee_recipient,
            relay_fee_recipient,
            beneficiary,
        );
        let proposer_added_value =
            updated_revenues.get(&proposer_fee_recipient).cloned().unwrap_or_default();

        let winning_builder_revenue = total_revenue
            .saturating_sub(updated_revenues.values().sum())
            .saturating_sub(estimated_payment_cost);

        debug!(
            target: "rpc::relay::block_merging",
            %total_revenue,
            %proposer_added_value,
            estimated_winning_builder_revenue=%winning_builder_revenue,
            "Finished processing revenue distribution",
        );

        // Sanity check. The winning builder gets something.
        // This is already indirectly checked in `prepare_revenues`.
        if winning_builder_revenue.is_zero() {
            return Err(BlockMergingApiError::ZeroRevenueForWinningBuilder);
        }

        self.append_payment_tx(
            &mut builder,
            signer,
            &updated_revenues,
            distribution_gas_limit,
            block_base_fee_per_gas.into(),
        )?;

        debug!(
            target: "rpc::relay::block_merging",
            "Finished appending payment tx",
        );

        let built_block = builder.finish(&state_provider)?;

        self.merging_metrics.finish.record(start_time.elapsed());

        let response = BlockMergeResponseV1 {
            base_block_hash,
            execution_payload: built_block.execution_payload,
            execution_requests: built_block.execution_requests,
            appended_blobs: built_block.appended_blob_versioned_hashes,
            proposer_value: proposer_added_value + original_value,
            builder_inclusions: revenues,
        };
        Ok((response, built_block.blob_versioned_hashes, request_cache))
    }

    fn append_payment_tx<'a, BB, Ex, Ev>(
        &self,
        builder: &mut BlockBuilder<BB>,
        signer: &PrivateKeySigner,
        updated_revenues: &HashMap<Address, U256>,
        distribution_gas_limit: u64,
        block_base_fee_per_gas: u128,
    ) -> Result<(), BlockMergingApiError>
    where
        BB: RethBlockBuilder<Primitives = EthPrimitives, Executor = Ex>,
        Ex: BlockExecutor<Transaction = SignedTx, Evm = Ev> + 'a,
        Ev: Evm<DB = &'a mut CachedRethDb<'a>> + 'a,
    {
        info!(target: "rpc::relay::block_merging", ?updated_revenues, "Preparing to append payment tx");
        let distributed_value: U256 = updated_revenues.values().sum();
        let calldata = encode_disperse_eth_calldata(updated_revenues);

        // Get the chain ID from the configured provider
        let chain_id = self.validation.provider.chain_spec().chain_id();

        let signer_address = signer.address();

        let Some(signer_info) = builder.get_state().basic_ref(signer_address)? else {
            return Err(BlockMergingApiError::EmptyBuilderSignerAccount(signer_address));
        };

        // Check there's enough balance for the payment.
        if signer_info.balance < distributed_value {
            return Err(BlockMergingApiError::NoBalanceInBuilderSigner {
                address: signer_address,
                current: signer_info.balance,
                required: distributed_value,
            });
        }
        let nonce = signer_info.nonce;

        let disperse_tx = TxEip1559 {
            chain_id,
            nonce,
            gas_limit: distribution_gas_limit,
            max_fee_per_gas: block_base_fee_per_gas,
            max_priority_fee_per_gas: 0,
            to: self.disperse_address.into(),
            value: distributed_value,
            access_list: Default::default(),
            input: calldata.into(),
        };
        info!(target: "rpc::relay::block_merging", ?disperse_tx, "Signing payment tx");

        let signed_disperse_tx = sign_transaction(signer, disperse_tx)?;

        // Execute the disperse transaction
        let is_success = builder.append_transaction(signed_disperse_tx)?;

        if !is_success {
            return Err(BlockMergingApiError::RevenueAllocationReverted);
        }

        Ok(())
    }
}

fn sign_transaction(
    signer: &PrivateKeySigner,
    tx: TxEip1559,
) -> Result<RecoveredTx, BlockMergingApiError> {
    let signature = signer
        .sign_hash_sync(&tx.signature_hash())
        .expect("signer is local and private key is valid");
    let signed_tx: SignedTx = tx.into_signed(signature).into();
    let recovered_signed_tx = Recovered::new_unchecked(signed_tx, signer.address());
    Ok(recovered_signed_tx)
}

/// Encodes a call to `disperseEther(address[],uint256[])` with the given recipients and values.
pub(crate) fn encode_disperse_eth_calldata(value_by_recipient: &HashMap<Address, U256>) -> Vec<u8> {
    sol! {
        function disperseEther(address[] recipients, uint256[] values) external payable;
    }

    let (recipients, values) = value_by_recipient.iter().unzip();

    disperseEtherCall { recipients, values }.abi_encode()
}

/// Computes revenue distribution, splitting merged block revenue
/// to the multiple participants. This also takes into account the
/// estimated payment cost, by subtracting it from the revenue.
///
/// Returns a map from address to value that should be sent to that
/// address.
pub(crate) fn prepare_revenues(
    distribution_config: &DistributionConfig,
    revenues: &HashMap<Address, BuilderInclusionResult>,
    estimated_payment_cost: U256,
    proposer_fee_recipient: Address,
    relay_fee_recipient: Address,
    block_beneficiary: Address,
) -> HashMap<Address, U256> {
    let mut updated_revenues = HashMap::with_capacity(revenues.len() + 1);

    let total_revenue: U256 = revenues.values().map(|v| v.revenue).sum();
    // Subtract the payment cost from the revenue
    let expected_revenue = total_revenue - estimated_payment_cost;

    // Compute the proposer revenue from the total revenue, to avoid rounding errors
    let proposer_revenue = distribution_config.proposer_split(expected_revenue);
    updated_revenues
        .entry(proposer_fee_recipient)
        .and_modify(|v| *v += proposer_revenue)
        .or_insert(proposer_revenue);

    // Compute the relay revenue from the total revenue, to avoid rounding errors
    let relay_revenue = distribution_config.relay_split(expected_revenue);
    updated_revenues
        .entry(relay_fee_recipient)
        .and_modify(|v| *v += relay_revenue)
        .or_insert(relay_revenue);

    // We assume the winning builder controls the beneficiary address, receiving
    // any undistributed revenue, and so don't allocate to it explicitly.

    // We divide the revenue among the different bundle origins.
    for (origin, origin_revenue) in revenues {
        // Update the revenue, subtracting part of the payment cost
        let actualized_revenue = (origin_revenue.revenue.widening_mul(expected_revenue) /
            U512::from(total_revenue))
        .to();
        let builder_revenue = distribution_config.merged_builder_split(actualized_revenue);
        updated_revenues
            .entry(*origin)
            .and_modify(|v| *v += builder_revenue)
            .or_insert(builder_revenue);
    }

    // Just in case, we remove the beneficiary address from the distribution
    updated_revenues.remove(&block_beneficiary);

    updated_revenues
}

struct BlockBuilder<BB> {
    block_builder: BB,

    // We need these to simulate orders
    evm_config: EthEvmConfig,
    evm_env: EvmEnvFor<EthEvmConfig>,
    blob_params: BlobParams,

    // Block builder keeps track of gas used, but it doesn't expose it
    // so we need to track it ourselves.
    gas_used: u64,
    // We use a custom gas limit, lower than the block gas limit,
    // to leave some gas for the final distribution and proposer payment txs.
    gas_limit: u64,

    /// Transaction hashes for the transactions in the block.
    tx_hashes: HashSet<TxHash>,
    /// Blob versioned hashes for the transactions in the block, including
    /// those in [Self::appended_blob_versioned_hashes].
    /// Used for optional block validation.
    blob_versioned_hashes: Vec<B256>,
    /// Blob versioned hashes for the transactions that were appended.
    appended_blob_versioned_hashes: Vec<B256>,
}

impl<'a, BB, Ex, Ev> BlockBuilder<BB>
where
    BB: RethBlockBuilder<Primitives = EthPrimitives, Executor = Ex>,
    Ex: BlockExecutor<Transaction = SignedTx, Evm = Ev> + 'a,
    Ev: Evm<DB = &'a mut CachedRethDb<'a>> + 'a,
{
    fn new(
        evm_config: EthEvmConfig,
        evm_env: EvmEnvFor<EthEvmConfig>,
        block_builder: BB,
        gas_limit: u64,
    ) -> Self {
        let timestamp: u64 =
            evm_env.block_env.timestamp.try_into().expect("all unix timestamps fit in an u64");
        let blob_params = evm_config
            .chain_spec()
            .blob_params_at_timestamp(timestamp)
            .expect("we are past Cancun");
        Self {
            block_builder,
            evm_config,
            evm_env,
            blob_params,
            gas_used: 0,
            gas_limit,
            tx_hashes: Default::default(),
            blob_versioned_hashes: Default::default(),
            appended_blob_versioned_hashes: Default::default(),
        }
    }

    fn execute_base_block(
        &mut self,
        txs: impl ExactSizeIterator<Item = RecoveredTx>,
    ) -> Result<(), BlockExecutionError> {
        self.block_builder.apply_pre_execution_changes()?;

        // Keep track of already applied txs, to discard duplicates
        self.tx_hashes = HashSet::with_capacity(txs.len());

        // Insert the transactions from the unmerged block
        for tx in txs {
            self.tx_hashes.insert(*tx.tx_hash());

            if let Some(versioned_hashes) = tx.blob_versioned_hashes() {
                self.blob_versioned_hashes.extend(versioned_hashes);
            }
            self.gas_used += self.block_builder.execute_transaction(tx)?;
        }

        Ok(())
    }

    fn get_state(&self) -> &CachedRethDb<'a> {
        self.block_builder.executor().evm().db()
    }

    fn was_already_applied(&self, tx_hash: &TxHash) -> bool {
        self.tx_hashes.contains(tx_hash)
    }

    fn simulate_order(
        &self,
        order: MergeableOrderRecovered,
    ) -> Result<SimulatedOrder, SimulationError> {
        let dropping_txs = order.dropping_txs();

        // Check for undroppable duplicate transactions
        let any_duplicate_undroppable_txs = order.transactions().iter().enumerate().any(|(i, tx)| {
            let hash = tx.tx_hash();
            let is_duplicate_undroppable_tx = self.was_already_applied(hash) && !dropping_txs.contains(&i);
            if is_duplicate_undroppable_tx {
                debug!(target: "rpc::relay::block_merging", %hash, "Duplicate undroppable transaction");
            }
            is_duplicate_undroppable_tx
        });

        if any_duplicate_undroppable_txs {
            return Err(SimulationError::DuplicateTransaction);
        }

        let available_gas = self.gas_limit - self.gas_used;
        let available_blobs =
            self.blob_params.max_blob_count - self.blob_versioned_hashes.len() as u64;

        let evm_env = self.evm_env.clone();
        let state = self.get_state();
        let simulated_order = simulate_order(
            &self.evm_config,
            state,
            evm_env,
            order,
            available_gas,
            available_blobs,
        )?;
        // Check the order has some revenue
        if simulated_order.builder_payment.is_zero() {
            debug!(target: "rpc::relay::block_merging", ?simulated_order, "Doesn't add value?");
            return Err(SimulationError::ZeroBuilderPayment);
        }
        // Check we have enough gas to include the order
        if self.gas_used + simulated_order.gas_used > self.gas_limit {
            return Err(SimulationError::OutOfBlockGas);
        }
        Ok(simulated_order)
    }

    fn append_transaction(&mut self, tx: RecoveredTx) -> Result<bool, BlockMergingApiError> {
        let mut is_success = false;
        let blobs_available =
            self.blob_params.max_blob_count - self.blob_versioned_hashes.len() as u64;
        // NOTE: we check this because the block builder doesn't seem to do it
        if tx.blob_count().unwrap_or(0) > blobs_available {
            return Err(BlockMergingApiError::BlobLimitReached);
        }
        self.gas_used += self
            .block_builder
            .execute_transaction_with_result_closure(tx.clone(), |r| is_success = r.is_success())?;

        self.tx_hashes.insert(*tx.tx_hash());
        // If tx has blobs, store the order index and tx sub-index to add the blobs to the payload
        // Also store the versioned hash for validation
        if let Some(versioned_hashes) = tx.blob_versioned_hashes() {
            self.blob_versioned_hashes.extend(versioned_hashes);
            self.appended_blob_versioned_hashes.extend(versioned_hashes);
        }
        Ok(is_success)
    }

    fn finish(
        self,
        state_provider: &dyn StateProvider,
    ) -> Result<BuiltBlock, BlockMergingApiError> {
        let blob_versioned_hashes = self.blob_versioned_hashes;
        let appended_blob_versioned_hashes = self.appended_blob_versioned_hashes;

        let outcome = self.block_builder.finish(state_provider)?;
        let execution_requests = outcome
            .execution_result
            .requests
            .try_into()
            .or(Err(BlockMergingApiError::ExecutionRequests))?;

        let sealed_block = outcome.block.into_sealed_block();
        let block_hash = sealed_block.hash();
        let block = sealed_block.into_block().into_ethereum_block();

        let execution_payload = ExecutionPayloadV3::from_block_unchecked(block_hash, &block);

        let result = BuiltBlock {
            execution_payload,
            execution_requests,
            blob_versioned_hashes,
            appended_blob_versioned_hashes,
        };
        Ok(result)
    }
}

struct BuiltBlock {
    execution_payload: ExecutionPayloadV3,
    execution_requests: ExecutionRequestsV4,
    /// Versioned hashes for the whole block
    blob_versioned_hashes: Vec<B256>,
    /// Versioned hashes for only the appended blobs
    appended_blob_versioned_hashes: Vec<B256>,
}

fn append_greedily_until_gas_limit<'a, BB, Ex, Ev>(
    builder: &mut BlockBuilder<BB>,
    simulated_orders: Vec<SimulatedOrder>,
) -> Result<HashMap<Address, BuilderInclusionResult>, BlockMergingApiError>
where
    BB: RethBlockBuilder<Primitives = EthPrimitives, Executor = Ex>,
    Ex: BlockExecutor<Transaction = SignedTx, Evm = Ev> + 'a,
    Ev: Evm<DB = &'a mut CachedRethDb<'a>> + 'a,
{
    let mut revenues = HashMap::new();

    // Append transactions by score until we run out of space
    for simulated_order in simulated_orders {
        let order = simulated_order.order;
        let origin = *order.origin();

        let Ok(simulated_order) = builder.simulate_order(order) else {
            continue;
        };

        let SimulatedOrder { order, should_be_included, builder_payment, .. } = simulated_order;

        // Append the bundle
        
        // We can't avoid re-execution here due to the BlockBuilder API
        let transactions = order.into_transactions();
        let mut txs = Vec::with_capacity(transactions.len());
        for (tx, _) in transactions.into_iter().zip(should_be_included).filter(|(_, sbi)| *sbi)
            
        {
            txs.push(*tx.tx_hash());
            builder.append_transaction(tx)?;
        }

        // Update the revenue for the bundle's origin
        revenues
            .entry(origin)
            .and_modify(|v: &mut BuilderInclusionResult| {
                v.revenue += builder_payment;
                v.txs.extend(txs.clone());
            })
            .or_insert(BuilderInclusionResult { revenue: builder_payment, txs: txs } );
    }
    Ok(revenues)
}

/// Simulates an order.
/// Returns whether the order is valid, the amount of gas used, and a list
/// marking whether to include a transaction of the order or not.
fn simulate_order<DBRef>(
    evm_config: &EthEvmConfig,
    db_ref: DBRef,
    evm_env: EvmEnvFor<EthEvmConfig>,
    order: MergeableOrderRecovered,
    available_gas: u64,
    available_blobs: u64,
) -> Result<SimulatedOrder, SimulationError>
where
    DBRef: DatabaseRef + core::fmt::Debug,
    DBRef::Error: Send + Sync + 'static,
    SimulationError: From<DBRef::Error>,
{
    // Wrap current state in cache to avoid mutating it
    let cached_db = CacheDB::new(db_ref);
    // Create a new EVM with the pre-state
    let mut evm = evm_config.evm_with_env(cached_db, evm_env);
    let initial_balance = get_balance_or_zero(evm.db(), evm.block.beneficiary)?;

    let txs = order.transactions();
    let reverting_txs = order.reverting_txs();
    let dropping_txs = order.dropping_txs();

    let mut gas_used = 0;
    let mut blobs_added = 0;
    let mut included_txs = vec![true; txs.len()];

    // Check the bundle can be included in the block
    for (i, tx) in txs.iter().enumerate() {
        let can_be_dropped = dropping_txs.contains(&i);
        let can_revert = reverting_txs.contains(&i);
        // If tx takes too much gas, try to drop it or fail
        if tx.gas_limit() > (available_gas - gas_used) {
            if !can_be_dropped {
                return Err(SimulationError::OutOfBlockGas);
            }
            included_txs[i] = false;
            continue;
        }
        // If tx exceeds blob limit, try to drop it or fail
        if tx.blob_count().unwrap_or(0) > (available_blobs - blobs_added) {
            if !can_be_dropped {
                return Err(SimulationError::OutOfBlockBlobs);
            }
            included_txs[i] = false;
            continue;
        }
        // Execute transaction
        match evm.transact(tx) {
            Ok(result) => {
                if result.result.is_success() || can_revert {
                    gas_used += result.result.gas_used();
                    blobs_added += tx.blob_count().unwrap_or(0);
                    // Apply the state changes to the simulated state
                    // Note that this only commits to the cache wrapper, not the underlying database
                    evm.db_mut().commit(result.state);
                } else {
                    // If tx reverted and is not allowed to, we check if it
                    // can be dropped instead, else we discard this bundle.
                    if can_be_dropped {
                        // Tx should be dropped
                        included_txs[i] = false;
                    } else {
                        return Err(SimulationError::RevertNotAllowed(i));
                    }
                }
            }
            Err(e) => {
                if e.is_invalid_tx_err() && (can_be_dropped || can_revert) {
                    // The transaction might have been invalidated by another one, so we drop it
                    included_txs[i] = false;
                } else {
                    // The error isn't transaction-related or tx can't be dropped, so we just drop
                    // this bundle
                    return Err(SimulationError::DropNotAllowed(i));
                }
            }
        };
    }
    let final_balance = get_balance_or_zero(evm.db(), evm.block.beneficiary)?;
    let builder_payment = final_balance.saturating_sub(initial_balance);
    Ok(SimulatedOrder { order, gas_used, should_be_included: included_txs, builder_payment })
}

fn get_balance_or_zero<DB: DatabaseRef>(
    db: DB,
    address: Address,
) -> Result<U256, <DB as DatabaseRef>::Error> {
    Ok(db.basic_ref(address)?.map_or(U256::ZERO, |info| info.balance))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn test_prepare_revenues() {
        let distribution_config = DistributionConfig {
            relay_bps: 2500,
            merged_builder_bps: 2500,
            winning_builder_bps: 2500,
        };
        let winning_builder_fee_recipient = address!("0x0000000000000000000000000000000000000001");
        let relay_fee_recipient = address!("0x0000000000000000000000000000000000000002");
        let proposer_fee_recipient = address!("0x0000000000000000000000000000000000000003");

        let addresses = vec![
            address!("0x0000000000000000000000000000000000000006"),
            address!("0x0000000000000000000000000000000000000007"),
        ];
        let values = vec![U256::from(10000), U256::from(30000)];

        let revenues = HashMap::from_iter(addresses.iter().cloned().zip(
            values.iter().cloned().map(|v| BuilderInclusionResult { revenue: v, txs: vec![] }),
        ));
        let updated_revenues = prepare_revenues(
            &distribution_config,
            &revenues,
            U256::ZERO,
            proposer_fee_recipient,
            relay_fee_recipient,
            winning_builder_fee_recipient,
        );

        // Check total for relay + proposer + builders
        assert_eq!(updated_revenues.values().sum::<U256>(), 30000);

        // Check relay got 1/4 the total sum
        assert_eq!(updated_revenues[&relay_fee_recipient], 10000);

        // Check each merging builder got 1/4 their contribution
        assert_eq!(updated_revenues[&addresses[0]], 2500);
        assert_eq!(updated_revenues[&addresses[1]], 7500);

        // Check proposer value is 1/4 the total sum
        assert_eq!(updated_revenues[&proposer_fee_recipient], 10000);

        // Check winning builder didn't get anything assigned,
        // since anything not allocated goes to them anyways
        assert!(!updated_revenues.contains_key(&winning_builder_fee_recipient));
    }

    #[test]
    fn test_prepare_revenues_with_small_values() {
        let distribution_config = DistributionConfig {
            relay_bps: 2500,
            merged_builder_bps: 2500,
            winning_builder_bps: 2500,
        };
        let winning_builder_fee_recipient = address!("0x0000000000000000000000000000000000000001");
        let relay_fee_recipient = address!("0x0000000000000000000000000000000000000002");
        let proposer_fee_recipient = address!("0x0000000000000000000000000000000000000003");

        let addresses = vec![
            address!("0x0000000000000000000000000000000000000006"),
            address!("0x0000000000000000000000000000000000000007"),
        ];
        let values = vec![U256::from(7), U256::from(5)];

        let revenues = HashMap::from_iter(addresses.iter().cloned().zip(
            values.iter().cloned().map(|v| BuilderInclusionResult { revenue: v, txs: vec![] }),
        ));
        let updated_revenues = prepare_revenues(
            &distribution_config,
            &revenues,
            U256::ZERO,
            proposer_fee_recipient,
            relay_fee_recipient,
            winning_builder_fee_recipient,
        );

        // Check total for relay + proposer + builders
        assert_eq!(updated_revenues.values().sum::<U256>(), 8);

        // Check proposer delta is 1/4 the total sum
        assert_eq!(updated_revenues[&proposer_fee_recipient], 3);

        // Check each merging builder got 1/4 their contribution
        assert_eq!(updated_revenues[&addresses[0]], 1);
        assert_eq!(updated_revenues[&addresses[1]], 1);

        // Check relay got 1/4 the total sum
        assert_eq!(updated_revenues[&relay_fee_recipient], 3);

        // Check winning builder didn't get anything assigned,
        // since anything not allocated goes to them anyways
        assert!(!updated_revenues.contains_key(&winning_builder_fee_recipient));
    }
}
