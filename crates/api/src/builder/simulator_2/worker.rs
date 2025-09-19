use bytes::Bytes;
use helix_common::{
    bid_submission::BidSubmission, chain_info::ChainInfo, local_cache::LocalCache,
    metadata_provider::MetadataProvider, RelayConfig, SubmissionTrace,
};
use helix_types::{
    BlockMergingData, BlsPublicKeyBytes, DehydratedBidSubmission, SignedBidSubmission,
    SignedBidSubmissionWithMergingData,
};
use http::{HeaderMap, HeaderValue};
use moka::ops::compute::Op;
use tokio::sync::oneshot;
use tracing::{error, warn};

use crate::{
    builder::{
        api::get_mergeable_orders, decoder::SubmissionDecoder, error::BuilderApiError,
        simulator_2::Event,
    },
    proposer::MergingPoolMessage,
    Api, HEADER_API_KEY, HEADER_HYDRATE, HEADER_IS_MERGEABLE, HEADER_SEQUENCE,
};

// TODO: spans
struct Worker<A: Api> {
    rx: crossbeam_channel::Receiver<WorkerJob>,
    tx: crossbeam_channel::Sender<Event>,
    // TODO: move merging to main loop
    merge_pool_tx: tokio::sync::mpsc::Sender<MergingPoolMessage>,
    cache: LocalCache,
    chain_info: ChainInfo,
    metadata_provider: A::MetadataProvider,
    relay_config: RelayConfig,
}

impl<A: Api> Worker<A> {
    fn run(mut self) {
        loop {
            let Ok(task) = self.rx.try_recv() else {
                continue;
            };

            match task {
                WorkerJob::BlockSubmission { headers, body, mut trace, res_tx } => {
                    match self.handle_block_submission(headers, body, &mut trace) {
                        Ok((submission, sequence, merging_data)) => {
                            let evt = Event::Submission {
                                submission: submission.clone(),
                                sequence,
                                trace,
                                res_tx,
                            };

                            let _ = self.tx.try_send(evt);

                            if self.relay_config.block_merging_config.is_enabled {
                                if let Some(merging_data) = merging_data {
                                    let Submission::Full(payload) = submission else {
                                        continue;
                                    };

                                    let mergeable_orders =
                                        get_mergeable_orders(&payload, merging_data)
                                            .inspect_err(
                                                |e| warn!(%e, "failed to get mergeable orders"),
                                            )
                                            .ok();

                                    if mergeable_orders
                                        .as_ref()
                                        .is_some_and(|o| !o.orders.is_empty())
                                    {
                                        let orders = mergeable_orders.unwrap();
                                        let message = MergingPoolMessage::new(&payload, orders);
                                        // We only log the error if this fails
                                        let _ = self.merge_pool_tx.try_send(message).inspect_err(|err| {
                                        error!(?err, "failed to send mergeable orders to merging pool");
                                    });
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            let _ = res_tx.send(Err(err));
                        }
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
        trace: &mut SubmissionTrace,
    ) -> Result<(Submission, Option<u64>, Option<BlockMergingData>), BuilderApiError> {
        trace.metadata = self.metadata_provider.get_metadata(&headers);
        let mut decoder = SubmissionDecoder::from_headers(&headers);
        let body = decoder.decompress(body)?;
        let builder_pubkey = decoder.extract_builder_pubkey(body.as_ref())?;

        let should_hydrate = headers.get(HEADER_HYDRATE).is_some();
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
                (decoder.decode(body)?, None)
            };

            if !skip_sigverify {
                payload.verify_signature(self.chain_info.builder_domain)?;
            }

            payload.validate_payload_ssz_lengths()?;

            (Submission::Full(payload), merging_data)
        };

        Ok((submission, sequence, merging_data))
    }
}

pub type SubmissionResult = Result<(), BuilderApiError>;

#[derive(Clone)]
pub enum Submission {
    // received after sigverify
    Full(SignedBidSubmission),
    // need to validate do the validate_payload_ssz_lengths
    Dehydrated(DehydratedBidSubmission),
}

impl Submission {
    pub fn bid_slot(&self) -> u64 {
        match self {
            Submission::Full(s) => s.slot().as_u64(),
            Submission::Dehydrated(s) => s.slot(),
        }
    }
}

pub enum WorkerJob {
    BlockSubmission {
        headers: http::HeaderMap,
        body: bytes::Bytes,
        trace: SubmissionTrace, // TODO: replace this with better tracing
        res_tx: oneshot::Sender<SubmissionResult>,
    },
}
