pub(crate) mod error;

use std::{collections::HashSet, fmt::Debug, sync::Arc, time::Duration};

use alloy_consensus::{BlockHeader, EnvKzgSettings, Transaction, TxReceipt};
use alloy_eips::{eip4844::kzg_to_versioned_hash, eip7685::RequestsOrHash};
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::{
    beacon::relay::{
        BidTrace, BuilderBlockValidationRequest, BuilderBlockValidationRequestV2,
        BuilderBlockValidationRequestV3, BuilderBlockValidationRequestV4,
        BuilderBlockValidationRequestV5,
    },
    engine::{
        BlobsBundleV1, BlobsBundleV2, CancunPayloadFields, ExecutionData, ExecutionPayload,
        ExecutionPayloadSidecar, PraguePayloadFields,
    },
};
use async_trait::async_trait;
use dashmap::DashSet;
use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use reth_ethereum::{
    Block, EthPrimitives, Receipt, TransactionSigned,
    consensus::{ConsensusError, FullConsensus, validation::MAX_RLP_BLOCK_SIZE},
    evm::{
        primitives::{Evm, execute::Executor},
        revm::{cached::CachedReads, database::StateProviderDatabase},
    },
    node::{EthereumEngineValidator, core::rpc::result::internal_rpc_err},
    primitives::{
        GotExpected, RecoveredBlock, SealedBlock, SealedHeaderFor,
        constants::GAS_LIMIT_BOUND_DIVISOR,
    },
    provider::{BlockExecutionOutput, ChainSpecProvider},
    rpc::eth::utils::recover_raw_transaction,
    storage::{BlockReaderIdExt, HeaderProvider, StateProviderFactory},
};
use reth_metrics::{Metrics, metrics::Gauge};
use reth_node_builder::{BlockBody, ConfigureEvm, PayloadValidator};
use reth_primitives::EthereumHardforks;
use reth_tasks::TaskExecutor;
use revm::{Database, database::State};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tokio::{
    spawn,
    sync::{RwLock, oneshot},
    time,
};
use tracing::{info, warn};

use crate::{
    common::{RethConsensus, RethPayloadValidator, RethProvider},
    inclusion::types::InclusionList,
    validation::error::{GetParentError, ValidationApiError},
};

/// The type that implements the `validation` rpc namespace trait
#[derive(Clone, derive_more::Deref)]
pub struct ValidationApi {
    #[deref]
    inner: Arc<ValidationApiInner>,
}

impl ValidationApi {
    /// Create a new instance of the [`ValidationApi`]
    pub fn new(
        provider: RethProvider,
        consensus: Arc<RethConsensus>,
        evm_config: reth_ethereum::evm::EthEvmConfig,
        config: ValidationApiConfig,
        task_spawner: Box<TaskExecutor>,
        payload_validator: Arc<EthereumEngineValidator>,
    ) -> Self {
        let ValidationApiConfig { blacklist_endpoint, validation_window } = config;
        let disallow = Arc::new(DashSet::new());

        let inner = Arc::new(ValidationApiInner {
            provider,
            consensus,
            payload_validator,
            evm_config,
            disallow: disallow.clone(),
            validation_window,
            cached_state: Default::default(),
            task_spawner,
            metrics: Default::default(),
        });

        inner.metrics.disallow_size.set(inner.disallow.len() as f64);

        // spawn background updater task
        let client = reqwest::Client::new();
        let ep = blacklist_endpoint.clone();
        let dash = disallow.clone();
        let gauge = inner.metrics.disallow_size.clone();
        spawn(async move {
            let mut interval = time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                match client.get(&ep).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(list) = resp.json::<Vec<String>>().await {
                            // build new set then swap
                            dash.clear();
                            for hex in list {
                                if let Ok(b) =
                                    hex.strip_prefix("0x").unwrap_or(&hex).parse::<B256>()
                                {
                                    dash.insert(Address::from_slice(b.as_slice()));
                                }
                            }
                            gauge.set(dash.len() as f64);
                        }
                    }
                    Ok(r) => warn!("Blacklist fetch failed: HTTP {}", r.status()),
                    Err(e) => warn!("Blacklist fetch error: {}", e),
                }
            }
        });

        Self { inner }
    }

    /// Returns the cached reads for the given head hash.
    pub(crate) async fn cached_reads(&self, head: B256) -> CachedReads {
        let cache = self.inner.cached_state.read().await;
        if cache.0 == head { cache.1.clone() } else { Default::default() }
    }

    /// Updates the cached state for the given head hash.
    pub(crate) async fn update_cached_reads(&self, head: B256, cached_state: CachedReads) {
        let mut cache = self.inner.cached_state.write().await;
        if cache.0 == head {
            cache.1.extend(cached_state);
        } else {
            *cache = (head, cached_state)
        }
    }
}

