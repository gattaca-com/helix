use std::{
    io::{Error, IoSlice},
    net::SocketAddr,
    sync::Arc,
    time::SystemTime,
};

use alloy_primitives::B256;
use helix_common::{
    bid_submission::v3::header_submission_v3::{
        GetPayloadV3, MessageHeader, MessageHeaderFlags, MessageType, PayloadSocketAddress,
    },
    SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::SignedBidSubmission;
use helix_utils::utcnow_ms;
use ssz::Decode;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::Receiver,
};

use super::V3Error;
use crate::{
    builder::{
        api::{get_nanos_from, BuilderApi, MAX_PAYLOAD_LENGTH},
        error::BuilderApiError,
        traits::BlockSimulator,
    },
    gossiper::traits::GossipClientTrait,
};

/// A task that fetches builder blocks for optimistic v3 submissions.
pub async fn fetch_builder_blocks<A, DB, S, G>(
    api: Arc<BuilderApi<A, DB, S, G>>,
    mut receiver: Receiver<(B256, PayloadSocketAddress)>,
) where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    S: BlockSimulator + 'static,
    G: GossipClientTrait + 'static,
{
    let mut header_buffer = [0u8; size_of::<MessageHeader>()];
    let mut payload_buffer = Vec::with_capacity(MAX_PAYLOAD_LENGTH);

    while let Some((block_hash, builder_address)) = receiver.recv().await {
        let receive = get_nanos_from(SystemTime::now()).unwrap_or_default();
        let trace = SubmissionTrace { receive, ..Default::default() };

        match fetch_block(block_hash, &builder_address, &mut header_buffer, &mut payload_buffer)
            .await
        {
            Ok(block) => match BuilderApi::handle_optimistic_payload(api.clone(), block, trace)
                .await
            {
                Ok(_) => tracing::info!(?block_hash, ?builder_address, "v3 block fetch successful"),
                Err(e) => {
                    tracing::error!(error=?e, ?block_hash, ?builder_address, "v3 block submission failed")
                }
            },
            Err(e) => {
                tracing::error!(error=?e, ?block_hash, ?builder_address, "v3 block fetch failed, demoting builder");
                api.demote_builder(
                    builder_address.builder_pubkey(),
                    &block_hash,
                    &BuilderApiError::PayloadError(e),
                )
                .await;
            }
        }
    }
}

async fn fetch_block(
    block_hash: B256,
    payload_address: &PayloadSocketAddress,
    header_buffer: &mut [u8],
    payload_buffer: &mut Vec<u8>,
) -> Result<SignedBidSubmission, V3Error> {
    let request_ts = utcnow_ms();
    let mut connection = connection(payload_address).await?;

    // Create and serialize the request.
    let request = GetPayloadV3 { block_hash, request_ts };
    let request_bytes = cbor4ii::serde::to_vec(vec![], &request).map_err(Error::other)?;

    let header = MessageHeader {
        message_type: MessageType::GetPayloadRequest,
        padding: 0,
        message_length: request_bytes.len() as u32,
        sequence_number: payload_address.seq(),
        message_flags: MessageHeaderFlags::CBOR_ENCODED,
    };

    // Write request.
    let header_bytes = header.as_ref();
    let mut write_len = header_bytes.len() + request_bytes.len();

    let slices = &[IoSlice::new(header_bytes), IoSlice::new(request_bytes.as_slice())];
    while write_len > 0 {
        let wrote = connection.write_vectored(slices).await?;
        write_len -= wrote;
    }

    // Read response header.
    connection.read_exact(header_buffer).await?;
    let rsp_header: &mut MessageHeader = header_buffer.into();

    // Read response payload
    let rsp_length = rsp_header.message_length as usize;
    payload_buffer.resize(rsp_length, 0);
    connection.read_exact(payload_buffer.as_mut_slice()).await?;

    if rsp_header.message_flags.contains(MessageHeaderFlags::CBOR_ENCODED) {
        cbor4ii::serde::from_slice(payload_buffer.as_slice()).map_err(V3Error::Cbor)
    } else if rsp_header.message_flags.contains(MessageHeaderFlags::SSZ_ENCODED) {
        SignedBidSubmission::from_ssz_bytes(payload_buffer.as_slice()).map_err(V3Error::Ssz)
    } else if header.message_flags.contains(MessageHeaderFlags::JSON_ENCODED) {
        serde_json::from_slice(payload_buffer.as_slice()).map_err(V3Error::Json)
    } else {
        Err(V3Error::Unknown(header))
    }
}

async fn connection(builder_address: &PayloadSocketAddress) -> Result<TcpStream, Error> {
    TcpStream::connect(SocketAddr::from(builder_address)).await
}
