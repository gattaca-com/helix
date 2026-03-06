use std::{ops::Range, sync::Arc};

use helix_common::{GetPayloadTrace, api::proposer_api::GetHeaderParams, chain_info::ChainInfo};
use helix_types::{
    BlsPublicKey, BlsPublicKeyBytes, ExecPayload, SigError, SignedBlindedBeaconBlock,
    SignedValidatorRegistration,
};
use tokio::sync::oneshot;
use tracing::trace;

use crate::{
    api::proposer::ProposerApiError,
    auctioneer::types::{Event, GetHeaderResult, GetPayloadResult, RegWorkerJob},
    gossip::BroadcastPayloadParams,
};

#[derive(Clone)]
pub struct AuctioneerHandle {
    auctioneer: crossbeam_channel::Sender<Event>,
}

impl AuctioneerHandle {
    pub fn new(auctioneer: crossbeam_channel::Sender<Event>) -> Self {
        Self { auctioneer }
    }

    pub fn get_header(
        &self,
        params: GetHeaderParams,
    ) -> Result<oneshot::Receiver<GetHeaderResult>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        trace!("sending to auctioneer");
        self.auctioneer
            .try_send(Event::GetHeader { params, res_tx: tx, span: tracing::Span::current() })
            .map_err(|_| ChannelFull)?;
        Ok(rx)
    }

    pub fn get_payload(
        &self,
        chain_info: &ChainInfo,
        proposer_pubkey: BlsPublicKeyBytes,
        blinded_block: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
    ) -> Result<oneshot::Receiver<GetPayloadResult>, ChannelFull> {
        let (res_tx, rx) = oneshot::channel();
        let blinded_block_hash: Result<_, ProposerApiError> = (|| {
            verify_signed_blinded_block_signature(chain_info, &blinded_block, &proposer_pubkey)?;
            blinded_block
                .message()
                .body()
                .execution_payload()
                .map_err(|_| ProposerApiError::InvalidFork)
                .map(|p| p.block_hash().0)
        })();
        match blinded_block_hash {
            Ok(block_hash) => {
                tracing::trace!("sending to auctioneer");
                if self
                    .auctioneer
                    .try_send(Event::GetPayload {
                        block_hash,
                        blinded: Box::new(blinded_block),
                        trace,
                        res_tx,
                        span: tracing::Span::current(),
                    })
                    .is_err()
                {
                    tracing::error!("failed to send get_payload to auctioneer");
                    return Err(ChannelFull)
                }
            }
            Err(err) => {
                let _ = res_tx.send(Err(err));
            }
        }
        Ok(rx)
    }

    pub fn gossip_payload(&self, req: BroadcastPayloadParams) -> Result<(), ChannelFull> {
        trace!("sending to worker");
        self.auctioneer.try_send(Event::GossipPayload(req)).map_err(|_| ChannelFull)
    }
}

pub struct ChannelFull;

#[derive(Clone)]
pub struct RegWorkerHandle {
    worker: crossbeam_channel::Sender<RegWorkerJob>,
}

impl RegWorkerHandle {
    pub fn new(worker: crossbeam_channel::Sender<RegWorkerJob>) -> Self {
        Self { worker }
    }

    pub fn send(
        &self,
        regs: Arc<Vec<SignedValidatorRegistration>>,
        range: Range<usize>,
    ) -> Result<oneshot::Receiver<Vec<(usize, bool)>>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        self.worker.try_send(RegWorkerJob { regs, range, res_tx: tx }).map_err(|_| ChannelFull)?;

        Ok(rx)
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
    let fork = chain_info.spec.fork_at_epoch(epoch);

    let valid = signed_blinded_beacon_block.verify_signature(
        None,
        &uncompressed_public_key,
        &fork,
        chain_info.genesis_validators_root,
        &chain_info.spec,
    );

    if !valid {
        return Err(SigError::InvalidBlsSignature);
    }

    Ok(())
}