impl ValidationApi {
    /// Validates the given block and a [`BidTrace`] against it.
    pub async fn validate_message_against_block(
        &self,
        block: RecoveredBlock<Block>,
        message: BidTrace,
        _registered_gas_limit: u64,
        apply_blacklist: bool,
        inclusion_list: Option<InclusionList>,
    ) -> Result<(), ValidationApiError> {
        self.validate_message_against_header(block.sealed_header(), &message)?;

        self.consensus.validate_header(block.sealed_header())?;
        self.consensus.validate_block_pre_execution(block.sealed_block())?;

        if !self.disallow.is_empty() && apply_blacklist {
            if self.disallow.contains(&block.beneficiary()) {
                return Err(ValidationApiError::Blacklist(block.beneficiary()));
            }
            if self.disallow.contains(&message.proposer_fee_recipient) {
                return Err(ValidationApiError::Blacklist(message.proposer_fee_recipient));
            }
            for (sender, tx) in block.senders_iter().zip(block.body().transactions()) {
                if self.disallow.contains(sender) {
                    return Err(ValidationApiError::Blacklist(*sender));
                }
                if let Some(to) = tx.to() {
                    if self.disallow.contains(&to) {
                        return Err(ValidationApiError::Blacklist(to));
                    }
                }
            }
        }

        let parent_header = self.get_parent_header(block.parent_hash())?;

        self.consensus.validate_header_against_parent(block.sealed_header(), &parent_header)?;
        let parent_header_hash = parent_header.hash();
        let state_provider = self.provider.state_by_block_hash(parent_header_hash)?;

        let mut request_cache = self.cached_reads(parent_header_hash).await;

        let cached_db = request_cache.as_db_mut(StateProviderDatabase::new(&state_provider));

        let mut executor = self.evm_config.batch_executor(cached_db);

        let mut accessed_blacklisted = None;

        let result = executor.execute_one(&block)?;

        let state = executor.into_state();

        if !self.disallow.is_empty() && apply_blacklist {
            // Check whether the submission interacted with any blacklisted account by scanning
            // the `State`'s cache that records everything read form database during execution.
            for account in state.cache.accounts.keys() {
                if self.disallow.contains(account) {
                    accessed_blacklisted = Some(*account);
                }
            }
        }

        if let Some(account) = accessed_blacklisted {
            return Err(ValidationApiError::Blacklist(account));
        }

        let output = BlockExecutionOutput { state: state.bundle_state.clone(), result };

        // Validate inclusion list constraint if provided
        if let Some(inclusion_list) = inclusion_list {
            self.validate_inclusion_list_constraint(&block, state, &inclusion_list)?;
        }

        // update the cached reads
        self.update_cached_reads(parent_header_hash, request_cache).await;

        self.consensus.validate_block_post_execution(&block, &output)?;

        self.ensure_payment(&block, &output, &message)?;

        let state_root =
            state_provider.state_root(state_provider.hashed_post_state(&output.state))?;

        if state_root != block.header().state_root() {
            return Err(ConsensusError::BodyStateRootDiff(
                GotExpected { got: state_root, expected: block.header().state_root() }.into(),
            )
            .into());
        }

        Ok(())
    }

