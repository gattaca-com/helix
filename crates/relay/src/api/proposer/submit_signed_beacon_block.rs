use std::sync::Arc;

use alloy_primitives::B256;
use axum::{Extension, http::HeaderMap};
use helix_common::{
    chain_info::ChainInfo, decoder::Encoding, spawn_tracked, utils::extract_request_id,
};
use helix_types::{
    BlsPublicKeyBytes, Domain, ForkName, Slot,
    gloas::{self, GloasBlock},
    ssz_json,
};
use hyper::StatusCode;
use tracing::{info, warn};

use super::{ProposerApi, get_payload::fork_name_from_header};
use crate::{
    api::{Api, proposer::error::ProposerApiError},
    auctioneer::{Event, SeenBlock},
};

/// Whether `pubkey`, the slot's scheduled proposer, signed this block. The bid's block
/// hash is public once the block is gossiped, so without this anyone could post a block
/// naming our bid and, with the equivocation guard, make helix withhold a legitimate reveal.
fn proposer_signed_block(
    block: &GloasBlock<'_>,
    block_root: B256,
    pubkey: &BlsPublicKeyBytes,
    chain_info: &ChainInfo,
) -> bool {
    let domain = chain_info.domain_at(Domain::BeaconProposer, Slot::new(block.slot()));
    gloas::bls_verify(pubkey.as_slice(), block.signature(), gloas::signing_root(block_root, domain))
        .is_ok()
}

impl<A: Api> ProposerApi<A> {
    /// Accepts a Gloas `SignedBeaconBlock`. Replaces `submitBlindedBlock`/`getPayload`; per
    /// <https://github.com/ethereum/builder-specs/blob/main/specs/gloas/validator.md#block-proposal>,
    /// Gloas has no blinded-block variant. This is the block the proposer gossips, sent to us
    /// as a head start: revealing our payload is our own duty, so its outcome is not
    /// reported back.
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers)))]
    pub async fn submit_signed_beacon_block(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let fork = fork_name_from_header(&headers).ok().flatten();
        if fork != Some(ForkName::Gloas) {
            return Err(ProposerApiError::InvalidFork);
        }

        let body = match Encoding::from_content_type(&headers) {
            Encoding::Json => ssz_json::to_ssz(&ssz_json::SIGNED_BEACON_BLOCK_GLOAS, &body)
                .map_err(|err| {
                    ProposerApiError::SszDecodeError(ssz::DecodeError::BytesInvalid(
                        err.to_string(),
                    ))
                })?
                .into(),
            Encoding::Ssz => body,
        };
        let block = GloasBlock::new(&body).ok_or(ProposerApiError::SszDecodeError(
            ssz::DecodeError::BytesInvalid("signed beacon block layout".to_string()),
        ))?;
        let slot = Slot::new(block.slot());

        info!(%slot, "accepted submitSignedBeaconBlock request");

        // Our beacon node's head still reaches the auctioneer, so a block we cannot check
        // here is only a lost head start.
        let Some(pubkey) = proposer_api.local_cache.proposer_pubkey(slot) else {
            warn!(%slot, "no scheduled proposer known for the block's slot");
            return Ok(StatusCode::ACCEPTED);
        };
        let block_root = block.block_root();
        if !proposer_signed_block(&block, block_root, &pubkey, &proposer_api.chain_info) {
            return Err(ProposerApiError::InvalidProposerSignature);
        }

        let event = Event::GloasBlock { slot, seen: SeenBlock::new(&block, block_root) };
        if proposer_api.auctioneer_handle.send_event(event).is_err() {
            return Err(ProposerApiError::InternalServerError);
        }

        // The proposer gossips its own block; ours is a head start for our node, so a
        // failure only costs that.
        let beacon_client = proposer_api.multi_beacon_client.clone();
        spawn_tracked!(async move {
            if let Err(err) = beacon_client.publish_gloas_block(body).await {
                warn!(%err, "could not publish the proposer's block");
            }
        });

        Ok(StatusCode::ACCEPTED)
    }
}

#[cfg(test)]
mod tests {
    use helix_common::utils::install_default_crypto_provider;
    use helix_types::{BeaconBlockGloas, BlsKeypair, EmptyBlock, SignedBeaconBlockGloas};
    use ssz::Encode;
    use tree_hash::TreeHash;

    use super::*;

    fn proposer() -> BlsKeypair {
        install_default_crypto_provider();
        BlsKeypair::random()
    }

    fn pubkey(keypair: &BlsKeypair) -> BlsPublicKeyBytes {
        keypair.pk.compress().serialize().into()
    }

    /// Signs a block the way the proposer's validator client would.
    fn signed_block(signer: &BlsKeypair) -> Vec<u8> {
        let chain_info = ChainInfo::default();
        let message = BeaconBlockGloas::empty(&chain_info.spec);
        let domain = chain_info.domain_at(Domain::BeaconProposer, message.slot);
        let signing_root = gloas::signing_root(message.tree_hash_root(), domain);
        SignedBeaconBlockGloas { message, signature: signer.sk.sign(signing_root) }.as_ssz_bytes()
    }

    fn verify(block: &[u8], pubkey: &BlsPublicKeyBytes) -> bool {
        let block = GloasBlock::new(block).expect("a well-formed block");
        proposer_signed_block(&block, block.block_root(), pubkey, &ChainInfo::default())
    }

    #[test]
    fn a_block_signed_by_the_scheduled_proposer_is_accepted() {
        let proposer = proposer();
        assert!(verify(&signed_block(&proposer), &pubkey(&proposer)));
    }

    #[test]
    fn a_block_signed_by_anyone_else_is_rejected() {
        let attacker = proposer();
        let proposer = proposer();
        assert!(!verify(&signed_block(&attacker), &pubkey(&proposer)));
    }

    #[test]
    fn an_unsigned_block_is_rejected() {
        let proposer = proposer();
        let block = SignedBeaconBlockGloas {
            message: BeaconBlockGloas::empty(&ChainInfo::default().spec),
            signature: helix_types::BlsSignature::empty(),
        }
        .as_ssz_bytes();
        assert!(!verify(&block, &pubkey(&proposer)));
    }
}
