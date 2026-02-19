use alloy_primitives::B256;
use helix_common::GetPayloadTrace;
use helix_types::{
    BeaconBlockBodyFulu, BeaconBlockFulu, GetPayloadResponse, PayloadAndBlobs, SignedBeaconBlock,
    SignedBeaconBlockFulu, SignedBlindedBeaconBlock, VersionedSignedProposal,
};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::{
    api::proposer::ProposerApiError,
    auctioneer::{
        PayloadBidData,
        bid_adjustor::BidAdjustor,
        context::Context,
        types::{GetPayloadResult, GetPayloadResultData, PayloadEntry, PendingPayload, SlotData},
    },
    gossip::BroadcastPayloadParams,
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_gossip_payload(
        &mut self,
        payload: BroadcastPayloadParams,
        slot_data: &SlotData,
    ) {
        if self.bid_slot != payload.slot {
            warn!(curr =% self.bid_slot, received =% payload.slot, "received gossiped payload for wrong slot");
            return;
        }

        if slot_data.proposer_pubkey() != &payload.proposer_pub_key {
            warn!(curr =% slot_data.proposer_pubkey(), received =% payload.proposer_pub_key, "received gossiped payload for wrong proposer");
            return;
        }

        let block_hash = payload.execution_payload.execution_payload.block_hash;
        let entry = PayloadEntry::new_gossip(payload.execution_payload, payload.bid_data);
        self.payloads.entry(block_hash).or_insert(entry);
    }

    /// If we start broacasting, returns the block hash of the block
    #[must_use]
    pub(super) fn handle_get_payload(
        &mut self,
        local: PayloadAndBlobs,
        blinded: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
        slot_data: &SlotData,
        bid: PayloadBidData,
    ) -> Option<B256> {
        let (res, maybe_block_hash) = match self.get_payload(blinded, local, trace, slot_data) {
            Ok((to_proposer, to_publish, trace)) => {
                let block_hash = to_proposer.data.execution_payload.block_hash;
                (
                    Ok(GetPayloadResultData {
                        to_proposer,
                        to_publish,
                        trace,
                        fork: slot_data.current_fork,
                        bid,
                    }),
                    Some(block_hash),
                )
            }

            Err(err) => (Err(err), None),
        };

        let _ = res_tx.send(res);

        maybe_block_hash
    }

    /// This should be called only on new submissions / gossiped payloads
    #[must_use]
    pub(super) fn maybe_try_unblind(&mut self, slot_data: &SlotData) -> Option<B256> {
        let pending = self.pending_payload.take()?;

        if let Some(local) = self.payloads.get(&pending.block_hash) {
            info!("found payload for pending get_payload");
            let PendingPayload { blinded, res_tx, trace, .. } = pending;
            self.handle_get_payload(
                local.payload_and_blobs(),
                blinded,
                trace,
                res_tx,
                slot_data,
                local.bid_data_ref().to_owned(),
            )
        } else {
            self.pending_payload = Some(pending);
            None
        }
    }

    fn get_payload(
        &self,
        blinded: SignedBlindedBeaconBlock,
        local: PayloadAndBlobs,
        trace: GetPayloadTrace,
        slot_data: &SlotData,
    ) -> Result<(GetPayloadResponse, VersionedSignedProposal, GetPayloadTrace), ProposerApiError>
    {
        self.validate_proposal_coordinate(&blinded, slot_data)?;

        if blinded.fork_name_unchecked() != slot_data.current_fork {
            return Err(ProposerApiError::UnsupportedBeaconChainVersion);
        }

        let (to_proposer, to_publish) = self.unblind(blinded, local, slot_data)?;
        Ok((to_proposer, to_publish, trace))
    }

    fn validate_proposal_coordinate(
        &self,
        blinded: &SignedBlindedBeaconBlock,
        slot_data: &SlotData,
    ) -> Result<(), ProposerApiError> {
        let slot_duty = &slot_data.registration_data;
        let actual_index = blinded.message().proposer_index();
        let expected_index = slot_duty.validator_index;

        if expected_index != actual_index {
            return Err(ProposerApiError::UnexpectedProposerIndex {
                expected: expected_index,
                actual: actual_index,
            });
        }

        if self.bid_slot != slot_duty.slot {
            return Err(ProposerApiError::InternalSlotMismatchesWithSlotDuty {
                internal_slot: self.bid_slot,
                slot_duty_slot: slot_duty.slot,
            });
        }

        if slot_duty.slot != blinded.message().slot() {
            return Err(ProposerApiError::InvalidBlindedBlockSlot {
                internal_slot: slot_duty.slot,
                blinded_block_slot: blinded.message().slot(),
            });
        }

        Ok(())
    }

    pub fn unblind(
        &self,
        blinded: SignedBlindedBeaconBlock,
        local: PayloadAndBlobs,
        slot_data: &SlotData,
    ) -> Result<(GetPayloadResponse, VersionedSignedProposal), ProposerApiError> {
        match blinded {
            SignedBlindedBeaconBlock::Altair(_)
            | SignedBlindedBeaconBlock::Base(_)
            | SignedBlindedBeaconBlock::Bellatrix(_)
            | SignedBlindedBeaconBlock::Capella(_)
            | SignedBlindedBeaconBlock::Deneb(_)
            | SignedBlindedBeaconBlock::Electra(_)
            | SignedBlindedBeaconBlock::Gloas(_) => {
                Err(ProposerApiError::UnsupportedBeaconChainVersion)
            }
            SignedBlindedBeaconBlock::Fulu(blinded_block) => {
                // validate
                // TODO: we should already have the header, as we served it in "get_header"
                // NOTE: not if it comes via gossip, just implement the check manually
                let local_execution_payload_header = local
                    .execution_payload
                    .to_header(None, None)
                    .to_lighthouse_fulu_header()
                    .map_err(ProposerApiError::SszError)?;

                let block = &blinded_block.message;
                let body = &block.body;
                let provided_header = &body.execution_payload.execution_payload_header;

                if &local_execution_payload_header != provided_header {
                    return Err(ProposerApiError::BlindedBlockAndPayloadHeaderMismatch);
                }

                let local_kzg_commitments = &local.blobs_bundle.commitments();

                if !local_kzg_commitments.iter().eq(body.blob_kzg_commitments.iter().map(|p| p.0)) {
                    return Err(ProposerApiError::BlobKzgCommitmentsMismatch);
                }

                // unblind

                let signature = blinded_block.signature.clone();

                if body.blob_kzg_commitments.len() != local.blobs_bundle.blobs().len() {
                    return Err(ProposerApiError::BlindedBlobsBundleLengthMismatch);
                }

                let local_execution_payload = local
                    .execution_payload
                    .to_lighthouse_fulu_payload()
                    .map_err(ProposerApiError::SszError)?;

                let inner = SignedBeaconBlockFulu {
                    message: BeaconBlockFulu {
                        slot: block.slot,
                        proposer_index: block.proposer_index,
                        parent_root: block.parent_root,
                        state_root: block.state_root,
                        body: BeaconBlockBodyFulu {
                            randao_reveal: body.randao_reveal.clone(),
                            eth1_data: body.eth1_data.clone(),
                            graffiti: body.graffiti,
                            proposer_slashings: body.proposer_slashings.clone(),
                            attester_slashings: body.attester_slashings.clone(),
                            attestations: body.attestations.clone(),
                            deposits: body.deposits.clone(),
                            voluntary_exits: body.voluntary_exits.clone(),
                            sync_aggregate: body.sync_aggregate.clone(),
                            execution_payload: local_execution_payload.into(),
                            bls_to_execution_changes: body.bls_to_execution_changes.clone(),
                            blob_kzg_commitments: body.blob_kzg_commitments.clone(),
                            execution_requests: body.execution_requests.clone(),
                        },
                    },
                    signature,
                };

                let signed_block = SignedBeaconBlock::Fulu(inner).into();

                let to_broadcast = VersionedSignedProposal {
                    signed_block,
                    kzg_proofs: local.blobs_bundle.proofs().clone(),
                    blobs: local.blobs_bundle.blobs().clone(),
                };
                let to_proposer = GetPayloadResponse {
                    version: slot_data.current_fork,
                    metadata: Default::default(),
                    data: local,
                };

                Ok((to_proposer, to_broadcast))
            }
        }
    }
}