    fn validate_inclusion_list_constraint<DB>(
        &self,
        block: &RecoveredBlock<Block>,
        post_state: State<DB>,
        inclusion_list: &InclusionList,
    ) -> Result<(), ValidationApiError>
    where
        DB: Database + Debug,
        <DB as revm::Database>::Error: Send + Sync + 'static,
    {
        // nothing to do if no inclusion‐list entries
        if inclusion_list.txs.is_empty() {
            return Ok(());
        }

        // collect which inclusion‐list hashes appeared in the block
        let mut included_hashes = HashSet::new();
        for tx in block.body().transactions() {
            if let Some(req) =
                inclusion_list.txs.iter().find(|t| t.hash.as_slice() == tx.tx_hash().as_slice())
            {
                included_hashes.insert(req.hash);
            }
        }

        // if all requested txs are already in the block, we’re done
        if included_hashes.len() == inclusion_list.txs.len() {
            return Ok(());
        }

        // set up a fresh EVM on top of a cache wrapping the post-block state
        let mut evm = self
            .evm_config
            .evm_for_block(post_state, block.header())
            .map_err(|_| ValidationApiError::InclusionList)?;

        // simulate each missing inclusion‐list tx
        for req in &inclusion_list.txs {
            // skip the ones that actually made it in
            if included_hashes.contains(&req.hash) {
                continue;
            }

            // RLP-decode the raw bytes
            let bytes_slice = req.bytes.as_ref();
            let transaction: reth_primitives::Recovered<TransactionSigned> =
                recover_raw_transaction(bytes_slice)
                    .map_err(|_| ValidationApiError::InclusionList)?;

            // execute the tx
            let outcome = evm.transact(transaction);

            // f it succeeded, then this tx *could* have been included but wasn’t → reject
            if outcome.is_ok() {
                return Err(ValidationApiError::InclusionList);
            }
            // otherwise it failed as expected; keep going
        }

        // every missing tx failed in simulation, so constraint is satisfied
        Ok(())
    }

    /// Ensures that fields of [`BidTrace`] match the fields of the [`SealedHeaderFor`].
    fn validate_message_against_header(
        &self,
        header: &SealedHeaderFor<EthPrimitives>,
        message: &BidTrace,
    ) -> Result<(), ValidationApiError> {
        if header.hash() != message.block_hash {
            tracing::error!("Block hash mismatch: {message:?}, {header:?}");
            Err(ValidationApiError::BlockHashMismatch(GotExpected {
                got: message.block_hash,
                expected: header.hash(),
            }))
        } else if header.parent_hash() != message.parent_hash {
            Err(ValidationApiError::ParentHashMismatch(GotExpected {
                got: message.parent_hash,
                expected: header.parent_hash(),
            }))
        } else if header.gas_limit() != message.gas_limit {
            Err(ValidationApiError::GasLimitMismatch(GotExpected {
                got: message.gas_limit,
                expected: header.gas_limit(),
            }))
        } else if header.gas_used() != message.gas_used {
            Err(ValidationApiError::GasUsedMismatch(GotExpected {
                got: message.gas_used,
                expected: header.gas_used(),
            }))
        } else {
            Ok(())
        }
    }

    /// Ensures that the chosen gas limit is the closest possible value for the validator's
    /// registered gas limit.
    ///
    /// Ref: <https://github.com/flashbots/builder/blob/a742641e24df68bc2fc476199b012b0abce40ffe/core/blockchain.go#L2474-L2477>
    fn _validate_gas_limit(
        &self,
        registered_gas_limit: u64,
        parent_header: &SealedHeaderFor<EthPrimitives>,
        header: &SealedHeaderFor<EthPrimitives>,
    ) -> Result<(), ValidationApiError> {
        let max_gas_limit =
            parent_header.gas_limit() + parent_header.gas_limit() / GAS_LIMIT_BOUND_DIVISOR - 1;
        let min_gas_limit =
            parent_header.gas_limit() - parent_header.gas_limit() / GAS_LIMIT_BOUND_DIVISOR + 1;

        let best_gas_limit =
            std::cmp::max(min_gas_limit, std::cmp::min(max_gas_limit, registered_gas_limit));

        if best_gas_limit != header.gas_limit() {
            return Err(ValidationApiError::GasLimitMismatch(GotExpected {
                got: header.gas_limit(),
                expected: best_gas_limit,
            }));
        }

        Ok(())
    }

