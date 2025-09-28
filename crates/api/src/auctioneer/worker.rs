use alloy_primitives::B256;
use helix_common::{
    chain_info::ChainInfo, local_cache::LocalCache, GetPayloadTrace, RelayConfig, SubmissionTrace,
};
use helix_types::{
    BlockMergingData, BlockMergingPreferences, BlsPublicKey, BlsPublicKeyBytes,
    DehydratedBidSubmission, ExecPayload, SigError, SignedBidSubmission,
    SignedBidSubmissionWithMergingData, SignedBlindedBeaconBlock,
};
use http::HeaderValue;
use tracing::{error, warn};

use crate::{
    auctioneer::{
        decoder::SubmissionDecoder,
        types::{Submission, WorkerJob},
        Event,
    },
    builder::{api::get_mergeable_orders, error::BuilderApiError},
    proposer::{MergingPoolMessage, ProposerApiError},
    HEADER_API_KEY, HEADER_HYDRATE, HEADER_IS_MERGEABLE, HEADER_SEQUENCE,
};

// TODO: spans
pub(super) struct Worker {
    pub(super) rx: crossbeam_channel::Receiver<WorkerJob>,
    pub(super) tx: crossbeam_channel::Sender<Event>,
    // TODO: move this to auctioneer
    pub(super) merge_pool_tx: tokio::sync::mpsc::Sender<MergingPoolMessage>,
    pub(super) cache: LocalCache,
    pub(super) chain_info: ChainInfo,
    pub(super) config: RelayConfig,
}

impl Worker {
    pub(super) fn run(self) {
        loop {
            let Ok(task) = self.rx.try_recv() else {
                continue;
            };

            self.handle_task(task);
        }
    }

    fn handle_task(&self, task: WorkerJob) {
        match task {
            WorkerJob::BlockSubmission { headers, body, mut trace, res_tx } => {
                match self.handle_block_submission(headers, body, &mut trace) {
                    Ok((submission, withdrawals_root, sequence, merging_data)) => {
                        let merging_preferences = merging_data
                            .as_ref()
                            .map(|m| BlockMergingPreferences { allow_appending: m.allow_appending })
                            .unwrap_or_default();

                        let message = Event::Submission {
                            // TODO: move this to auctioneer, avoid clones
                            submission: submission.clone(),
                            merging_preferences,
                            withdrawals_root,
                            sequence,
                            trace,
                            res_tx,
                        };

                        if self.tx.try_send(message).is_err() {
                            error!("failed sending submisison to auctioneer");
                        }

                        if self.config.block_merging_config.is_enabled {
                            if let Some(merging_data) = merging_data {
                                let Submission::Full(payload) = submission else {
                                    return;
                                };

                                let mergeable_orders =
                                    match get_mergeable_orders(&payload, merging_data) {
                                        Ok(orders) => orders,
                                        Err(err) => {
                                            warn!(%err, "failed to get mergeable orders");
                                            return;
                                        }
                                    };

                                if mergeable_orders.orders.is_empty() {
                                    return;
                                }

                                let message = MergingPoolMessage::new(&payload, mergeable_orders);
                                if let Err(err) = self.merge_pool_tx.try_send(message) {
                                    error!(?err, "failed to send mergeable orders to merging pool");
                                };
                            }
                        }
                    }

                    Err(err) => {
                        let _ = res_tx.send(Err(err));
                    }
                }
            }

            WorkerJob::GetPayload { blinded_block, proposer_pubkey, mut trace, res_tx } => {
                match self.handle_get_payload(&proposer_pubkey, blinded_block, &mut trace) {
                    Ok((blinded, block_hash)) => {
                        let _ = self.tx.try_send(Event::GetPayload {
                            block_hash,
                            blinded,
                            trace,
                            res_tx,
                        });
                    }
                    Err(err) => {
                        let _ = res_tx.send(Err(err));
                    }
                }
            }
        }
    }

    // TODO: populate trace
    fn handle_block_submission(
        &self,
        headers: http::HeaderMap,
        body: bytes::Bytes,
        _trace: &mut SubmissionTrace,
    ) -> Result<(Submission, B256, Option<u64>, Option<BlockMergingData>), BuilderApiError> {
        let mut decoder = SubmissionDecoder::from_headers(&headers);
        let body = decoder.decompress(body)?;
        let builder_pubkey = decoder.extract_builder_pubkey(body.as_ref())?;

        let skip_sigverify = headers
            .get(HEADER_API_KEY)
            .is_some_and(|key| self.cache.validate_api_key(key, &builder_pubkey));
        let should_hydrate = headers.get(HEADER_HYDRATE).is_some();
        let has_mergeable_data = matches!(headers.get(HEADER_IS_MERGEABLE), Some(header) if header == HeaderValue::from_static("true"));
        let sequence = headers
            .get(HEADER_SEQUENCE)
            .and_then(|seq| seq.to_str().ok())
            .and_then(|seq| seq.parse::<u64>().ok());

        let (submission, merging_data) = if should_hydrate {
            // caches are per builder and the builder pubkey is still unvalidated so we rely on the
            // api key pubkey for safety
            if !skip_sigverify {
                return Err(BuilderApiError::UntrustedBuilderOnDehydratedPayload);
            }

            let payload: DehydratedBidSubmission = decoder.decode(body)?;
            (Submission::Dehydrated(payload), None)
        } else {
            let (payload, merging_data) = if has_mergeable_data {
                let payload: SignedBidSubmissionWithMergingData = decoder.decode(body)?;
                (payload.submission, Some(payload.merging_data))
            } else {
                let payload: SignedBidSubmission = decoder.decode(body)?;
                (payload, None)
            };

            if !skip_sigverify {
                payload.verify_signature(self.chain_info.builder_domain)?;
            }

            payload.validate_payload_ssz_lengths()?;
            (Submission::Full(payload), merging_data)
        };

        let withdrawals_root = submission.withdrawal_root();

        Ok((submission, withdrawals_root, sequence, merging_data))
    }

    fn handle_get_payload(
        &self,
        proposer_pubkey: &BlsPublicKeyBytes,
        blinded_block: SignedBlindedBeaconBlock,
        _trace: &mut GetPayloadTrace,
    ) -> Result<(SignedBlindedBeaconBlock, B256), ProposerApiError> {
        verify_signed_blinded_block_signature(&self.chain_info, &blinded_block, proposer_pubkey)?;

        let block_hash = blinded_block
            .message()
            .body()
            .execution_payload()
            .map_err(|_| ProposerApiError::InvalidFork)? // this should never happen as post altair there's always an execution payload
            .block_hash()
            .0;

        Ok((blinded_block, block_hash))
    }
}

fn verify_signed_blinded_block_signature(
    chain_info: &ChainInfo,
    signed_blinded_beacon_block: &SignedBlindedBeaconBlock,
    public_key: &BlsPublicKeyBytes,
) -> Result<(), SigError> {
    let uncompressed_public_key = BlsPublicKey::deserialize(public_key.as_slice())
        .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
    let slot = signed_blinded_beacon_block.message().slot();
    let epoch = slot.epoch(chain_info.slots_per_epoch());
    let fork = chain_info.context.fork_at_epoch(epoch);

    let valid = signed_blinded_beacon_block.verify_signature(
        None,
        &uncompressed_public_key,
        &fork,
        chain_info.genesis_validators_root,
        &chain_info.context,
    );

    if !valid {
        return Err(SigError::InvalidBlsSignature);
    }

    Ok(())
}
