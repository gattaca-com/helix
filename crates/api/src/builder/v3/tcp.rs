use std::{io::Error, net::SocketAddr, sync::Arc};

use axum::response::IntoResponse;
use helix_common::{
    bid_submission::v3::header_submission_v3::{
        HeaderSubmissionV3, MessageHeader, MessageHeaderFlags, MessageType, SubmissionV3Error,
    },
    utils::utcnow_ns,
    HeaderSubmissionTrace,
};
use ssz::{Decode, Encode};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::{error, info};

use super::V3Error;
use crate::{builder::api::BuilderApi, Api};

pub async fn run_api<A: Api>(
    listening_port: u16,
    builder_api: Arc<BuilderApi<A>>,
) -> Result<(), Error> {
    let tcp_listener = TcpListener::bind(format!("0.0.0.0:{listening_port}")).await?;
    while let Ok((socket, remote_addr)) = tcp_listener.accept().await {
        tokio::spawn(handle_builder_connection(socket, remote_addr, builder_api.clone()));
    }
    error!("Builder API TCP listener has exited");
    Err(Error::other("TCP API listener exited"))
}

async fn handle_builder_connection<A: Api>(
    mut stream: TcpStream,
    remote_addr: SocketAddr,
    builder_api: Arc<BuilderApi<A>>,
) {
    info!(?remote_addr, "builder connection connected");

    let mut header_buffer = [0u8; size_of::<MessageHeader>()];
    let mut payload_buffer = Vec::with_capacity(32 * 1024);

    while (stream.read_exact(&mut header_buffer).await).is_ok() {
        let msg_header: &MessageHeader = header_buffer.as_slice().into();

        if msg_header.message_type == MessageType::HeaderSubmission {
            let receive = utcnow_ns();
            let mut trace = HeaderSubmissionTrace { receive, ..Default::default() };

            payload_buffer.resize(msg_header.message_length as usize, 0);
            let Ok(_) = stream.read_exact(payload_buffer.as_mut_slice()).await else {
                error!(?msg_header, "tcp stream read failed for header submission");
                break;
            };

            match decode_message(msg_header, payload_buffer.as_slice()) {
                Ok(header) => {
                    trace.decode = utcnow_ns();
                    match BuilderApi::handle_submit_header(
                        &builder_api,
                        header.submission,
                        header.url,
                        header.tx_count,
                        msg_header.message_flags.contains(MessageHeaderFlags::CANCELLATION_ENABLED),
                        trace,
                        false,
                    )
                    .await
                    {
                        Ok(_) => {
                            let ack = MessageHeader {
                                message_type: MessageType::HeaderSubmissionAck,
                                padding: 0,
                                message_length: 0,
                                sequence_number: msg_header.sequence_number,
                                message_flags: MessageHeaderFlags::empty(),
                            };
                            let _ = stream.write_all(ack.as_ref()).await;
                        }
                        Err(e) => {
                            error!(error=?e, "v3 header submission failed");
                            let err_code = e.into_response().status().as_u16();
                            if let Ok(encoded) = encode_error(msg_header, err_code) {
                                let err = MessageHeader {
                                    message_type: MessageType::Error,
                                    padding: 0,
                                    message_length: encoded.len() as u32,
                                    sequence_number: msg_header.sequence_number,
                                    message_flags: msg_header.message_flags,
                                };
                                if (stream.write_all(err.as_ref()).await).is_ok() {
                                    let _ = stream.write_all(encoded.as_slice()).await;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    trace.decode = utcnow_ns();
                    error!(error=?e, ?msg_header, "Failed to decode payload for header submission");
                    break;
                }
            }
        }
    }
    info!(?remote_addr, "builder connection disconnected");
}

fn decode_message(header: &MessageHeader, payload: &[u8]) -> Result<HeaderSubmissionV3, V3Error> {
    if header.message_flags.contains(MessageHeaderFlags::CBOR_ENCODED) {
        cbor4ii::serde::from_slice(payload).map_err(V3Error::Cbor)
    } else if header.message_flags.contains(MessageHeaderFlags::SSZ_ENCODED) {
        HeaderSubmissionV3::from_ssz_bytes(payload).map_err(V3Error::Ssz)
    } else if header.message_flags.contains(MessageHeaderFlags::JSON_ENCODED) {
        serde_json::from_slice(payload).map_err(V3Error::Json)
    } else {
        Err(V3Error::Unknown(*header))
    }
}

fn encode_error(header: &MessageHeader, status: u16) -> Result<Vec<u8>, Error> {
    let error = SubmissionV3Error { status };
    if header.message_flags.contains(MessageHeaderFlags::CBOR_ENCODED) {
        let vec = vec![];
        cbor4ii::serde::to_vec(vec, &error).map_err(Error::other)
    } else if header.message_flags.contains(MessageHeaderFlags::SSZ_ENCODED) {
        Ok(error.as_ssz_bytes())
    } else if header.message_flags.contains(MessageHeaderFlags::JSON_ENCODED) {
        serde_json::to_vec(&error).map_err(Error::other)
    } else {
        Err(Error::other(format!("Unknown message encoding: {header:?}")))
    }
}