    /// Ensures that the proposer has received [`BidTrace::value`] for this block.
    ///
    /// Firstly attempts to verify the payment by checking the state changes, otherwise falls back
    /// to checking the latest block transaction.
    fn ensure_payment(
        &self,
        block: &SealedBlock<Block>,
        output: &BlockExecutionOutput<Receipt>,
        message: &BidTrace,
    ) -> Result<(), ValidationApiError> {
        let (mut balance_before, balance_after) = if let Some(acc) =
            output.state.state.get(&message.proposer_fee_recipient)
        {
            let balance_before = acc.original_info.as_ref().map(|i| i.balance).unwrap_or_default();
            let balance_after = acc.info.as_ref().map(|i| i.balance).unwrap_or_default();

            (balance_before, balance_after)
        } else {
            // account might have balance but considering it zero is fine as long as we know
            // that balance have not changed
            (U256::ZERO, U256::ZERO)
        };

        if let Some(withdrawals) = block.body().withdrawals() {
            for withdrawal in withdrawals {
                if withdrawal.address == message.proposer_fee_recipient {
                    balance_before += withdrawal.amount_wei();
                }
            }
        }

        if balance_after >= balance_before + message.value {
            return Ok(());
        }

        let (receipt, tx) = output
            .receipts
            .last()
            .zip(block.body().transactions().last())
            .ok_or(ValidationApiError::ProposerPayment)?;

        if !receipt.status() {
            return Err(ValidationApiError::ProposerPayment);
        }

        if tx.chain_id() != Some(self.evm_config.chain_spec().chain().id()) {
            return Err(ValidationApiError::ProposerPayment);
        }

        if tx.to() != Some(message.proposer_fee_recipient) {
            return Err(ValidationApiError::ProposerPayment);
        }

        if tx.value() != message.value {
            return Err(ValidationApiError::ProposerPayment);
        }

        if !tx.input().is_empty() {
            return Err(ValidationApiError::ProposerPayment);
        }

        if let Some(block_base_fee) = block.header().base_fee_per_gas() {
            if tx.effective_tip_per_gas(block_base_fee).unwrap_or_default() != 0 {
                return Err(ValidationApiError::ProposerPayment);
            }
        }

        Ok(())
    }

    /// Validates the given [`BlobsBundleV1`] and returns versioned hashes for blobs.
    pub fn validate_blobs_bundle(
        &self,
        mut blobs_bundle: BlobsBundleV1,
    ) -> Result<Vec<B256>, ValidationApiError> {
        if blobs_bundle.commitments.len() != blobs_bundle.proofs.len() ||
            blobs_bundle.commitments.len() != blobs_bundle.blobs.len()
        {
            return Err(ValidationApiError::InvalidBlobsBundle);
        }

        let versioned_hashes = blobs_bundle
            .commitments
            .iter()
            .map(|c| kzg_to_versioned_hash(c.as_slice()))
            .collect::<Vec<_>>();

        let sidecar = blobs_bundle.pop_sidecar(blobs_bundle.blobs.len());

        sidecar.validate(&versioned_hashes, EnvKzgSettings::default().get())?;

        Ok(versioned_hashes)
    }

    /// Validates the given [`BlobsBundleV1`] and returns versioned hashes for blobs.
    pub fn validate_blobs_bundle_v2(
        &self,
        blobs_bundle: BlobsBundleV2,
    ) -> Result<Vec<B256>, ValidationApiError> {
        let versioned_hashes = blobs_bundle
            .commitments
            .iter()
            .map(|c| kzg_to_versioned_hash(c.as_slice()))
            .collect::<Vec<_>>();

        blobs_bundle
            .try_into_sidecar()
            .map_err(|_| ValidationApiError::InvalidBlobsBundle)?
            .validate(&versioned_hashes, EnvKzgSettings::default().get())?;

        Ok(versioned_hashes)
    }

    /// Core logic for validating the builder submission v4
    async fn _validate_builder_submission_v4(
        &self,
        request: ExtendedValidationRequestV4,
    ) -> Result<(), ValidationApiError> {
        info!(target: "rpc::relay", "Validating builder submission v4 test");
        let block = self.payload_validator.ensure_well_formed_payload(ExecutionData {
            payload: ExecutionPayload::V3(request.base.request.execution_payload),
            sidecar: ExecutionPayloadSidecar::v4(
                CancunPayloadFields {
                    parent_beacon_block_root: request.base.parent_beacon_block_root,
                    versioned_hashes: self
                        .validate_blobs_bundle(request.base.request.blobs_bundle)?,
                },
                PraguePayloadFields {
                    requests: RequestsOrHash::Requests(
                        request.base.request.execution_requests.to_requests(),
                    ),
                },
            ),
        })?;

        self.validate_message_against_block(
            block,
            request.base.request.message,
            request.base.registered_gas_limit,
            request.apply_blacklist,
            request.inclusion_list,
        )
        .await
    }

    /// Core logic for validating the builder submission v5
    async fn _validate_builder_submission_v5(
        &self,
        request: ExtendedValidationRequestV5,
    ) -> Result<(), ValidationApiError> {
        let block = self.payload_validator.ensure_well_formed_payload(ExecutionData {
            payload: ExecutionPayload::V3(request.base.request.execution_payload),
            sidecar: ExecutionPayloadSidecar::v4(
                CancunPayloadFields {
                    parent_beacon_block_root: request.base.parent_beacon_block_root,
                    versioned_hashes: self
                        .validate_blobs_bundle_v2(request.base.request.blobs_bundle)?,
                },
                PraguePayloadFields {
                    requests: RequestsOrHash::Requests(
                        request.base.request.execution_requests.to_requests(),
                    ),
                },
            ),
        })?;

        // Check block size as per EIP-7934 (only applies when Osaka hardfork is active)
        let chain_spec = self.provider.chain_spec();
        if chain_spec.is_osaka_active_at_timestamp(block.timestamp()) &&
            block.rlp_length() > MAX_RLP_BLOCK_SIZE
        {
            return Err(ValidationApiError::Consensus(ConsensusError::BlockTooLarge {
                rlp_length: block.rlp_length(),
                max_rlp_length: MAX_RLP_BLOCK_SIZE,
            }));
        }

        self.validate_message_against_block(
            block,
            request.base.request.message,
            request.base.registered_gas_limit,
            request.apply_blacklist,
            request.inclusion_list,
        )
        .await
    }

    pub(crate) fn get_parent_header(
        &self,
        parent_hash: B256,
    ) -> Result<SealedHeaderFor<EthPrimitives>, GetParentError> {
        let latest_header =
            self.provider.latest_header()?.ok_or_else(|| GetParentError::MissingLatestBlock)?;

        let parent_header = if parent_hash == latest_header.hash() {
            latest_header
        } else {
            // parent is not the latest header so we need to fetch it and ensure it's not too old
            let parent_header = self
                .provider
                .sealed_header_by_hash(parent_hash)?
                .ok_or_else(|| GetParentError::MissingParentBlock)?;

            if latest_header.number().saturating_sub(parent_header.number()) >
                self.validation_window
            {
                return Err(GetParentError::BlockTooOld);
            }
            parent_header
        };
        Ok(parent_header)
    }
}

#[async_trait]
impl BlockSubmissionValidationApiServer for ValidationApi {
    async fn validate_builder_submission_v1(
        &self,
        _request: BuilderBlockValidationRequest,
    ) -> RpcResult<()> {
        warn!(target: "rpc::relay", "Method `relay_validateBuilderSubmissionV1` is not supported");
        Err(internal_rpc_err("unimplemented"))
    }

    async fn validate_builder_submission_v2(
        &self,
        _request: BuilderBlockValidationRequestV2,
    ) -> RpcResult<()> {
        warn!(target: "rpc::relay", "Method `relay_validateBuilderSubmissionV2` is not supported");
        Err(internal_rpc_err("unimplemented"))
    }

    /// Validates a block submitted to the relay
    async fn validate_builder_submission_v3(
        &self,
        _request: BuilderBlockValidationRequestV3,
    ) -> RpcResult<()> {
        warn!(target: "rpc::relay", "Method `relay_validateBuilderSubmissionV3` is not supported");
        Err(internal_rpc_err("unimplemented"))
    }

    /// Validates a block submitted to the relay
    async fn validate_builder_submission_v4(
        &self,
        request: ExtendedValidationRequestV4,
    ) -> RpcResult<()> {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        self.task_spawner.spawn_blocking(Box::pin(async move {
            let result = Self::_validate_builder_submission_v4(&this, request)
                .await
                .map_err(ErrorObject::from);
            let _ = tx.send(result);
        }));

        rx.await.map_err(|_| internal_rpc_err("Internal blocking task error"))?
    }

    /// Validates a block submitted to the relay
    async fn validate_builder_submission_v5(
        &self,
        request: ExtendedValidationRequestV5,
    ) -> RpcResult<()> {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        self.task_spawner.spawn_blocking(Box::pin(async move {
            let result = Self::_validate_builder_submission_v5(&this, request)
                .await
                .map_err(ErrorObject::from);
            let _ = tx.send(result);
        }));

        rx.await.map_err(|_| internal_rpc_err("Internal blocking task error"))?
    }
}

pub struct ValidationApiInner {
    /// The provider that can interact with the chain.
    pub(crate) provider: RethProvider,
    /// Consensus implementation.
    consensus: Arc<RethConsensus>,
    /// Execution payload validator.
    pub(crate) payload_validator: Arc<RethPayloadValidator>,
    /// Block executor factory.
    pub(crate) evm_config: reth_ethereum::evm::EthEvmConfig,
    /// Set of disallowed addresses
    disallow: Arc<DashSet<Address>>,
    /// The maximum block distance - parent to latest - allowed for validation
    validation_window: u64,
    /// Cached state reads to avoid redundant disk I/O across multiple validation attempts
    /// targeting the same state. Stores a tuple of (`block_hash`, `cached_reads`) for the
    /// latest head block state. Uses async `RwLock` to safely handle concurrent validation
    /// requests.
    cached_state: RwLock<(B256, CachedReads)>,
    /// Task spawner for blocking operations
    pub(crate) task_spawner: Box<TaskExecutor>,
    /// Validation metrics
    metrics: ValidationMetrics,
}

/// Configuration for validation API.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidationApiConfig {
    /// Blacklist endpoint.
    pub blacklist_endpoint: String,
    /// The maximum block distance - parent to latest - allowed for validation
    pub validation_window: u64,
}

impl ValidationApiConfig {
    /// Default validation blocks window of 3 blocks
    pub const DEFAULT_VALIDATION_WINDOW: u64 = 3;

    pub fn new(blacklist_endpoint: String) -> Self {
        Self { blacklist_endpoint, validation_window: Self::DEFAULT_VALIDATION_WINDOW }
    }
}

impl Default for ValidationApiConfig {
    fn default() -> Self {
        Self {
            blacklist_endpoint: Default::default(),
            validation_window: Self::DEFAULT_VALIDATION_WINDOW,
        }
    }
}

/// Metrics for the validation endpoint.
#[derive(Metrics)]
#[metrics(scope = "builder.validation")]
pub(crate) struct ValidationMetrics {
    /// The number of entries configured in the builder validation disallow list.
    pub(crate) disallow_size: Gauge,
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtendedValidationRequestV4 {
    #[serde(flatten)]
    pub base: BuilderBlockValidationRequestV4,

    pub inclusion_list: Option<InclusionList>,

    #[serde(default)]
    pub apply_blacklist: bool,
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtendedValidationRequestV5 {
    #[serde(flatten)]
    pub base: BuilderBlockValidationRequestV5,

    pub inclusion_list: Option<InclusionList>,

    #[serde(default)]
    pub apply_blacklist: bool,
}

/// Block validation rpc interface.
#[rpc(server, namespace = "relay")]
pub trait BlockSubmissionValidationApi {
    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV1")]
    async fn validate_builder_submission_v1(
        &self,
        request: BuilderBlockValidationRequest,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV2")]
    async fn validate_builder_submission_v2(
        &self,
        request: BuilderBlockValidationRequestV2,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV3")]
    async fn validate_builder_submission_v3(
        &self,
        request: BuilderBlockValidationRequestV3,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV4")]
    async fn validate_builder_submission_v4(
        &self,
        request: ExtendedValidationRequestV4,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV5")]
    async fn validate_builder_submission_v5(
        &self,
        request: ExtendedValidationRequestV5,
    ) -> jsonrpsee::core::RpcResult<()>;
}
